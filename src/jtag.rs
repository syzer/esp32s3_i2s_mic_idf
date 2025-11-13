use anyhow::Result;
use esp_idf_sys as sys;
use log::{error, info};
use std::time::Duration;

const SAMPLE_RATE: u32 = 16_000;
const DMA_DESC_NUM: u32 = 24;
const DMA_FRAME_NUM: u32 = 512;
const BUFFER_SAMPLES_PER_READ: usize = 512;
const FRAME_BYTES: usize = 640; // 20 ms @ 16 kHz mono s16

// FreeRTOS task configuration
const TASK_STACK_WORDS: u32 = 8192; // stack in 32-bit words
const TASK_PRIORITY: u32 = 12;      // higher than default app tasks
const TASK_CORE_ID: i32 = 0;        // pin to PRO CPU on ESP32-S3

// GPIO mapping for the microphone
const MIC_CLK_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_42;
const MIC_DIN_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_41;

pub fn run() -> Result<()> {
    // Link ESP-IDF patches and ensure runtime is initialized
    // keep esp_idf_svc link too for compatibility with earlier template expectations
    sys::link_patches();
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    // Create pinned FreeRTOS task with higher priority so USB/serial cannot preempt the audio path
    let task_name = b"pdm_rx\0";
    let res = unsafe {
        sys::xTaskCreatePinnedToCore(
            Some(pdm_rx_task),
            task_name.as_ptr(),// as *const i8,
            TASK_STACK_WORDS,
            core::ptr::null_mut(),
            TASK_PRIORITY,
            core::ptr::null_mut(),
            TASK_CORE_ID,
        )
    };
    if res != 1 { // pdPASS
        panic!("xTaskCreatePinnedToCore failed: {}", res);
    }
    // Keep main task minimal to avoid stack overflow; let worker run forever
    loop { std::thread::sleep(Duration::from_millis(1000)); }
}

// High-priority, pinned task that runs the PDM capture and streaming loop
unsafe extern "C" fn pdm_rx_task(_pv: *mut core::ffi::c_void) {
    // Disable logs in hot path to avoid UART stalls

    // Initialize USB Serial JTAG driver before any writes
    let mut usb_cfg: sys::usb_serial_jtag_driver_config_t = unsafe { core::mem::zeroed() };
    usb_cfg.tx_buffer_size = 2048*2;
    usb_cfg.rx_buffer_size = 2048*2;
    let err = unsafe { sys::usb_serial_jtag_driver_install(&mut usb_cfg) };
    // Do not log install errors here; continue best-effort
    let rx_channel = create_i2s_rx_channel();
    configure_pdm_rx(rx_channel);

    let ret = sys::i2s_channel_enable(rx_channel);
    if ret != 0 {
        error!("i2s_channel_enable failed: {}", ret);
        sys::vTaskDelete(core::ptr::null_mut());
    }

    stream_pcm(rx_channel);
}

/// Create and return an enabled I2S RX channel handle.
unsafe fn create_i2s_rx_channel() -> sys::i2s_chan_handle_t {
    let mut rx_channel: sys::i2s_chan_handle_t = core::ptr::null_mut();

    let channel_config: sys::i2s_chan_config_t = sys::i2s_chan_config_t {
        id: 0, // I2S_NUM_0
        role: sys::i2s_role_t_I2S_ROLE_MASTER,
        dma_desc_num: DMA_DESC_NUM,
        dma_frame_num: DMA_FRAME_NUM,
        auto_clear_before_cb: true,
        allow_pd: false,
        intr_priority: 0,
        ..unsafe { core::mem::zeroed() }
    };

    let ret = sys::i2s_new_channel(&channel_config, core::ptr::null_mut(), &mut rx_channel);
    if ret != 0 {
        panic!("i2s_new_channel failed: {}", ret);
    }

    rx_channel
}

/// Configure the created channel for PDM RX using the selected pins and 16-bit mono PCM.
unsafe fn configure_pdm_rx(rx_channel: sys::i2s_chan_handle_t) {
    // GPIO config: use union field for DIN
    let mut pdm_gpio: sys::i2s_pdm_rx_gpio_config_t = core::mem::zeroed();
    pdm_gpio.clk = MIC_CLK_GPIO;
    pdm_gpio.__bindgen_anon_1.din = MIC_DIN_GPIO;

    // Clocking config: 16 kHz target, 128x MCLK, auto BCLK divider
    let pdm_clk: sys::i2s_pdm_rx_clk_config_t = sys::i2s_pdm_rx_clk_config_t {
        sample_rate_hz: SAMPLE_RATE,
        clk_src: sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT,
        mclk_multiple: sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256,
        // dn_sample_mode: sys::i2s_pdm_dsr_t_I2S_PDM_DSR_8S,
        dn_sample_mode: sys::i2s_pdm_dsr_t_I2S_PDM_DSR_16S,
        bclk_div: 0, // auto
        ..unsafe { core::mem::zeroed() }
    };

    let pdm_slot: sys::i2s_pdm_rx_slot_config_t = sys::i2s_pdm_rx_slot_config_t {
        data_bit_width: sys::i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT,
        // slot_bit_width: sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_AUTO,
        slot_bit_width: sys::i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_16BIT, // not AUTO
        slot_mode: sys::i2s_slot_mode_t_I2S_SLOT_MODE_MONO,
        // slot_mask: sys::i2s_pdm_slot_mask_t_I2S_PDM_SLOT_LEFT,
        slot_mask: sys::i2s_pdm_slot_mask_t_I2S_PDM_SLOT_RIGHT,
        ..unsafe { core::mem::zeroed() }
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

/// Continuously read PCM frames from the RX channel and write them to stdout.
unsafe fn stream_pcm(rx_channel: sys::i2s_chan_handle_t) -> ! {
    static mut PCM_BUF: [i16; BUFFER_SAMPLES_PER_READ] = [0; BUFFER_SAMPLES_PER_READ];

    loop {
        let mut num_bytes_read: usize = 0;
        let ret = sys::i2s_channel_read(
            rx_channel,
            unsafe { PCM_BUF.as_mut_ptr() } as *mut core::ffi::c_void,
            (core::mem::size_of::<i16>() * unsafe { PCM_BUF.len() }) as usize,
            &mut num_bytes_read as *mut usize,
            u32::MAX, // portMAX_DELAY
        );

        if ret != 0 { continue; }

        if num_bytes_read == 0 {
            continue;
        }

        // Attenuate slightly to avoid clipping at the source
        let samples = unsafe {
            core::slice::from_raw_parts_mut(PCM_BUF.as_mut_ptr(), num_bytes_read / core::mem::size_of::<i16>())
        };
        for s in samples.iter_mut() {
            // *s = *s >> 2; // 12 dB down (arith shift keeps sign)
            *s = *s >> 1;   // ~6 dB down is enough (or comment attenuation for now)
            *s = (*s).clamp(-32760, 32760);
        }

        // Write a single DMA frame directly via USB-Serial-JTAG per read
        let raw = core::slice::from_raw_parts(unsafe { PCM_BUF.as_ptr() } as *const u8, num_bytes_read);
        let wrote = sys::usb_serial_jtag_write_bytes(
            raw.as_ptr() as *const core::ffi::c_void,
            raw.len(),
            10,
        );
        if wrote <= 0 {
            unsafe { sys::vTaskDelay(1) }; // short backoff
            continue;
        }
    }
}
