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


## Listening in a browser via WebSocket + WebAudio

You can forward audio to a browser client that plays it with the WebAudio API. The repository contains a small client at `tools/index.html` which expects a websocket that sends raw 16 kHz signed 16-bit little-endian (s16le) mono PCM.

Steps to try it locally:

1. Start the websocket server that forwards PCM from the device. For development you can run the local helper binary (this binary is a stub that sets up the same I2S pipeline; implement forwarding if you want to use it):

```
just run-websocket
```

2. Serve the `tools/` folder so the browser can load the client. You can use `live-server` (npm) which automatically reloads, or a simple Python static server. Example using live-server (you already ran this):

```fish
cd tools
live-server .
# or python3 -m http.server 8080
```

You should see output like:

Serving "." at http://127.0.0.1:8080

3. Open the page in your browser and set the websocket URL to your running server (default in the page is `ws://localhost:9000`). Click "Connect & Play".

Notes about the audio format
- Sample rate: 16000 Hz
- Format: signed 16-bit little-endian (s16le)
- Channels: mono

If your websocket server uses a different URL or encoding, update `tools/index.html` accordingly or rework the server to forward raw s16le PCM frames.

Troubleshooting
- If the browser shows no audio, ensure the websocket server is running and sending binary ArrayBuffer messages containing s16le frames.
- If you get a 404 for favicon (GET /favicon.ico 404) it's harmless.

