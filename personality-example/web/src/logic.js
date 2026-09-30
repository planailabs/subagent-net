// Pure logic for the room client (tested with node --test).

/** A linear resampler that keeps its phase across chunks. */
export function makeResampler(from, to) {
  const step = from / to;
  let pos = 0; // position of the next output sample, in input samples, relative to the chunk start
  let last = 0; // the previous chunk's last sample (for interpolating across the boundary)
  return (chunk) => {
    const out = [];
    while (pos < chunk.length) {
      const i = Math.floor(pos);
      const f = pos - i;
      const a = i < 0 ? last : chunk[i];
      const b = i + 1 < chunk.length ? chunk[i + 1] : chunk[chunk.length - 1];
      out.push(a + (b - a) * f);
      pos += step;
    }
    pos -= chunk.length;
    if (chunk.length) last = chunk[chunk.length - 1];
    return Float32Array.from(out);
  };
}

/** Float samples in -1..1 to 16-bit PCM. */
export function toPcm16(samples) {
  const out = new Int16Array(samples.length);
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    out[i] = Math.round(s < 0 ? s * 32768 : s * 32767);
  }
  return out;
}

/** How open her mouth is at time t (s) of a speech. */
export function mouthAt(envelope, frame, t) {
  if (!envelope.length || t < 0) return 0;
  const x = t / frame;
  const i = Math.floor(x);
  if (i >= envelope.length) return 0;
  const a = envelope[i];
  const b = i + 1 < envelope.length ? envelope[i + 1] : 0;
  return a + (b - a) * (x - i);
}

export const LEG_BONES = new Set(["hips", "thigh_L", "thigh_R", "shin_L", "shin_R", "foot_L", "foot_R"]);

/** Whether a three.js track ("thigh_L.quaternion") moves the legs. */
export function isLegTrack(name) {
  return LEG_BONES.has(name.split(".")[0]);
}

/**
 * The clips she should be playing: the pose's clip, or its legs under an
 * action's upper body. Clip variants are named "walk:legs", "talk:upper".
 */
export function clipWeights(vesper) {
  const base = vesper.pose === "walk" ? "walk" : vesper.pose === "sit" ? "sit" : "idle";
  const action = vesper.action && vesper.action[0];
  return action ? { [`${base}:legs`]: 1, [`${action}:upper`]: 1 } : { [base]: 1 };
}

/** Moves clip weights toward the wanted ones, fading over `fade` seconds. */
export function stepWeights(current, wanted, dt, fade = 0.25) {
  const out = {};
  const k = fade > 0 ? dt / fade : 1;
  for (const name of new Set([...Object.keys(current), ...Object.keys(wanted)])) {
    const from = current[name] || 0;
    const to = wanted[name] || 0;
    const w = to > from ? Math.min(to, from + k) : Math.max(to, from - k);
    if (w > 0) out[name] = w;
  }
  return out;
}

/** Exponential smoothing towards a target (frame-rate independent). */
export function smooth(cur, target, dt, rate = 10) {
  return cur + (target - cur) * (1 - Math.exp(-dt * rate));
}

/** Smoothing for angles, the short way round. */
export function smoothAngle(cur, target, dt, rate = 10) {
  let d = (target - cur) % (2 * Math.PI);
  if (d > Math.PI) d -= 2 * Math.PI;
  if (d < -Math.PI) d += 2 * Math.PI;
  return cur + d * (1 - Math.exp(-dt * rate));
}

/** How far the coffee is, 0..1, at world time t. */
export function brewProgress(brew, t, secs = 20) {
  if (!brew || brew.state === "idle") return 0;
  if (brew.state === "ready") return 1;
  return Math.max(0, Math.min(1, 1 - (brew.ready_at - t) / secs));
}

/** The sky behind the window by local hour: [r, g, b] in 0..1. */
export function skyColor(hour) {
  if (hour >= 5 && hour < 8) return [0.55, 0.45, 0.55]; // dawn
  if (hour >= 8 && hour < 17) return [0.55, 0.6, 0.66]; // grey day
  if (hour >= 17 && hour < 21) return [0.3, 0.16, 0.38]; // dusk
  return [0.03, 0.04, 0.1]; // night
}
