# WAT
Stream microphone to audio, video and file.
Works on Xiao Sense ESP32-S3 development board. See https://xiao.esp32.com/en/latest/hardware/sense.html for details.
![spectrum](docs/spectrum.png)


## Quickstart
0) install toolchain
https://github.com/esp-rs/esp-idf-template?tab=readme-ov-file#install-cargo-sub-commands
install just
https://github.com/casey/just
AKA
```
cargo isntall just
```

1) Build, flash and open monitor

```
just
```

2) Close the monitor (Ctrl-C) once the device is running
3) Stream audio to your computer

```
just s3-to-video
```

Notes:
- Use `just s3-to-video-raw` for an unprocessed stream, or `just s3-to-rnn` for RNNoise filtering (run `just install-denoiser` once).
- To choose a different serial port/baud for the Rust helper: `just serial-run PORT=/dev/tty.usbmodemXXXX BAUD=921600`.

