// The first-launch film, Act 3: the live part. It takes over from the star film (intro.js) at the
// moment the ball has formed, starting from exactly the same dot positions, so there is no cut.
// The ball hops, twirls in 3D, then its dots rush left and re-form as the listening orb while the
// setup window grows out around it: dots trace a rounded rectangle that opens from the orb to the
// window's size, the window fills in, and the outline fades. Plain 2D canvas, time-driven.

import { resolvePreset, MODE_FRAMES, scaleCounts, scaleRadii } from './vendor/thinking-orbs.js';

const TAU = Math.PI * 2;
const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const ease = x => x * x * (3 - 2 * x);
const out3 = x => 1 - (1 - x) ** 3;
const inOut = x => (x < 0.5 ? 4 * x * x * x : 1 - (-2 * x + 2) ** 3 / 2);
const span = (t, a, b) => clamp((t - a) / (b - a));

// The same dense listening orb as the pill.
const dense = (p, count, size) => ({ ...p, opts: scaleRadii(scaleCounts(p.opts, count), size) });
const LISTEN = dense(resolvePreset('listening', 64), 3, 0.62);

/** Times (s, on the film's clock) of Act 3's beats. */
export const ACT3 = { take: 7.4, hop: 7.9, land: 8.45, twirl: 8.45, twirled: 9.3, rush: 9.4, rushed: 10.15, card: 9.65, carded: 10.35, fill: 10.05, content: 10.35 };

/**
 * `canvas` covers the screen (css px), transparent. `layout()` returns the window's rect and the
 * orb's slot in it: { card: {x,y,w,h,r}, orb: {x,y,size} }, all css px.
 */
