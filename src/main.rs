use anyhow::Result;
use esp_idf_sys as sys;
use log::{error, info};
use std::time::Duration;

use esp32s3_i2s_mic_idf::{create_i2s_rx_channel, configure_pdm_rx, TASK_STACK_WORDS, TASK_PRIORITY, TASK_CORE_ID, BUFFER_SAMPLES_PER_READ};

fn main() -> Result<()> {
    // Link ESP-IDF patches and ensure runtime is initialized
    sys::link_patches();
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    // Start the shared PDM pipeline and hand off to the application-defined stream function
    // (keeps the hot-path `stream_pcm` in this file).
    esp32s3_i2s_mic_idf::start_pdm_with_stream(stream_pcm as esp32s3_i2s_mic_idf::StreamFn);

    // Keep main task minimal to avoid stack overflow; the background task runs the pipeline
    loop { std::thread::sleep(Duration::from_millis(1000)); }
}

/// Continuously read PCM frames from the RX channel and write them to stdout.
unsafe extern "C" fn stream_pcm(rx_channel: sys::i2s_chan_handle_t) -> ! {
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
