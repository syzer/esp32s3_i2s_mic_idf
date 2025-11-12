// Shared helpers and constants used by both the firmware binary and the helper binaries
use esp_idf_sys as sys;
use core::cell::UnsafeCell;

pub const SAMPLE_RATE: u32 = 16_000;
pub const DMA_DESC_NUM: u32 = 24;
pub const DMA_FRAME_NUM: u32 = 512;
pub const BUFFER_SAMPLES_PER_READ: usize = 512;
pub const FRAME_BYTES: usize = 640; // 20 ms @ 16 kHz mono s16

// FreeRTOS task configuration
pub const TASK_STACK_WORDS: u32 = 8192; // stack in 32-bit words
pub const TASK_PRIORITY: u32 = 12;      // higher than default app tasks
pub const TASK_CORE_ID: i32 = 0;        // pin to PRO CPU on ESP32-S3

// GPIO mapping for the microphone
pub const MIC_CLK_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_42;
pub const MIC_DIN_GPIO: sys::gpio_num_t = sys::gpio_num_t_GPIO_NUM_41;

/// Create and return an I2S RX channel handle.
/// Unsafe because it calls raw ESP-IDF bindings.
pub unsafe fn create_i2s_rx_channel() -> sys::i2s_chan_handle_t {
    let mut rx_channel: sys::i2s_chan_handle_t = core::ptr::null_mut();

    let channel_config: sys::i2s_chan_config_t = sys::i2s_chan_config_t {
        id: 0, // I2S_NUM_0
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

/// Configure a previously-created channel for PDM RX
pub unsafe fn configure_pdm_rx(rx_channel: sys::i2s_chan_handle_t) {
    // GPIO config: use union field for DIN
    let mut pdm_gpio: sys::i2s_pdm_rx_gpio_config_t = core::mem::zeroed();
    pdm_gpio.clk = MIC_CLK_GPIO;
    pdm_gpio.__bindgen_anon_1.din = MIC_DIN_GPIO;

    // Clocking config: 16 kHz target, 128x MCLK, auto BCLK divider
    let pdm_clk: sys::i2s_pdm_rx_clk_config_t = sys::i2s_pdm_rx_clk_config_t {
        sample_rate_hz: SAMPLE_RATE,
        clk_src: sys::soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT,
        mclk_multiple: sys::i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256,
        // use 16S by default here
        dn_sample_mode: sys::i2s_pdm_dsr_t_I2S_PDM_DSR_16S,
        bclk_div: 0, // auto
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

/// Read PCM frames from the RX channel and forward them to the provided sink callback.
/// The sink receives a slice of i16 samples and must return the number of bytes written
/// (or a negative value on error). This function never returns.
pub unsafe fn stream_to_sink<F>(rx_channel: sys::i2s_chan_handle_t, mut sink: F) -> !
where
    F: FnMut(&[i16]) -> isize,
{
    // Wrapper type so we can mark the static as Sync. Access is still unsafe and
    // must follow the single-writer or external synchronization assumptions.
    struct PcmBuf(UnsafeCell<[i16; BUFFER_SAMPLES_PER_READ]>);
    unsafe impl Sync for PcmBuf {}

    static PCM_BUF: PcmBuf = PcmBuf(UnsafeCell::new([0; BUFFER_SAMPLES_PER_READ]));

    loop {
        let mut num_bytes_read: usize = 0;
        let ret = sys::i2s_channel_read(
            rx_channel,
            PCM_BUF.0.get() as *mut core::ffi::c_void,
            (core::mem::size_of::<i16>() * BUFFER_SAMPLES_PER_READ) as usize,
            &mut num_bytes_read as *mut usize,
            u32::MAX, // portMAX_DELAY
        );

        if ret != 0 {
            continue;
        }
        if num_bytes_read == 0 {
            continue;
        }

    let samples = core::slice::from_raw_parts_mut(PCM_BUF.0.get() as *mut i16, num_bytes_read / core::mem::size_of::<i16>());

        // Attenuate slightly and clamp
        for s in samples.iter_mut() {
            *s = *s >> 1;
            *s = (*s).clamp(-32760, 32760);
        }

        let wrote = sink(samples);
        if wrote <= 0 {
            sys::vTaskDelay(1);
            continue;
        }
    }
}

/// Convenience sink that writes raw s16le PCM to the USB-Serial-JTAG driver.
pub unsafe fn usb_serial_sink(samples: &[i16]) -> isize {
    let raw = core::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * core::mem::size_of::<i16>());
    sys::usb_serial_jtag_write_bytes(raw.as_ptr() as *const core::ffi::c_void, raw.len(), 10) as isize
}

/// Convenience: stream from channel to USB serial (never returns).
pub unsafe fn stream_to_usb(rx_channel: sys::i2s_chan_handle_t) -> ! {
    stream_to_sink(rx_channel, |samples: &[i16]| unsafe { usb_serial_sink(samples) })
}

// Function-pointer trampoline API so callers can provide their own never-returning
// stream function located in `main.rs` or another binary.
pub type StreamFn = unsafe extern "C" fn(sys::i2s_chan_handle_t) -> !;

static mut STREAM_FN: Option<StreamFn> = None;

unsafe extern "C" fn pdm_trampoline(_pv: *mut core::ffi::c_void) {
    // Install USB Serial JTAG driver as the firmware did so stream functions can write to it.
    let mut usb_cfg: sys::usb_serial_jtag_driver_config_t = core::mem::zeroed();
    usb_cfg.tx_buffer_size = 2048 * 2;
    usb_cfg.rx_buffer_size = 2048 * 2;
    let _ = sys::usb_serial_jtag_driver_install(&mut usb_cfg);

    let rx_channel = create_i2s_rx_channel();
    configure_pdm_rx(rx_channel);

    let ret = sys::i2s_channel_enable(rx_channel);
    if ret != 0 {
        // Could log if desired, but keep trampoline minimal.
        sys::vTaskDelete(core::ptr::null_mut());
    }

    // Call the user-provided stream function. It is expected to never return (!).
    match STREAM_FN {
        Some(f) => f(rx_channel),
        None => {
            // No stream function set; delete task.
            sys::vTaskDelete(core::ptr::null_mut());
        }
    }
}

/// Start the PDM pipeline and spawn a pinned FreeRTOS task that will call the provided
/// `stream` function. The `stream` must have signature `unsafe extern "C" fn(rx) -> !`.
pub fn start_pdm_with_stream(stream: StreamFn) {
    unsafe {
        STREAM_FN = Some(stream);
    }

    // spawn the FreeRTOS task
    let task_name = b"pdm_rx\0";
    unsafe {
        let _res = sys::xTaskCreatePinnedToCore(
            Some(pdm_trampoline),
            task_name.as_ptr(),
            TASK_STACK_WORDS,
            core::ptr::null_mut(),
            TASK_PRIORITY,
            core::ptr::null_mut(),
            TASK_CORE_ID,
        );
    }
}
