// The first-launch film after the stars: the live part. It takes over from intro.js at the moment
// the ball has formed, from exactly the same dot positions, so there is no cut. The ball hops and
// twirls, flows into the orb where it is, the setup window grows out from behind it and says
// welcome, then the orb's dots move to the window's left column under real physics (each dot a
// mass on a spring, released in a ripple). At the end of setup the dots stream back to the centre,
// the window shrinks away behind them, and they fall under gravity to the pill's place at the
// bottom of the screen, bounce, and settle into the idle ball.
//
// Physics needs history, so the simulation steps at a fixed 240 Hz from Act 3's start to the
// requested time (and starts over when asked for an earlier one): any frame can be drawn.

import { resolvePreset, MODE_FRAMES, scaleCounts, scaleRadii } from './vendor/thinking-orbs.js';

const TAU = Math.PI * 2;
const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
const ease = x => x * x * (3 - 2 * x);
const out3 = x => 1 - (1 - x) ** 3;
const inOut = x => (x < 0.5 ? 4 * x * x * x : 1 - (-2 * x + 2) ** 3 / 2);
const span = (t, a, b) => clamp((t - a) / (b - a));
const lerp = (a, b, k) => a + (b - a) * k;

const dense = (p, count, size) => ({ ...p, opts: scaleRadii(scaleCounts(p.opts, count), size) });
const LISTEN = dense(resolvePreset('listening', 64), 3, 0.62);

/** Beats (s, on the film's clock). `finish` is when setup ends; in the app it comes from the flow. */
export const ACT3 = {
  take: 7.4, hop: 7.9, land: 8.45, twirl: 8.45, twirled: 9.3,
  form: 9.35,                     // the ball flows into the orb, where it is
  card: 9.55, carded: 10.3,       // the window grows out from behind the orb
  fill: 10.0, filled: 10.4,
  welcome: 10.45,                 // "Welcome to Echotype"
  left: 12.3,                     // the orb moves to the left column
  content: 12.9,                  // screen 1 comes in
  finish: 15.2,                   // setup is done: back to the centre
  shrink: 15.6, shrunk: 16.2,     // the window shrinks away behind the orb
  fall: 16.35,                    // the dots fall to the pill
  landed: 17.6,
};

const STEP = 1 / 240;

