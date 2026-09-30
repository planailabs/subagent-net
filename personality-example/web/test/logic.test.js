import { test } from "node:test";
import assert from "node:assert/strict";
import { makeResampler, toPcm16, mouthAt, isLegTrack, clipWeights, stepWeights, smooth, smoothAngle, brewProgress, skyColor } from "../src/logic.js";

test("resampling 48k to 16k keeps every third sample, across chunks", () => {
  const r = makeResampler(48000, 16000);
  const input = Float32Array.from({ length: 300 }, (_, i) => i / 300);
  const out = [...r(input.subarray(0, 128)), ...r(input.subarray(128, 256)), ...r(input.subarray(256))];
  assert.equal(out.length, 100);
  out.forEach((v, i) => assert.ok(Math.abs(v - input[i * 3]) < 1e-6, `sample ${i}`));
});

test("resampling 44.1k to 16k gives the right count over chunk boundaries", () => {
  const r = makeResampler(44100, 16000);
  let n = 0;
  for (let c = 0; c < 441; c++) n += r(new Float32Array(100)).length; // one second
  assert.ok(Math.abs(n - 16000) <= 1, `${n}`);
});

test("pcm16 clamps", () => {
  assert.deepEqual([...toPcm16([0, 1, -1, 2, -2, 0.5])], [0, 32767, -32768, 32767, -32768, 16384]);
});

test("the mouth follows the envelope", () => {
  const env = [0, 1, 0.5];
  assert.equal(mouthAt(env, 0.04, 0), 0);
  assert.equal(mouthAt(env, 0.04, 0.04), 1);
  assert.ok(Math.abs(mouthAt(env, 0.04, 0.02) - 0.5) < 1e-9);
  assert.equal(mouthAt(env, 0.04, 1), 0, "closed after the speech");
  assert.equal(mouthAt([], 0.04, 0.1), 0);
});

test("clips: the pose, or legs under an action", () => {
  assert.equal(isLegTrack("thigh_L.quaternion"), true);
  assert.equal(isLegTrack("head.quaternion"), false);
  assert.deepEqual(clipWeights({ pose: "stand", action: null }), { idle: 1 });
  assert.deepEqual(clipWeights({ pose: "walk", action: ["talk", 3] }), { "walk:legs": 1, "talk:upper": 1 });
  assert.deepEqual(clipWeights({ pose: "sit", action: null }), { sit: 1 });
});

test("weights fade in and out", () => {
  let w = { idle: 1 };
  w = stepWeights(w, { walk: 1 }, 0.1, 0.2);
  assert.deepEqual(w, { idle: 0.5, walk: 0.5 });
  w = stepWeights(w, { walk: 1 }, 0.2, 0.2);
  assert.deepEqual(w, { walk: 1 });
  assert.deepEqual(stepWeights({ a: 1 }, { b: 1 }, 1, 0), { b: 1 });
});

test("smoothing converges, angles take the short way", () => {
  assert.ok(Math.abs(smooth(0, 1, 10) - 1) < 1e-6);
  assert.ok(smooth(0, 1, 0.01) < 0.2);
  const a = smoothAngle(3.0, -3.0, 0.05);
  assert.ok(a > 3.0, `goes up through pi, not down through 0: ${a}`);
});

test("coffee progress and the sky", () => {
  assert.equal(brewProgress({ state: "idle" }, 5), 0);
  assert.equal(brewProgress({ state: "ready" }, 5), 1);
  assert.equal(brewProgress({ state: "brewing", ready_at: 30 }, 20), 0.5);
  assert.deepEqual(skyColor(23), skyColor(2));
  assert.notDeepEqual(skyColor(12), skyColor(23));
});
