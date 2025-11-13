use anyhow::{anyhow, Result};
use esp_idf_sys as sys;
use esp_idf_svc::sys as svc_sys;
use std::time::Duration;

use esp32s3_i2s_mic_idf as fw;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::io::vfs::MountedEventfs;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::netif::{self, EspNetif};
use esp_idf_svc::ipv4;
use esp_idf_svc::wifi::{AsyncWifi, EspWifi, WifiDriver};
use embedded_svc::wifi::{ClientConfiguration, Configuration};
use tokio::runtime::Builder as TokioBuilder;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use core::cell::UnsafeCell;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::accept_async;
use std::net::SocketAddr;
use once_cell::sync::OnceCell;
use tokio::net::{TcpListener, TcpStream};
use futures_util::{StreamExt, SinkExt};
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};

/// Holds all Wi-Fi/network stack objects to keep them alive for the program lifetime.
struct NetworkStack {
    event_loop: EspSystemEventLoop,
    timer: esp_idf_svc::timer::EspTaskTimerService,
    nvs: Option<EspDefaultNvsPartition>,
    async_wifi: AsyncWifi<EspWifi<'static>>,
}

impl Drop for NetworkStack {
    fn drop(&mut self) {
        // Very noisy but helpful for debugging unexpected drops on device
        log::info!("NetworkStack is being dropped! THIS SHOULD NOT HAPPEN while server runs");
    }
}

const FRAME_POOL_CAPACITY: usize = 64;
const FRAME_BACKLOG: usize = 4;

static PCM_SENDER: OnceCell<broadcast::Sender<FrameHandle>> = OnceCell::new();
static FRAME_COUNT: AtomicUsize = AtomicUsize::new(0);
static PDM_STARTED: AtomicBool = AtomicBool::new(false);
static ACTIVE_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

struct FrameSlot {
    data: UnsafeCell<[u8; fw::FRAME_BYTES]>,
    len: AtomicUsize,
    refs: AtomicUsize,
}

unsafe impl Sync for FrameSlot {}

impl FrameSlot {
    const fn new() -> Self {
        Self {
            data: UnsafeCell::new([0; fw::FRAME_BYTES]),
            len: AtomicUsize::new(0),
            refs: AtomicUsize::new(0),
        }
    }

    fn as_slice(&self) -> &[u8] {
        let len = self.len.load(Ordering::Acquire);
        unsafe { core::slice::from_raw_parts(self.data.get() as *const u8, len) }
    }

    fn as_mut_slice(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.data.get() as *mut u8, fw::FRAME_BYTES) }
    }
}

const fn init_slots() -> [FrameSlot; FRAME_POOL_CAPACITY] {
    const SLOT: FrameSlot = FrameSlot::new();
    [SLOT; FRAME_POOL_CAPACITY]
}

static FRAME_SLOTS: [FrameSlot; FRAME_POOL_CAPACITY] = init_slots();

fn acquire_slot(len: usize) -> Option<usize> {
    if len > fw::FRAME_BYTES {
        return None;
    }
    for idx in 0..FRAME_POOL_CAPACITY {
        let slot = &FRAME_SLOTS[idx];
        if slot
            .refs
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            slot.len.store(0, Ordering::Release);
            return Some(idx);
        }
    }
    None
}

fn retain_slot(index: usize) {
    FRAME_SLOTS[index].refs.fetch_add(1, Ordering::AcqRel);
}

fn release_slot(index: usize) {
    let slot = &FRAME_SLOTS[index];
    let prev = slot.refs.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        slot.len.store(0, Ordering::Release);
    }
}

#[derive(Debug)]
struct FrameHandle {
    index: usize,
}

impl Clone for FrameHandle {
    fn clone(&self) -> Self {
        retain_slot(self.index);
        Self { index: self.index }
    }
}

impl Drop for FrameHandle {
    fn drop(&mut self) {
        release_slot(self.index);
    }
}

impl FrameHandle {
    fn as_slice(&self) -> &[u8] {
        FRAME_SLOTS[self.index].as_slice()
    }
}

struct SubscriberGuard;

