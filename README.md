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

## Websocket audio streaming
The firmware also exposes the PCM stream over Wi-Fi via a websocket endpoint (`/audio`).

1. Copy `.env.example` to `.env` and fill in `WIFI_SSID`/`WIFI_PASS` for the network you want to join.
2. Flash & run the websocket build:
   ```
   just run-websocket
   ```
3. After the device acquires DHCP it prints a banner like `=== set -gx S3_IP 192.168.68.122 ===`. Export that IP for your shell (e.g. `export S3_IP=192.168.68.122` or copy the `set -gx` command if you use fish).
4. You can now:
   - Open `tools/index.html` in a browser and connect using the IP (plays audio via Web Audio).
   - Or inspect the raw PCM frames from your terminal:
     ```
     just ws-hexdump S3_IP=$S3_IP
     ```
     (Requires [`websocat`](https://github.com/vi/websocat) on your host.)
