// Push-to-talk: the microphone as 16 kHz mono s16le frames.
import { makeResampler, toPcm16 } from "./logic.js";

const TAP = `class Tap extends AudioWorkletProcessor {
  process(inputs) { const ch = inputs[0][0]; if (ch) this.port.postMessage(ch.slice(0)); return true; }
}
registerProcessor("tap", Tap);`;

export class Mic {
  /** onFrame(Int16Array) is called while recording. */
  constructor(onFrame) {
    this.onFrame = onFrame;
    this.recording = false;
  }

  async open() {
    if (this.ctx) return;
    const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true } });
    const ctx = new AudioContext();
    await ctx.audioWorklet.addModule(URL.createObjectURL(new Blob([TAP], { type: "text/javascript" })));
    const tap = new AudioWorkletNode(ctx, "tap");
    ctx.createMediaStreamSource(stream).connect(tap);
    tap.port.onmessage = (e) => {
      if (this.recording) this.onFrame(toPcm16(this.resample(e.data)));
    };
    this.ctx = ctx;
  }

  async start() {
    await this.open();
    await this.ctx.resume();
    this.resample = makeResampler(this.ctx.sampleRate, 16000);
    this.recording = true;
  }

  stop() {
    this.recording = false;
  }
}