impl SubscriberGuard {
    fn new() -> Self {
        ACTIVE_SUBSCRIBERS.fetch_add(1, Ordering::SeqCst);
        SubscriberGuard
    }
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        ACTIVE_SUBSCRIBERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn main() -> Result<()> {
    // Link patches and init logging similarly to firmware main
    sys::link_patches();
    svc_sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Mount EventFD filesystem before starting Tokio runtime. This is required on
    // ESP-IDF so Tokio's eventfd usage doesn't fail with PermissionDenied.
    let _eventfs = MountedEventfs::mount(1).map_err(|e| anyhow!("Failed to mount EventFD filesystem: {}", e))?;

    // Create pinned FreeRTOS task like the firmware does
    // Start the PDM pipeline but use a custom stream function implemented here
    // Try to read WIFI_* env vars injected at build time. If absent, do nothing and
    // avoid crashing — this helper binary should still run even without credentials.
    let (ssid, pass) = match (option_env!("WIFI_SSID"), option_env!("WIFI_PASS")) {
        (Some(s), Some(p)) => (s.to_string(), p.to_string()),
        _ => {
            // Credentials not provided; skip Wi‑Fi setup.
            // Start PDM pipeline only if it hasn't already been started.
            if !PDM_STARTED.swap(true, Ordering::SeqCst) {
                fw::start_pdm_with_stream(pdm_ws_stream as fw::StreamFn);
            } else {
                log::info!("main: PDM pipeline already started; skipping start in fallback");
            }
            // Keep main minimal; the background task will run
            loop { std::thread::sleep(Duration::from_millis(1000)); }
        }
    };

    // Build a minimal tokio runtime and run the async connect task plus WS server
    // log free heap before creating runtime (helps diagnose OOM on target)
    log::info!("heap before runtime: {}", unsafe { sys::esp_get_free_heap_size() });
    let rt = TokioBuilder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow!("failed to build tokio runtime: {}", e))?;
    log::info!("heap after runtime created: {}", unsafe { sys::esp_get_free_heap_size() });

    // Create a broadcast channel for PCM frames (binary messages). We'll use a OnceCell
    // to make the sender available to the PDM sink which runs in an RTOS task.
    let (tx, _rx_guard) = broadcast::channel::<FrameHandle>(FRAME_BACKLOG);
    PCM_SENDER.set(tx).ok();

