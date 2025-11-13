class PCMPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queue = [];
    this.current = null;
    this.offset = 0;
    this.port.onmessage = (event) => {
      const data = event.data;
      if (data instanceof Float32Array) {
        this.queue.push(data);
      } else if (data && data.buffer instanceof ArrayBuffer) {
        this.queue.push(new Float32Array(data.buffer));
      }
    };
  }

  process(_inputs, outputs) {
    const outputChannels = outputs[0];
    if (!outputChannels || outputChannels.length === 0) {
      return true;
    }
    const channel = outputChannels[0];
    const frames = channel.length;

    let frameIndex = 0;
    while (frameIndex < frames) {
      if (!this.current || this.offset >= this.current.length) {
        this.current = this.queue.shift();
        this.offset = 0;
        if (!this.current) {
          while (frameIndex < frames) {
            channel[frameIndex++] = 0;
          }
          return true;
        }
      }

      const available = this.current.length - this.offset;
      const remaining = frames - frameIndex;
      const toCopy = Math.min(available, remaining);
      channel.set(this.current.subarray(this.offset, this.offset + toCopy), frameIndex);
      frameIndex += toCopy;
      this.offset += toCopy;
    }

    return true;
  }
}

registerProcessor('pcm-player-processor', PCMPlayerProcessor);
