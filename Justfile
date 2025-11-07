# Justfile for esp32s3_i2s_mic_idf

default: run-release

run-release:
	cargo run --release

s3-to-video-raw:
	just serial-run | ffplay -f s16le -ar 16000 -i -

s3-to-video-compressor:
	just serial-run \
	| ffplay -f s16le -ar 16000 -i - \
	  -af "volume=0.6,highpass=f=120,adeclick=w=20:o=80:t=4:b=2:m=s,alimiter=limit=0.90:level=disabled,acompressor=threshold=-12dB:ratio=6:attack=0.5:release=60"


# Alias for convenience
s3-to-video:
	just s3-to-video-compressor

install-denoiser:
	mkdir -p ~/.config/ffmpeg
	curl -L -o ~/.config/ffmpeg/rnnoise.rnnn \
	  https://github.com/GregorR/rnnoise-models/raw/master/somnolent-hogwash-2018-09-01/sh.rnnn

s3-to-rnn:
	just serial-run \
		| ffplay -autoexit -f s16le -ar 16000 -i - \
		-af "volume=2,\
		arnndn=m=$HOME/.config/ffmpeg/rnnoise.rnnn:mix=1.0,\
		highpass=f=90,lowpass=f=3800,\
		adeclick=w=12:o=85:t=6:b=2,\
		speechnorm=e=6:r=0.0001:l=1,\
		acompressor=threshold=-12dB:ratio=6:attack=2:release=60:makeup=6,\
		alimiter=limit=0.8:level=disabled"

# Host-side Rust helper (build and run on macOS host)
serial-build:
	cargo build --manifest-path tools/serial_to_stdout/Cargo.toml --target x86_64-apple-darwin --release

serial-run PORT='/dev/cu.usbmodem2101' BAUD='921600':
	cargo run --manifest-path tools/serial_to_stdout/Cargo.toml --target x86_64-apple-darwin --release -- {{PORT}} {{BAUD}}

s3-to-video-rust PORT='/dev/cu.usbmodem2101':
	cargo run --manifest-path tools/serial_to_stdout/Cargo.toml --target x86_64-apple-darwin --release -- {{PORT}} \
		| ffplay -f s16le -ar 16000 -i -
