use anyhow::{anyhow, Context, Result};
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use embedded_websocket as ws;
use embedded_websocket::{WebSocketContext, WebSocketSendMessageType, WebSocketServer};
use heapless::Vec;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::prelude::Peripherals,
    log::EspLogger,
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, EspWifi},
};
use esp_idf_sys as sys;
use httparse;
use log::{error, info, warn};
use std::convert::TryInto;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::thread;
use std::time::Duration;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASS: &str = env!("WIFI_PASS");

const WS_LISTEN_ADDR: &str = "0.0.0.0";
const WS_PORT: u16 = 80;
const WS_PATH: &str = "/audio";

const SAMPLE_RATE: u32 = 16_000;
const DMA_DESC_NUM: u32 = 24;
const DMA_FRAME_NUM: u32 = 512;
const BUFFER_SAMPLES_PER_READ: usize = 512;
const FRAME_BYTES: usize = BUFFER_SAMPLES_PER_READ * core::mem::size_of::<i16>();

type Frame = Vec<u8, FRAME_BYTES>;

// FreeRTOS task configuration (match serial build defaults)
const TASK_STACK_WORDS: u32 = 8192;
const TASK_PRIORITY: u32 = 12;
const TASK_CORE_ID: i32 = 0;

const MIC_CLK_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_42;
const MIC_DIN_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_41;

pub fn run() -> Result<()> {
    sys::link_patches();
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let mut wifi = init_wifi()?;
    connect_wifi(&mut wifi)?;
    let ip_info = wifi
        .wifi()
        .sta_netif()
        .get_ip_info()
        .context("failed to fetch DHCP info")?;
    let ip_string = ip_info.ip.to_string();
    info!("Wi-Fi connected, DHCP info: {:?}", ip_info);
    println!(
        "\n========================================\nset -gx S3_IP {}\n========================================\n",
        ip_string
    );
    if !ip_string.starts_with("192.") {
        warn!(
            "IP {} does not start with 192.*, check router configuration",
            ip_string
        );
    }
    info!("Websocket endpoint: {}", format_endpoint(&ip_string));

    let (tx, rx) = sync_channel::<Frame>(8);
    spawn_audio_task(tx)?;

    let server_thread = thread::Builder::new()
        .name("ws_server".into())
        .stack_size(32 * 1024)
        .spawn(move || {
            if let Err(err) = websocket_server(rx) {
                error!("websocket server terminated: {err:?}");
            }
        })
        .context("failed to spawn websocket server thread")?;

    loop {
        thread::sleep(Duration::from_secs(5));
        if server_thread.is_finished() {
            return Err(anyhow!("websocket server thread exited unexpectedly"));
        }
    }
}

fn init_wifi() -> Result<BlockingWifi<EspWifi<'static>>> {
    let peripherals = Peripherals::take().context("failed to take peripherals")?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take().ok();

    let esp_wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), nvs)
        .context("failed to create Wi-Fi driver")?;
    BlockingWifi::wrap(esp_wifi, sys_loop).context("failed to wrap Wi-Fi driver")
}

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<()> {
    let auth_method = if WIFI_PASS.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    let client_config = ClientConfiguration {
        ssid: WIFI_SSID
            .try_into()
            .map_err(|_| anyhow!("invalid WIFI_SSID"))?,
        bssid: None,
        auth_method,
        password: WIFI_PASS
            .try_into()
            .map_err(|_| anyhow!("invalid WIFI_PASS"))?,
        channel: None,
        ..Default::default()
    };

    wifi.set_configuration(&Configuration::Client(client_config))?;
    wifi.start()?;
    info!("Wi-Fi driver started");
    wifi.connect()?;
    info!("Wi-Fi connected, waiting for interface");
    wifi.wait_netif_up()?;
    Ok(())
}

fn format_endpoint(ip: &str) -> String {
    if WS_PORT == 80 {
        format!("ws://{}{}", ip, WS_PATH)
    } else {
        format!("ws://{}:{}{}", ip, WS_PORT, WS_PATH)
    }
}