    // Spawn a tiny background printer so serial shows whether frames are produced.
    // This is low-risk instrumentation and can be removed later.
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            log::info!("debug: frames produced so far: {}", FRAME_COUNT.load(Ordering::Relaxed));
        }
    });

    // Create and hold NetworkStack for program lifetime. Use the single runtime `rt`
    // (avoid creating a second runtime — that can exhaust RAM on embedded targets).
    log::info!("heap before connect: {}", unsafe { sys::esp_get_free_heap_size() });
    let net_stack = match rt.block_on(connect_and_print_ip_keep_alive(ssid, pass)) {
        Ok(stack) => stack,
        Err(e) => {
            log::error!("WiFi connect failed: {:#?}", e);
            return Err(anyhow!("wifi connect failed: {}", e));
        }
    };
    log::info!("heap after connect returned (stack held): {}", unsafe { sys::esp_get_free_heap_size() });

    let res = rt.block_on(async {
        tokio::try_join!(
            async {
                // Keep alive forever
                let _hold = &net_stack;
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            },
            async {
                // Start websocket server on port 9000
                let addr: SocketAddr = "0.0.0.0:9000".parse().map_err(|e| anyhow!("invalid ws listen address: {}", e))?;
                let listener = TcpListener::bind(addr).await.map_err(|e| anyhow!("failed to bind ws listener: {}", e))?;
                log::info!("Websocket server listening on {}", addr);
                let tx = PCM_SENDER.get().ok_or_else(|| anyhow!("PCM_SENDER not initialized"))?.clone();
                loop {
                    match listener.accept().await {
                        Ok((stream, peer)) => {
                            let tx = tx.clone();
                            // Always try to upgrade to WebSocket first
                            match accept_async(stream).await {
                                Ok(ws_stream) => {
                                    log::info!("Accepted WebSocket connection from {}", peer);
                                    tokio::spawn(handle_ws_connection(ws_stream, peer, tx));
                                }
                                Err(_) => {
                                    log::info!("Accepted HTTP connection from {}", peer);
                                    // Minimal HTTP 200 OK response
                                    // Instead, just close connection (no response)
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("ws accept error: {}", e);
                            break;
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            }
        )
    });

    res.map_err(|e| anyhow!("websocket runtime error: {}", e))?;

    // fw::start_pdm_with_stream(pdm_ws_stream as fw::StreamFn);

    // Keep main minimal; the background task will run
    loop { std::thread::sleep(Duration::from_millis(1000)); }
}

fn make_frame(samples: &[i16]) -> Option<FrameHandle> {
    let len_bytes = samples.len() * core::mem::size_of::<i16>();
    let idx = acquire_slot(len_bytes)?;
    let slot = &FRAME_SLOTS[idx];
    let buf = slot.as_mut_slice();
    const GAIN: f32 = 2.0;
    for (chunk, sample) in buf[..len_bytes].chunks_exact_mut(2).zip(samples.iter()) {
        let scaled = (*sample as f32 * GAIN).round();
        let clamped = if scaled > i16::MAX as f32 {
            i16::MAX
        } else if scaled < i16::MIN as f32 {
            i16::MIN
        } else {
            scaled as i16
        };
        let u = clamped as u16;
        chunk[0] = (u & 0xff) as u8;
        chunk[1] = ((u >> 8) & 0xff) as u8;
    }
    slot.len.store(len_bytes, Ordering::Release);
    Some(FrameHandle { index: idx })
}

// Stream function used by run-websocket. It must be `unsafe extern "C" fn(_,)->!`.
unsafe extern "C" fn pdm_ws_stream(rx_channel: sys::i2s_chan_handle_t) -> ! {
    // TODO: implement forwarding to websocket clients here. For now just call the generic
    // stream_to_sink with a todo closure to keep the signature.
    // Try to publish PCM frames to the broadcast channel as binary messages (s16le).
    fw::stream_to_sink(rx_channel, |samples: &[i16]| {
        if let Some(tx) = PCM_SENDER.get() {
            if ACTIVE_SUBSCRIBERS.load(Ordering::Relaxed) == 0 {
                return samples.len() as isize * core::mem::size_of::<i16>() as isize;
            }
            match make_frame(samples) {
                Some(frame) => {
                    if let Err(e) = tx.send(frame) {
                        log::warn!("broadcast send failed: {}", e);
                    } else {
                        FRAME_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                }
                None => {
                    log::warn!("pcm frame pool exhausted; dropping frame");
                }
            }
            samples.len() as isize * core::mem::size_of::<i16>() as isize
        } else {
            0isize
        }
    })
}

async fn handle_ws_connection(ws_stream: tokio_tungstenite::WebSocketStream<TcpStream>, peer: SocketAddr, tx: broadcast::Sender<FrameHandle>) {
    let mut rx = tx.subscribe();
    let _sub_guard = SubscriberGuard::new();
    if !PDM_STARTED.load(Ordering::SeqCst) {
        if ACTIVE_SUBSCRIBERS.load(Ordering::SeqCst) == 1 {
            log::info!("handle_ws_connection: starting PDM pipeline");
            PDM_STARTED.store(true, Ordering::SeqCst);
            fw::start_pdm_with_stream(pdm_ws_stream as fw::StreamFn);
        }
    }
    let (mut ws_sink, mut ws_stream) = ws_stream.split();

    // mpsc channel for outgoing messages; writer task owns the WebSocket sink
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(16);

    // Writer task: consumes messages from out_rx and sends them on the ws sink
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ws_sink.send(msg).await {
                log::error!("ws writer send error to {}: {}", peer, e);
                break;
            }
        }
    });

    // Spawn a task to forward broadcasted PCM frames to the writer via out_tx
    let forward = {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(frame_handle) => {
                        let payload = frame_handle.as_slice().to_vec();
                        if out_tx.send(Message::Binary(payload)).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        log::warn!(
                            "ws forward: client {} lagged {} frames; continuing with latest data",
                            peer,
                            skipped
                        );
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        })
    };

    // simple read loop to keep the connection open and react to pings/close
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(m) => match m {
                Message::Close(_) => break,
                Message::Ping(p) => {
                    let _ = out_tx.send(Message::Pong(p)).await;
                }
                _ => {}
            },
            Err(e) => {
                log::error!("ws recv error from {}: {}", peer, e);
                break;
            }
        }
    }
    // ensure forward and writer tasks finish
    let _ = forward.await;
    let _ = writer.await;
}

