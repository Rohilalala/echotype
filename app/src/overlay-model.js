// What overlay.js decides without touching the page, so node can test it (tests/overlay-model.test.js).

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

// "12:34", or "1:02:03" past the hour; a clock that runs behind never shows a negative time.
export function clockText(ms) {
  const s = Math.max(0, Math.floor(ms / 1000)), h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, ss = String(s % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

// What the capsule shows: a dictation state first, then a recording meeting, then the idle bar
// (opened into its hint while hovered). `view` is what pill.rs sent (null in the browser preview,
// where the state alone decides). pill.rs sizes the hit area by the same order.
export function face(state, view) {
  const active = state !== 'idle' && (view ? view.active : true);
  const meeting = !active && !!view?.meeting;
  return { active, meeting, expand: !active && !meeting && !!view?.expand };
}

// The card's first line: why nothing was pasted, short enough for one line (the full reason is
// the tooltip). Accessibility off and a blocked paste both left the text on the clipboard.
export function cardReason(p) {
  if (p.action === 'accessibility') return 'Accessibility is off, copied instead';
  return !p.warning || p.warning === 'Nowhere to paste' ? 'Nowhere to paste' : 'Paste was blocked, copied instead';
}