export function createAct3(canvas, { n = 520, r = 46, tilt = 0.42, spin = 0.6, dpr = Math.min(2, devicePixelRatio || 1), layout, light = false, home } = {}) {
  const g = canvas.getContext('2d');
  const golden = Math.PI * (3 - Math.sqrt(5));
  const sphere = Array.from({ length: n }, (_, i) => {
    const y = 1 - (2 * (i + 0.5)) / n, rad = Math.sqrt(1 - y * y), a = i * golden + spin;
    return [Math.cos(a) * rad, y, Math.sin(a) * rad];
  });
  const hash = i => { const h = Math.sin(i * 12.9898) * 43758.5453; return h - Math.floor(h); };
  const jitter = sphere.map((_, i) => hash(i + 3));
  let W = 0, H = 0;
  let sim = null; // { t, p: [{x,y,vx,vy,bounced}] }

  function size() {
    W = canvas.clientWidth;
    H = canvas.clientHeight;
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    sim = null;
  }

  // ----- the ball (kinematic: hop and twirl) -----
  function ballAt(t) {
    const cx = W / 2;
    let cy = H / 2, sx = 1, sy = 1, turn = (t - ACT3.take) * 0.35;
    const crouch = span(t, ACT3.hop - 0.12, ACT3.hop), up = span(t, ACT3.hop, ACT3.land);
    if (t < ACT3.hop) { sy = 1 - 0.1 * ease(crouch); sx = 1 + 0.08 * ease(crouch); }
    else if (t < ACT3.land) { cy -= 38 * Math.sin(Math.PI * up); sy = 1 + 0.08 * Math.sin(Math.PI * up); sx = 1 - 0.05 * Math.sin(Math.PI * up); }
    const since = t - ACT3.land;
    if (since > 0) { const k = Math.exp(-since * 9) * Math.cos(since * 26); sy *= 1 - 0.12 * k; sx *= 1 + 0.1 * k; }
    const tw = span(t, ACT3.twirl, ACT3.twirled);
    turn += TAU * inOut(tw);
    if (tw > 0 && tw < 1) cy -= 16 * Math.sin(Math.PI * tw);
    return { cx, cy, sx, sy, turn };
  }
  function project(v, b) {
    const [x0, y0, z0] = v, c = Math.cos(b.turn), s = Math.sin(b.turn);
    const x = x0 * c + z0 * s, z = -x0 * s + z0 * c;
    const y2 = y0 * Math.cos(tilt) - z * Math.sin(tilt), z2 = y0 * Math.sin(tilt) + z * Math.cos(tilt);
    return [b.cx + x * r * b.sx, b.cy - y2 * r * b.sy, (z2 + 1) / 2];
  }

  // ----- where things are -----
  // The orb's centre: the middle of the screen (a little above the window's centre, so "Welcome"
  // fits under it), then the window's left column, then the middle again for the way home.
  function orbHome(t, L) {
    const mid = { x: W / 2, y: L.card.y + L.card.h * 0.4 };
    if (t < ACT3.left || t >= ACT3.finish) return mid;
    return { x: L.orb.x, y: L.orb.y };
  }
  // The idle ball at the bottom of the screen: a sunflower disc, as the pill draws it.
  const BALL = 90;
  function ballSlot(i, P) {
    const j = i % BALL, rr = P.r * Math.sqrt((j + 0.5) / BALL), th = j * 2.39996;
    return [P.x + rr * Math.cos(th), P.y + rr * Math.sin(th)];
  }

  // Where dot i is pulled at time t, and how hard (null while it falls).
  function target(i, t, frame, L) {
    if (t >= ACT3.fall) return null;
    const d = frame.get(i), o = orbHome(t, L), size = L.orb.size;
    const moving = t >= ACT3.left && t < ACT3.finish;
    if (!d) return { x: o.x, y: o.y, k: 40, z: 0.55 };
    return { x: o.x - size / 2 + d.x, y: o.y - size / 2 + d.y, k: moving ? 55 : 70, z: moving ? 0.5 : 0.55 };
  }

  const orbFrame = (t, L) => new Map(MODE_FRAMES[LISTEN.mode](L.orb.size, Math.max(0, t - ACT3.form) * LISTEN.speed, LISTEN.opts).dots.map(d => [d.i, d]));

  function step(t) {
    const L = layout(W, H);
    if (t < ACT3.form) {
      // The ball is kinematic: positions straight from the hop and twirl, velocity from the change.
      const b = ballAt(t);
      sim.p.forEach((q, i) => {
        const [x, y] = project(sphere[i], b);
        q.vx = (x - q.x) / STEP;
        q.vy = (y - q.y) / STEP;
        q.x = x;
        q.y = y;
      });
      return;
    }
    const frame = orbFrame(t, L), P = home(W, H);
    sim.p.forEach((q, i) => {
      // A ripple: dots set off a little apart at each move, so the swarm flows rather than jumps.
      const lag = jitter[i] * 0.22;
      let tt = t;
      if (t >= ACT3.left && t < ACT3.left + lag) tt = ACT3.left - 0.001;
      if (t >= ACT3.finish && t < ACT3.finish + lag) tt = ACT3.finish - 0.001;
      if (t >= ACT3.fall && t < ACT3.fall + lag * 1.6) tt = ACT3.fall - 0.001;
      const T = target(i, tt, frame, L);
      if (T) {
        // A mass on a spring, a little under-damped: it overshoots slightly and settles.
        const k = T.k * 4, c = 2 * Math.sqrt(k) * T.z;
        q.vx += (k * (T.x - q.x) - c * q.vx) * STEP;
        q.vy += (k * (T.y - q.y) - c * q.vy) * STEP;
      } else {
        // Falling to the pill: gravity, steered sideways to its place in the ball, one bounce off
        // the ground, then held there by a stiff spring.
        const [tx, ty] = ballSlot(i, P);
        q.vx += (60 * (tx - q.x) - 9 * q.vx) * STEP;
        if (!q.bounced) {
          q.vy += 2600 * STEP;
          if (q.y >= ty && q.vy > 0) { q.y = ty; q.vy *= -0.32; q.bounced = true; }
        } else {
          q.vy += (900 * (ty - q.y) - 40 * q.vy) * STEP;
        }
      }
      q.x += q.vx * STEP;
      q.y += q.vy * STEP;
    });
  }

  function advance(t) {
    if (!sim || t < sim.t) {
      const b = ballAt(ACT3.take);
      sim = { t: ACT3.take, p: sphere.map(v => { const [x, y] = project(v, b); return { x, y, vx: 0, vy: 0, bounced: false }; }) };
    }
    while (sim.t + STEP <= t) {
      sim.t += STEP;
      step(sim.t);
    }
  }

  function edge(R, u) {
    const rad = Math.min(R.r, R.w / 2, R.h / 2), sw = R.w - 2 * rad, sh = R.h - 2 * rad, arc = (Math.PI / 2) * rad;
    const segs = [sw, arc, sh, arc, sw, arc, sh, arc];
    let d = (u % 1) * segs.reduce((a, b) => a + b, 0);
    const Lx = R.x + rad, Rx = R.x + R.w - rad, T = R.y + rad, B = R.y + R.h - rad;
    const corner = (ox, oy, a0, f) => [ox + rad * Math.cos(a0 + (f * Math.PI) / 2), oy + rad * Math.sin(a0 + (f * Math.PI) / 2)];
    const parts = [f => [Lx + f * sw, R.y], f => corner(Rx, T, -Math.PI / 2, f), f => [R.x + R.w, T + f * sh], f => corner(Rx, B, 0, f), f => [Rx - f * sw, R.y + R.h], f => corner(Lx, B, Math.PI / 2, f), f => [R.x, B - f * sh], f => corner(Lx, T, Math.PI, f)];
    for (let k = 0; k < 8; k++) {
      if (d <= segs[k] || k === 7) return parts[k](segs[k] ? Math.min(1, d / segs[k]) : 0);
      d -= segs[k];
    }
  }

  /** The window at t: it grows out from behind the orb, and at the end shrinks back into it. */
  function windowAt(t, L) {
    const o = orbHome(t < ACT3.finish ? Math.min(t, ACT3.left - 0.01) : t, L), s = L.orb.size * 0.62;
    const small = { x: o.x - s, y: o.y - s, w: s * 2, h: s * 2, r: s };
    const grow = out3(span(t, ACT3.card, ACT3.carded)) * (1 - inOut(span(t, ACT3.shrink, ACT3.shrunk)));
    const R = { x: lerp(small.x, L.card.x, grow), y: lerp(small.y, L.card.y, grow), w: lerp(small.w, L.card.w, grow), h: lerp(small.h, L.card.h, grow), r: lerp(small.r, L.card.r, grow) };
    const fill = ease(span(t, ACT3.fill, ACT3.filled)) * (1 - ease(span(t, ACT3.shrink, ACT3.shrunk - 0.15)));
    return { R, grow, fill };
  }

  /** Draws at film time t; returns the window's rect and fill for the page's own window element. */
  function draw(t) {
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, W, H);
    if (t < ACT3.take) return { fill: 0 };
    const L = layout(W, H);
    advance(t);
    const win = windowAt(t, L);

    // The window's outline in dots while it grows out (and shrinks back): behind the orb.
    const outline = t < ACT3.card ? 0 : (1 - win.fill) * Math.min(1, win.grow * 4);
    if (outline > 0.01) {
      const per = 2 * (win.R.w + win.R.h), count = Math.round(per / 5);
      g.fillStyle = light ? `rgba(29,29,31,${outline * 0.6})` : `rgba(238,241,255,${outline * 0.8})`;
      for (let i = 0; i < count; i++) {
        const [x, y] = edge(win.R, i / count + (1 - win.grow) * 0.15);
        g.beginPath();
        g.arc(x, y, 0.9, 0, TAU);
        g.fill();
      }
    }

    // The ball's disc: under the ball until it flows into the orb, and again once it has landed.
    const P = home(W, H);
    const disc = (cx, cy, rx, ry, a) => {
      g.save();
      g.globalAlpha = a;
      g.beginPath();
      g.ellipse(cx, cy, rx, ry, 0, 0, TAU);
      g.fillStyle = 'rgba(0,0,0,0.78)';
      g.shadowColor = 'rgba(0,0,0,0.3)';
      g.shadowBlur = 16;
      g.shadowOffsetY = 4;
      g.fill();
      g.shadowColor = 'transparent';
      g.strokeStyle = 'rgba(255,255,255,0.14)';
      g.lineWidth = 1;
      g.stroke();
      g.restore();
    };
    const discIn = 1 - ease(span(t, ACT3.form - 0.05, ACT3.form + 0.3));
    if (discIn > 0.01) { const b = ballAt(Math.min(t, ACT3.form)); disc(b.cx, b.cy, r * 1.15 * b.sx, r * 1.15 * b.sy, discIn); }
    const discHome = ease(span(t, ACT3.landed - 0.5, ACT3.landed));
    if (discHome > 0.01) disc(P.x, P.y, P.r + 1.5, P.r + 1.5, discHome);

    // The dots: their look follows their life (ball, orb, falling, home).
    const frame = orbFrame(t, L);
    const b = ballAt(Math.min(t, ACT3.form));
    const kOrb = out3(span(t, ACT3.form, ACT3.form + 0.6));
    const kHome = ease(span(t, ACT3.fall + 0.6, ACT3.landed));
    // Over the window on a light screen the dots are ink; over the desktop they're light, shadowed.
    const onWindow = light ? win.fill : 0;
    const col = [Math.round(lerp(238, 29, onWindow)), Math.round(lerp(241, 29, onWindow)), Math.round(lerp(255, 31, onWindow))].join(',');
    const pts = sim.p.map((q, i) => {
      const [, , bz] = project(sphere[i], b);
      const d = frame.get(i);
      let rad = lerp(lerp(1.0, 1.7, bz), d ? d.r : 0.4, kOrb);
      let a = lerp(lerp(0.28, 1, bz), d ? (d.a ?? 1) * clamp((1 - d.white) * 1.4) : 0, kOrb);
      // Every dot falls home; the idle ball then shows 90 of them.
      if (t >= ACT3.fall - 0.4) a = lerp(a, Math.max(a, 0.6), span(t, ACT3.fall - 0.4, ACT3.fall));
      if (t >= ACT3.fall) { rad = lerp(rad, 0.55, kHome); a = lerp(a, i < BALL ? 0.9 : 0, kHome); }
      return { x: q.x, y: q.y, vx: q.vx, vy: q.vy, z: d && kOrb > 0.5 && t < ACT3.fall ? d.z : bz, r: rad, a };
    });
    pts.sort((p, q) => p.z - q.z);
    const overDesktop = win.fill < 0.5;
    for (const p of pts) {
      if (p.a < 0.02) continue;
      if (light && overDesktop && t >= ACT3.form) {
        g.fillStyle = `rgba(0,0,0,${0.22 * p.a})`;
        g.beginPath();
        g.arc(p.x + 0.8, p.y + 1.4, Math.max(0.6, p.r * 1.6), 0, TAU);
        g.fill();
      }
      const v = Math.hypot(p.vx, p.vy);
      if (v > 120 && t >= ACT3.form) {
        // Motion blur: a short streak behind a fast dot, ~16 ms of its travel.
        const len = Math.min(v * 0.016, 26), bx = p.x - (p.vx / v) * len, by = p.y - (p.vy / v) * len;
        const grad = g.createLinearGradient(bx, by, p.x, p.y);
        grad.addColorStop(0, `rgba(${col},0)`);
        grad.addColorStop(1, `rgba(${col},${p.a})`);
        g.strokeStyle = grad;
        g.lineWidth = p.r * 1.6;
        g.lineCap = 'round';
        g.beginPath();
        g.moveTo(bx, by);
        g.lineTo(p.x, p.y);
        g.stroke();
      }
      g.fillStyle = `rgba(${col},${p.a})`;
      g.beginPath();
      g.arc(p.x, p.y, Math.max(0.3, p.r), 0, TAU);
      g.fill();
    }
    return { fill: win.fill, rect: win.R };
  }

  size();
  return { draw, size };
}