/// Async helper that performs the Wi‑Fi initialization and prints the IP once obtained.
async fn connect_and_print_ip_keep_alive(ssid: String, pass: String) -> Result<NetworkStack, anyhow::Error> {
    // Initialize platform pieces with verbose logging so device output shows the failing step.
    log::info!("connect: taking EspSystemEventLoop...");
    let event_loop = EspSystemEventLoop::take().map_err(|e| anyhow!("connect: EspSystemEventLoop::take failed: {}", e))?;

    log::info!("connect: creating EspTaskTimerService...");
    let timer = esp_idf_svc::timer::EspTaskTimerService::new().map_err(|e| anyhow!("connect: EspTaskTimerService::new failed: {}", e))?;

    log::info!("connect: taking peripherals...");
    let peripherals = esp_idf_hal::prelude::Peripherals::take()
        .map_err(|e| anyhow!("connect: Peripherals::take failed: {}", e))?;

    // NVS partition (optional)
    log::info!("connect: taking NVS partition (optional)...");
    let nvs = EspDefaultNvsPartition::take().ok();

    // Create Wifi driver and async wrapper
    log::info!("connect: creating WifiDriver...");
    let wifi_driver = WifiDriver::new(peripherals.modem, event_loop.clone(), nvs.clone())
        .map_err(|e| anyhow!("connect: WifiDriver::new failed: {}", e))?;
    let ipv4_config = ipv4::ClientConfiguration::DHCP(ipv4::DHCPClientSettings::default());
    let net_if = EspNetif::new_with_conf(&netif::NetifConfiguration {
        ip_configuration: Option::from(ipv4::Configuration::Client(ipv4_config)),
        ..netif::NetifConfiguration::wifi_default_client()
    })?;

    let ap_netif = EspNetif::new(netif::NetifStack::Ap)?;

    log::info!("connect: wrapping EspWifi and AsyncWifi...");
    let esp_wifi = EspWifi::wrap_all(wifi_driver, net_if, ap_netif)
        .map_err(|e| anyhow!("connect: EspWifi::wrap_all failed: {}", e))?;
    let mut async_wifi = AsyncWifi::wrap(esp_wifi, event_loop.clone(), timer.clone())
        .map_err(|e| anyhow!("connect: AsyncWifi::wrap failed: {}", e))?;

    use core::str::FromStr as _;
    let ssid_h: heapless::String::<32> = heapless::String::from_str(&ssid).map_err(|_| anyhow!("SSID too long"))?;
    let pass_h: heapless::String::<64> = heapless::String::from_str(&pass).map_err(|_| anyhow!("Wifi password too long"))?;
    let client_config = ClientConfiguration { ssid: ssid_h, password: pass_h, channel: None, ..Default::default() };

    log::info!("connect: setting wifi configuration...");
    async_wifi.set_configuration(&Configuration::Client(client_config))
        .map_err(|e| anyhow!("connect: set_configuration failed: {}", e))?;

    log::info!("connect: attempting to join SSID '{}'", ssid);
    log::info!("connect: starting wifi...");
    async_wifi.start().await.map_err(|e| anyhow!("connect: wifi start failed: {}", e))?;

    log::info!("connect: connecting wifi (will retry until connected)...");

    // Keep attempting to connect until success. If connect() times out or errors,
    // log and retry with an increasing backoff. We must not return early because
    // returning would drop wifi objects and deinit the stack (observed previously).
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match async_wifi.connect().await {
            Ok(()) => {
                log::info!("connect: wifi connect succeeded on attempt {}", attempt);
                break;
            }
            Err(e) => {
                log::error!("connect: wifi connect attempt {} failed: {}", attempt, e);
                // exponential backoff: use 1s base and cap exponent at 6 -> max backoff ~= 64s
                let base_ms: u64 = 1000;
                let exp = (attempt.saturating_sub(1)).min(6);
                let backoff = std::time::Duration::from_millis(base_ms.saturating_mul(1u64 << exp));
                log::info!("connect: retrying in {:?}...", backoff);
                tokio::time::sleep(backoff).await;
                continue;
            }
        }
    }

    // Wait indefinitely for a valid DHCP IP (non-zero). Poll periodically.
    loop {
        if let Ok(info) = async_wifi.wifi().sta_netif().get_ip_info() {
            if info.ip.octets() != [0, 0, 0, 0] {
                log::info!("set -gx S3_IP {}", info.ip);
                break;
            }
        }
        // wait up to 800ms for link-up or DHCP progress
        let _ = async_wifi.ip_wait_while(|w| w.is_up().map(|s| !s), Some(std::time::Duration::from_millis(800))).await;
    }
    // After Wi‑Fi is configured, start the PDM pipeline task and then return the wifi/netif objects to keep them alive.
    // Optionally skip starting the PDM pipeline at build time to help debug crashes
    // on resource-constrained targets. Set SKIP_PDM=1 at build time to skip.
    if option_env!("SKIP_PDM") == Some("1") {
        log::info!("connect: SKIP_PDM=1; not starting PDM pipeline (debug mode)");
    } else {
        log::info!("connect: deferring PDM start until first WebSocket client");
    }
    Ok(NetworkStack {
        event_loop,
        timer,
        nvs,
        async_wifi,
    })
}