export function createAct3(canvas, { n = 520, r = 46, tilt = 0.42, spin = 0.6, dpr = Math.min(2, devicePixelRatio || 1), layout, light = false } = {}) {
  const g = canvas.getContext('2d');
  const golden = Math.PI * (3 - Math.sqrt(5));
  // The ball's dots as 3D unit vectors, built exactly as intro.js ballDots builds them.
  const sphere = Array.from({ length: n }, (_, i) => {
    const y = 1 - (2 * (i + 0.5)) / n, rad = Math.sqrt(1 - y * y), a = i * golden + spin;
    return [Math.cos(a) * rad, y, Math.sin(a) * rad];
  });
  const hash = i => { const h = Math.sin(i * 12.9898) * 43758.5453; return h - Math.floor(h); };
  const delay = sphere.map((_, i) => hash(i + 3) * 0.22);  // the rush leaves in a ripple
  const bow = sphere.map((_, i) => (hash(i + 9) - 0.5) * 120); // and on slightly curved paths
  let W = 0, H = 0;

  function size() {
    W = canvas.clientWidth;
    H = canvas.clientHeight;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
  }

  // The ball at time t: centre, squash (sx, sy) and 3D turn.
  function ballAt(t) {
    const cx = W / 2;
    let cy = H / 2, sx = 1, sy = 1, turn = (t - ACT3.take) * 0.35;
    // Hop: a little crouch first, up, then a squash on landing that springs back.
    const crouch = span(t, ACT3.hop - 0.12, ACT3.hop), up = span(t, ACT3.hop, ACT3.land);
    if (t < ACT3.hop) { sy = 1 - 0.1 * ease(crouch); sx = 1 + 0.08 * ease(crouch); }
    else if (t < ACT3.land) { cy -= 38 * Math.sin(Math.PI * up); sy = 1 + 0.08 * Math.sin(Math.PI * up); sx = 1 - 0.05 * Math.sin(Math.PI * up); }
    const since = t - ACT3.land;
    if (since > 0) { const k = Math.exp(-since * 9) * Math.cos(since * 26); sy *= 1 - 0.12 * k; sx *= 1 + 0.1 * k; }
    // Twirl: a full turn about its axis, with a smaller hop at the middle of it.
    const tw = span(t, ACT3.twirl, ACT3.twirled);
    turn += TAU * inOut(tw);
    if (tw > 0 && tw < 1) cy -= 16 * Math.sin(Math.PI * tw);
    return { cx, cy, sx, sy, turn };
  }

  function project(v, b) {
    const [x0, y0, z0] = v;
    const c = Math.cos(b.turn), s = Math.sin(b.turn);
    const x = x0 * c + z0 * s, z = -x0 * s + z0 * c;
    const y2 = y0 * Math.cos(tilt) - z * Math.sin(tilt), z2 = y0 * Math.sin(tilt) + z * Math.cos(tilt);
    return [b.cx + x * r * b.sx, b.cy - y2 * r * b.sy, (z2 + 1) / 2];
  }

  // A point u (0..1) round a rounded rectangle, clockwise from the top.
  function edge(R, u) {
    const rad = Math.min(R.r, R.w / 2, R.h / 2), sw = R.w - 2 * rad, sh = R.h - 2 * rad, arc = (Math.PI / 2) * rad;
    const segs = [sw, arc, sh, arc, sw, arc, sh, arc];
    let d = (u % 1) * segs.reduce((a, b) => a + b, 0);
    const L = R.x + rad, Rr = R.x + R.w - rad, T = R.y + rad, B = R.y + R.h - rad;
    const corner = (ox, oy, a0, f) => [ox + rad * Math.cos(a0 + (f * Math.PI) / 2), oy + rad * Math.sin(a0 + (f * Math.PI) / 2)];
    const parts = [f => [L + f * sw, R.y], f => corner(Rr, T, -Math.PI / 2, f), f => [R.x + R.w, T + f * sh], f => corner(Rr, B, 0, f), f => [Rr - f * sw, R.y + R.h], f => corner(L, B, Math.PI / 2, f), f => [R.x, B - f * sh], f => corner(L, T, Math.PI, f)];
    for (let k = 0; k < 8; k++) {
      if (d <= segs[k] || k === 7) return parts[k](segs[k] ? Math.min(1, d / segs[k]) : 0);
      d -= segs[k];
    }
  }

  const lerp = (a, b, k) => a + (b - a) * k;

  /** Draws Act 3 at film time t; returns how far the window has filled in (0..1) and its rect. */
  function draw(t) {
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    if (t < ACT3.take) return { fill: 0 };
    const L = layout(W, H);
    const rushStart = ballAt(ACT3.rush);
    // The orb the dots re-form as: the dense listening orb, dot i keyed by id like the pill.
    const size = L.orb.size, orbT = Math.max(0, t - ACT3.rush) * LISTEN.speed;
    const frame = MODE_FRAMES[LISTEN.mode](size, orbT, LISTEN.opts).dots;
    const byId = new Map(frame.map(d => [d.i, d]));
    const k0 = span(t, ACT3.rush, ACT3.rushed);

    // The disc behind the ball: fades as the dots leave it.
    if (k0 < 1) {
      const b = ballAt(Math.min(t, ACT3.rush));
      g.save();
      g.globalAlpha = 1 - ease(span(t, ACT3.rush, ACT3.rush + 0.3));
      g.beginPath();
      g.ellipse(b.cx, b.cy, r * 1.15 * b.sx, r * 1.15 * b.sy, 0, 0, TAU);
      g.fillStyle = 'rgba(0,0,0,0.78)';
      g.shadowColor = 'rgba(0,0,0,0.35)';
      g.shadowBlur = 24;
      g.shadowOffsetY = 6;
      g.fill();
      g.shadowColor = 'transparent';
      g.strokeStyle = 'rgba(255,255,255,0.14)';
      g.lineWidth = 1;
      g.stroke();
      g.restore();
    }

    // The ball's dots (back to front), then their rush to the orb.
    const pts = sphere.map((v, i) => {
      const k = t < ACT3.rush ? 0 : out3(span(t - delay[i], ACT3.rush, ACT3.rushed - 0.22));
      const [bx, by, bz] = project(v, k > 0 ? rushStart : ballAt(t));
      const d = byId.get(i);
      if (k <= 0) return { x: bx, y: by, z: bz, r: lerp(1.0, 1.7, bz), a: lerp(0.28, 1, bz), px: bx, py: by };
      const ox = L.orb.x - size / 2, oy = L.orb.y - size / 2;
      const tx = d ? ox + d.x : L.orb.x, ty = d ? oy + d.y : L.orb.y;
      // A curved path: bowed off the straight line, more in the middle of the trip.
      const mx = (bx + tx) / 2, my = (by + ty) / 2 + bow[i];
      const q = (u) => [(1 - u) ** 2 * bx + 2 * (1 - u) * u * mx + u * u * tx, (1 - u) ** 2 * by + 2 * (1 - u) * u * my + u * u * ty];
      // The streak shrinks to nothing as the dot arrives, so the finished orb is crisp.
      const [x, y] = q(k), [px, py] = q(Math.max(0, k - 0.08 * (1 - k)));
      const ink = d ? 1 - d.white : 0.3;
      return { x, y, z: d ? d.z : -1, r: lerp(lerp(1.0, 1.7, bz), d ? d.r : 0.3, k), a: lerp(lerp(0.28, 1, bz), d ? (d.a ?? 1) * clamp(ink * 1.4) : 0, k), px, py };
    });
    pts.sort((a, b) => a.z - b.z);
    // On a light screen the dots turn to ink as the window fills in (white would vanish on it),
    // and while they fly over the desktop they float with a soft shadow, like the stars.
    const kFill = ease(span(t, ACT3.fill, ACT3.content));
    const ink = light ? [Math.round(lerp(238, 29, kFill)), Math.round(lerp(241, 29, kFill)), Math.round(lerp(255, 31, kFill))] : [238, 241, 255];
    const rgba = a => `rgba(${ink[0]},${ink[1]},${ink[2]},${a})`;
    for (const p of pts) {
      if (p.a < 0.02) continue;
      const moving = Math.hypot(p.x - p.px, p.y - p.py);
      if (moving > 1.5) {
        // A short streak, bright at its head.
        const grad = g.createLinearGradient(p.px, p.py, p.x, p.y);
        grad.addColorStop(0, rgba(0));
        grad.addColorStop(1, rgba(p.a));
        g.strokeStyle = grad;
        g.lineWidth = p.r * 1.6;
        g.lineCap = 'round';
        g.beginPath();
        g.moveTo(p.px, p.py);
        g.lineTo(p.x, p.y);
        g.stroke();
      }
      if (light && kFill < 1 && t >= ACT3.rush) {
        g.fillStyle = `rgba(0,0,0,${0.22 * p.a * (1 - kFill)})`;
        g.beginPath();
        g.arc(p.x + 0.8, p.y + 1.4, Math.max(0.6, p.r * 1.6), 0, TAU);
        g.fill();
      }
      g.fillStyle = rgba(p.a);
      g.beginPath();
      g.arc(p.x, p.y, Math.max(0.3, p.r), 0, TAU);
      g.fill();
    }
    // After the rush, the orb keeps listening: the dots it doesn't use stay hidden.

    // The window grows out round the orb: a rounded rectangle traced in dots, from just around
    // the orb to the window's size; the outline fades once the window has filled in.
    const kc = out3(span(t, ACT3.card, ACT3.carded));
    const fill = ease(span(t, ACT3.fill, ACT3.content));
    if (kc > 0) {
      const from = { x: L.orb.x - size * 0.62, y: L.orb.y - size * 0.62, w: size * 1.24, h: size * 1.24, r: size * 0.62 };
      const R = { x: lerp(from.x, L.card.x, kc), y: lerp(from.y, L.card.y, kc), w: lerp(from.w, L.card.w, kc), h: lerp(from.h, L.card.h, kc), r: lerp(from.r, L.card.r, kc) };
      const per = 2 * (R.w + R.h), count = Math.round(per / 5);
      const a = (1 - fill) * Math.min(1, kc * 3) * 0.85;
      if (a > 0.01) {
        g.fillStyle = light ? `rgba(29,29,31,${a * 0.7})` : `rgba(238,241,255,${a})`;
        for (let i = 0; i < count; i++) {
          const [x, y] = edge(R, i / count + (1 - kc) * 0.15); // the outline also turns a little as it opens
          g.beginPath();
          g.arc(x, y, 0.9, 0, TAU);
          g.fill();
        }
      }
      return { fill, rect: R };
    }
    return { fill: 0 };
  }

  size();
  return { draw, size };
}