fn websocket_server(rx: Receiver<Frame>) -> Result<()> {
    let listener = TcpListener::bind((WS_LISTEN_ADDR, WS_PORT)).context("failed to bind websocket port")?;
    info!(
        "Listening for websocket clients on {}:{}",
        WS_LISTEN_ADDR, WS_PORT
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                info!("Client connected from {}", stream.peer_addr()?);
                if let Err(err) = handle_client(stream, &rx) {
                    warn!("Client session ended: {err:?}");
                }
            }
            Err(err) => warn!("TCP accept failed: {err:?}"),
        }
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, rx: &Receiver<Frame>) -> Result<()> {
    let mut read_buf = [0_u8; 4096];
    let mut read_cursor = 0_usize;

    let (context, path) =
        match read_websocket_upgrade(&mut stream, &mut read_buf, &mut read_cursor)? {
            Some(ctx) => ctx,
            None => return Ok(()), // non-websocket request already handled
        };

    let mut write_buf = [0_u8; 4096];
    let mut websocket = WebSocketServer::new_server();
    let handshake_len = websocket
        .server_accept(&context.sec_websocket_key, None, &mut write_buf)
        .map_err(|err| anyhow!("websocket handshake failed: {err:?}"))?;

    stream
        .write_all(&write_buf[..handshake_len])
        .context("failed to send websocket accept response")?;
    info!("Websocket upgrade completed for path {}", path);

    drain_audio_queue(rx);

    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(frame) => {
                let len = websocket
                    .write(
                        WebSocketSendMessageType::Binary,
                        true,
                        frame.as_slice(),
                        &mut write_buf,
                    )
                    .map_err(|err| anyhow!("failed to encode frame: {err:?}"))?;
                if let Err(err) = stream.write_all(&write_buf[..len]) {
                    return Err(anyhow!("socket write failed: {err:?}"));
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Keep connection alive even if we temporarily drop frames
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("audio producer disconnected"));
            }
        }
    }
}

fn read_websocket_upgrade(
    stream: &mut TcpStream,
    read_buf: &mut [u8],
    read_cursor: &mut usize,
) -> Result<Option<(WebSocketContext, String)>> {
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut request = httparse::Request::new(&mut headers);

        let bytes_read = stream.read(&mut read_buf[*read_cursor..])?;
        if bytes_read == 0 {
            return Err(anyhow!("connection closed before HTTP header complete"));
        }

        match request
            .parse(&read_buf[..*read_cursor + bytes_read])
            .map_err(|err| anyhow!("HTTP parse error: {err:?}"))?
        {
            httparse::Status::Complete(len) => {
                *read_cursor += bytes_read - len;
                let header_iter = request.headers.iter().map(|h| (h.name, h.value));
                match ws::read_http_header(header_iter)
                    .map_err(|err| anyhow!("invalid websocket header: {err:?}"))?
                {
                    Some(context) => {
                        match request.path {
                            Some(path) if path == WS_PATH => {
                                info!("Received websocket upgrade request for {}", path);
                                return Ok(Some((context, path.to_string())));
                            }
                            Some(other) => {
                                warn!("Unexpected websocket path: {}", other);
                                send_simple_response(
                                    stream,
                                    404,
                                    "Not Found",
                                    format!(
                                        "Expected websocket path {}, got {}",
                                        WS_PATH, other
                                    ),
                                )?;
                                return Ok(None);
                            }
                            None => {
                                warn!("Websocket request missing path");
                                send_simple_response(
                                    stream,
                                    400,
                                    "Bad Request",
                                    "Missing HTTP path",
                                )?;
                                return Ok(None);
                            }
                        }
                    }
                    None => {
                        handle_non_websocket_request(stream, request.path)?;
                        return Ok(None);
                    }
                }
            }
            httparse::Status::Partial => {
                *read_cursor += bytes_read;
            }
        }
    }
}

fn handle_non_websocket_request(stream: &mut TcpStream, path: Option<&str>) -> Result<()> {
    match path {
        Some("/") => send_simple_response(
            stream,
            200,
            "OK",
            format!(
                "Websocket audio stream available at ws://<device-ip>{}\n",
                WS_PATH
            ),
        ),
        _ => send_simple_response(stream, 404, "Not Found", "Use ws://<ip>/audio"),
    }
}

