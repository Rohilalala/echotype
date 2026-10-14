// The pill's meter needle: a damped spring that rises fast (~30 ms) and falls slowly (~300 ms),
// like a VU meter. Damping 0.8 keeps the overshoot near 1%. `still` (reduced motion) swaps the
// spring for a plain lag with the same timing: the needle still moves, it never overshoots.
const ATTACK = 95, RELEASE = 10, DAMPING = 0.8; // rad/s, rad/s, ratio
const TAU_UP = 0.013, TAU_DOWN = 0.13; // s, the lag's time constants (90% / 10% in ~30 / ~300 ms)

export function step({ x, v }, target, dt, still = false) {
  if (still) {
    x += (target - x) * (1 - Math.exp(-dt / (target > x ? TAU_UP : TAU_DOWN)));
    return { x, v: 0 };
  }
  const n = Math.max(1, Math.ceil(dt / 0.004)), h = dt / n;
  for (let i = 0; i < n; i++) {
    const w = target > x || v > 0 ? ATTACK : RELEASE;
    v += (w * w * (target - x) - 2 * DAMPING * w * v) * h;
    x += v * h;
  }
  return x < 0 ? { x: 0, v: 0 } : { x, v };
}

// Degrees from upright: -48 at rest, +48 at full scale, a little past it at most.
export const angle = x => -48 + 96 * Math.min(x, 1.03);