fn send_simple_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: impl AsRef<str>,
) -> Result<()> {
    let body = body.as_ref();
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn drain_audio_queue(rx: &Receiver<Frame>) {
    loop {
        match rx.try_recv() {
            Ok(_) => continue,
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
}

fn spawn_audio_task(tx: SyncSender<Frame>) -> Result<()> {
    let ctx = Box::new(AudioContext { tx });
    let ctx_ptr = Box::into_raw(ctx);

    let task_name = b"ws_pdm\0";
    let res = unsafe {
        sys::xTaskCreatePinnedToCore(
            Some(pdm_rx_task),
            task_name.as_ptr(),
            TASK_STACK_WORDS,
            ctx_ptr as *mut core::ffi::c_void,
            TASK_PRIORITY,
            core::ptr::null_mut(),
            TASK_CORE_ID,
        )
    };

    if res != 1 {
        unsafe { drop(Box::from_raw(ctx_ptr)); }
        anyhow::bail!("xTaskCreatePinnedToCore failed with error {}", res);
    }

    Ok(())
}

struct AudioContext {
    tx: SyncSender<Frame>,
}

impl AudioContext {
    fn push_frame(&self, raw: &[u8]) {
        let mut frame = Frame::new();
        if frame.extend_from_slice(raw).is_err() {
            warn!("PCM frame larger than expected, dropping");
            return;
        }

        if let Err(err) = self.tx.try_send(frame) {
            match err {
                std::sync::mpsc::TrySendError::Full(_) => {
                    // Drop when consumer is slow.
                }
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    warn!("Websocket receiver dropped; stopping audio forwarding");
                }
            }
        }
    }
}

unsafe extern "C" fn pdm_rx_task(ctx: *mut core::ffi::c_void) {
    let ctx = &*(ctx as *const AudioContext);

    let rx_channel = create_i2s_rx_channel();
    configure_pdm_rx(rx_channel);

    let ret = sys::i2s_channel_enable(rx_channel);
    if ret != 0 {
        error!("i2s_channel_enable failed: {}", ret);
        sys::vTaskDelete(core::ptr::null_mut());
    }

    stream_pcm(rx_channel, ctx);
}

unsafe fn create_i2s_rx_channel() -> sys::i2s_chan_handle_t {
    let mut rx_channel: sys::i2s_chan_handle_t = core::ptr::null_mut();

    let channel_config: sys::i2s_chan_config_t = sys::i2s_chan_config_t {
        id: 0,
        role: sys::i2s_role_t_I2S_ROLE_MASTER,
        dma_desc_num: DMA_DESC_NUM,
        dma_frame_num: DMA_FRAME_NUM,
        auto_clear_before_cb: true,
        allow_pd: false,
        intr_priority: 0,
        ..core::mem::zeroed()
    };

    let ret = sys::i2s_new_channel(&channel_config, core::ptr::null_mut(), &mut rx_channel);
    if ret != 0 {
        panic!("i2s_new_channel failed: {}", ret);
    }

    rx_channel
}

unsafe fn configure_pdm_rx(rx_channel: sys::i2s_chan_handle_t) {
    let mut pdm_gpio: sys::i2s_pdm_rx_gpio_config_t = core::mem::zeroed();
    pdm_gpio.clk = MIC_CLK_GPIO;
    pdm_gpio.__bindgen_anon_1.din = MIC_DIN_GPIO;

    let pdm_clk: sys::i2s_pdm_rx_clk_config_t = sys::i2s_pdm_rx_clk_config_t {
        sample_rate_hz: SAMPLE_RATE,
        clk_src: sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT,
        mclk_multiple: sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256,
        dn_sample_mode: sys::i2s_pdm_dsr_t_I2S_PDM_DSR_16S,
        bclk_div: 0,
        ..core::mem::zeroed()
    };

    let pdm_slot: sys::i2s_pdm_rx_slot_config_t = sys::i2s_pdm_rx_slot_config_t {
        data_bit_width: sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT,
        slot_bit_width: sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_16BIT,
        slot_mode: sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO,
        slot_mask: sys::i2s_pdm_slot_mask_t_I2S_PDM_SLOT_RIGHT,
        ..core::mem::zeroed()
    };

    let mut pdm_cfg = sys::i2s_pdm_rx_config_t {
        clk_cfg: pdm_clk,
        slot_cfg: pdm_slot,
        gpio_cfg: pdm_gpio,
    };

    let ret = sys::i2s_channel_init_pdm_rx_mode(rx_channel, &mut pdm_cfg);
    if ret != 0 {
        panic!("i2s_channel_init_pdm_rx_mode failed: {}", ret);
    }
}

unsafe fn stream_pcm(rx_channel: sys::i2s_chan_handle_t, ctx: &AudioContext) -> ! {
    static mut PCM_BUF: [i16; BUFFER_SAMPLES_PER_READ] = [0; BUFFER_SAMPLES_PER_READ];

    loop {
        let mut num_bytes_read: usize = 0;
        let ret = sys::i2s_channel_read(
            rx_channel,
            PCM_BUF.as_mut_ptr() as *mut core::ffi::c_void,
            core::mem::size_of::<i16>() * PCM_BUF.len(),
            &mut num_bytes_read as *mut usize,
            u32::MAX,
        );

        if ret != 0 || num_bytes_read == 0 {
            continue;
        }

        let samples = core::slice::from_raw_parts_mut(
            PCM_BUF.as_mut_ptr(),
            num_bytes_read / core::mem::size_of::<i16>(),
        );
        for s in samples.iter_mut() {
            *s = (*s >> 1).clamp(-32760, 32760);
        }

        let raw = core::slice::from_raw_parts(PCM_BUF.as_ptr() as *const u8, num_bytes_read);
        ctx.push_frame(raw);
    }
}
