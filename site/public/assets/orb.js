// Onboarding's orb: one swarm of dots on a window-sized canvas that is the ball, the listening
// orb, the thinking orb and a check mark, and springs to whatever slot the current step gives it.
// The pill's approach (overlay.js: thinking-orbs dots by id, each on its own spring), drawn in the
// window's own ink on its own paper instead of light dots on a dark backing.
import { resolvePreset, MODE_FRAMES, scaleCounts, scaleRadii } from './vendor/thinking-orbs.js';

const N = 2000; // as many as the pill, so every style and detail fits
// The same dense orb as the pill (overlay.js), so it's one object everywhere.
const dense = (p, count, size) => ({ ...p, opts: scaleRadii(scaleCounts(p.opts, count), size) });
// The default orbs at the default detail, a little denser: drawn three to four times the pill's size.
const DENSE = { listening: { rings: 13, lonDensity: 30 }, working: { orbitN: 12, ghostN: 34, ghostA: 0.85 } };
// Settings → Pill's listening and thinking styles and detail, built the way the pill builds them.
function build(listen, think, detail) {
  const L = dense(resolvePreset(listen, 64), 3 * detail, 0.62), T = dense(resolvePreset(think, 64), 1.3 * detail, 0.8);
  if (detail === 1) { L.opts = { ...L.opts, ...DENSE[listen] }; T.opts = { ...T.opts, ...DENSE[think] }; }
  return [L, T];
}
const hash = i => { const v = Math.sin(i * 12.9898) * 43758.5453; return v - Math.floor(v); };
// A check mark through these points, as fractions of the orb's size from its centre.
const CHECK = [[-0.26, 0.02], [-0.07, 0.21], [0.28, -0.19]];
const CHECK_DOTS = 38, BALL_DOTS = 64;

function along(pts, f) {
  const seg = pts.slice(1).map((p, i) => Math.hypot(p[0] - pts[i][0], p[1] - pts[i][1]));
  let d = f * seg.reduce((a, b) => a + b, 0);
  for (let i = 0; i < seg.length; i++) {
    if (d <= seg[i] || i === seg.length - 1) {
      const t = seg[i] ? Math.min(1, d / seg[i]) : 0;
      return [pts[i][0] + (pts[i + 1][0] - pts[i][0]) * t, pts[i][1] + (pts[i + 1][1] - pts[i][1]) * t];
    }
    d -= seg[i];
  }
}

/** `still`: prefers-reduced-motion. Nothing moves; the level shows as the orb's opacity. */
export function createOrb(cvs, still) {
  const g = cvs.getContext('2d');
  const P = Array.from({ length: N }, (_, i) => ({ x: 0, y: 0, vx: 0, vy: 0, r: 0, a: 0, w: 0.3, k: 300 + 260 * hash(i + 1) }));
  const box = { x: 0, y: 0, s: 0 };
  let kind = 'ball', slot = null, at = null, meter = 0, level = 0, prog = null, ring = 0, fade = 1;
  let clockL = 0, clockT = 0, last = 0, raf = 0, ink = '13,13,13', frames = 0, placed = false, dpr = 1;
  // style(): `tint` is the pill's dot colour, drawn on the pill's dark disc (`disc` fades it in and
  // out, `discR` hugs the orb or the ball); `col` eases to it. `pill`: the resting ball as the pill
  // draws it ({ size: ball_size, zoom: pill_size }), to scale; `ballR` eases between sizes.
  let [LISTEN, THINK] = build('listening', 'working', 1), tint = null, disc = 0, discR = 0, pill = null, ballR = 0;
  let inkA = [13, 13, 13], col = [13, 13, 13];
  // twirl(): one turn of the orb about its vertical axis (`spin` 0..1, null when still). One that
  // has started always finishes; changes during it queue exactly one more (`again`).
  let spin = null, again = false;

  const follow = (v, to, dt, ms) => (still ? to : v + (to - v) * (1 - Math.exp(-dt / ms)));

  // Where the orb should be: an explicit point, else the middle of the slot element.
  function place() {
    if (at) return at;
    if (!slot || !slot.isConnected || !slot.offsetParent) return null;
    const r = slot.getBoundingClientRect(), c = cvs.getBoundingClientRect();
    return { x: r.left - c.left + r.width / 2, y: r.top - c.top + r.height / 2, s: Math.min(r.width, r.height) };
  }

  // Particle i's target: [x, y, radius, alpha, depth tint (0 near .. 1 far), z].
  function targets(cx, cy) {
    const T = new Array(N), s = box.s;
    if (kind === 'orb' || kind === 'think') {
      const p = kind === 'orb' ? LISTEN : THINK;
      const size = s * (1 + 0.2 * level);
      const { dots } = MODE_FRAMES[p.mode](size, kind === 'orb' ? clockL : clockT, p.opts);
      // Depth in px for the twirl: globe, puzzle and constellation give it as -1..1, so scale those by the radius.
      let zMax = 0, xMax = 0;
      for (const d of dots) { zMax = Math.max(zMax, Math.abs(d.z)); xMax = Math.max(xMax, Math.abs(d.x - size / 2)); }
      const zk = zMax <= 1.01 ? xMax : 1;
      for (const d of dots) if (d.i < N) T[d.i] = [cx + d.x - size / 2, cy + d.y - size / 2, d.r, d.a ?? 1, d.white, d.z * zk];
      // Ids this orb doesn't use wait, unseen, on one it does.
      for (let i = 0; i < N; i++) if (!T[i]) { const o = dots[i % dots.length]; T[i] = [cx + o.x - size / 2, cy + o.y - size / 2, 0.4, 0, 0.5, -9]; }
      return T;
    }
    if (kind === 'check') {
      for (let i = 0; i < N; i++) {
        const j = i % CHECK_DOTS, [x, y] = along(CHECK, j / (CHECK_DOTS - 1));
        T[i] = [cx + x * s, cy + y * s, Math.max(1.1, s * 0.02), i < CHECK_DOTS ? 1 : 0, 0.05, 0];
      }
      return T;
    }
    // The ball: a sunflower disc, the size of the pill's resting ball (a little larger in here),
    // or exactly the pill's (overlay.js: 90 dots of 0.45 px in 10 px, times ball_size and zoom).
    const R = ballR, n = pill ? 90 : BALL_DOTS, dr = pill ? 0.45 * pill.zoom : 0.9;
    for (let i = 0; i < N; i++) {
      const j = i % n, rr = R * Math.sqrt((j + 0.5) / n), th = j * 2.39996;
      T[i] = [cx + rr * Math.cos(th), cy + rr * Math.sin(th), dr, i < n ? 1 : 0, 0.1, 0];
    }
    return T;
  }

  function frame(now) {
    const dt = Math.max(0, Math.min(64, now - last));
    last = now;
    raf = requestAnimationFrame(frame);
    // The canvas is the whole window: keep its backing store at the window's size and density.
    const w = cvs.clientWidth, h = cvs.clientHeight, d = devicePixelRatio || 1;
    if (cvs.width !== Math.round(w * d) || cvs.height !== Math.round(h * d)) { cvs.width = Math.round(w * d); cvs.height = Math.round(h * d); dpr = d; }
    if (frames++ % 30 === 0) { const m = getComputedStyle(cvs).color.match(/\d+(\.\d+)?/g); if (m) { ink = m.slice(0, 3).join(','); inkA = m.slice(0, 3).map(Number); } }
    const want = place();
    if (!want) return g.clearRect(0, 0, cvs.width, cvs.height);
    if (!placed) { Object.assign(box, want); placed = true; }
    // A point (the finish's drop) is a slower glide than following a slot, so the ball stays whole.
    box.x = follow(box.x, want.x, dt, at ? 200 : 90);
    box.y = follow(box.y, want.y, dt, at ? 200 : 90);
    box.s = follow(box.s, want.s, dt, 120);
    level = follow(level, kind === 'orb' ? meter : 0, dt, meter > level ? 30 : 150); // fast attack, slow release
    ring = follow(ring, prog ?? ring, dt, 200);
    disc = follow(disc, tint ? 1 : 0, dt, 160);
    // About 250 ms from one colour or ball size to the next; the first frame starts there.
    const first = frames === 1, ease = (v, to, ms) => (first ? to : follow(v, to, dt, ms));
    col = (tint ?? inkA).map((c, k) => ease(col[k], c, 80));
    ballR = ease(ballR, pill ? Math.max(1, (5 * pill.size - 0.8) * pill.zoom) : Math.max(7, Math.min(13, box.s * 0.1)), 80);
    discR = ease(discR, kind === 'ball' ? ballR + 0.8 * (pill?.zoom ?? 1) : box.s * 0.48, 90);
    if (!still) {
      clockL += (dt / 1000) * LISTEN.speed * 0.55 * (1 + 2 * level);
      clockT += (dt / 1000) * THINK.speed * 0.7;
    }
    const T = targets(box.x, box.y);
    const s = dt / 1000;
    for (let i = 0; i < N; i++) {
      const q = P[i], [tx, ty, tr, ta, tw] = T[i];
      if (still || frames === 1) { q.x = tx; q.y = ty; q.vx = q.vy = 0; }
      else {
        // Slightly under-damped: a touch of overshoot (about 1.06x), then still.
        const c = 2 * Math.sqrt(q.k) * 0.66;
        q.vx += (q.k * (tx - q.x) - c * q.vx) * s;
        q.vy += (q.k * (ty - q.y) - c * q.vy) * s;
        q.x += q.vx * s;
        q.y += q.vy * s;
      }
      q.r = follow(q.r, tr, dt, 90);
      q.a = follow(q.a, ta, dt, 110);
      q.w = follow(q.w, tw, dt, 120);
    }

    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, w, h);
    // Reduced motion: the level is the orb's opacity rather than its size and speed.
    const base = (still && kind === 'orb' ? 0.55 + 0.45 * Math.min(1, meter * 1.6) : 1) * fade;
    if (disc > 0.01) {
      g.fillStyle = `rgba(29,30,34,${disc * fade})`; // .swatch's disc
      g.beginPath(); g.arc(box.x, box.y, discR, 0, Math.PI * 2); g.fill();
    }
    const dot = col.map(Math.round).join(',');
    if (prog != null || ring > 0.001) {
      const R = box.s * 0.64;
      g.lineWidth = 2;
      g.lineCap = 'round';
      g.strokeStyle = `rgba(${ink},${0.12 * fade})`;
      g.beginPath(); g.arc(box.x, box.y, R, 0, Math.PI * 2); g.stroke();
      if (prog != null) {
        g.strokeStyle = `rgba(${ink},${0.8 * fade})`;
        g.beginPath(); g.arc(box.x, box.y, R, -Math.PI / 2, -Math.PI / 2 + Math.PI * 2 * Math.max(0.01, ring)); g.stroke();
      }
    }
    // The twirl turns each dot's 3D point (its x from the centre, its depth z) after the springs, so
    // the shape turns whole instead of smearing; ease-in-out over 0.9 s.
    if (spin != null) { spin += dt / 900; if (spin >= 1) { spin = again ? 0 : null; again = false; } }
    const turn = spin != null && (kind === 'orb' || kind === 'think') ? Math.PI * (1 - Math.cos(Math.PI * spin)) : 0;
    const cs = Math.cos(turn), sn = Math.sin(turn), X = [], Z = [];
    for (let i = 0; i < N; i++) { const dx = P[i].x - box.x, z = T[i][5] ?? 0; X[i] = box.x + dx * cs + z * sn; Z[i] = -dx * sn + z * cs; }
    const order = kind === 'orb' || kind === 'think' ? [...P.keys()].sort((i, j) => Z[i] - Z[j]) : P.keys();
    for (const i of order) {
      const q = P[i], a = q.a * Math.max(0.18, Math.min(1, 1.12 - q.w)) * base;
      if (a < 0.02) continue;
      g.fillStyle = `rgba(${dot},${a})`;
      g.beginPath();
      g.arc(X[i], q.y, Math.max(0.35, q.r), 0, Math.PI * 2);
      g.fill();
    }
  }

  return {
    /** Follow this element: the orb fills the largest square centred in it. */
    slot(el) { slot = el; at = null; },
    /** Go to a point instead (the finish: the ball drops out of the card). */
    point(x, y, s) { at = { x, y, s: s ?? box.s }; },
    /** 'ball' | 'orb' (listening) | 'think' | 'check'. Leaving the ball bursts outward. */
    set(k) {
      if (k === kind) return;
      if (kind === 'ball' && k !== 'ball' && !still) this.burst(0.8);
      kind = k;
    },
    get kind() { return kind; },
    level(v) { meter = Math.max(0, Math.min(1, v)); },
    /** A progress ring around the orb (0..1), or null for none. */
    progress(p) { prog = p == null ? null : Math.max(0, Math.min(1, p)); if (prog == null) ring = 0; },
    fade(v) { fade = v; },
    /** The user's orb, or null for the default in ink: { listen, think, detail, color } from
     *  settings, and `pill: { size, zoom }` to show the 'ball' at the pill's own size. The dots spring
     *  to the new style's frames; the colour and ball size ease there. */
    style(o) {
      [LISTEN, THINK] = build(o?.listen ?? 'listening', o?.think ?? 'working', o?.detail ?? 1);
      const n = o?.color ? parseInt(o.color.slice(1), 16) : NaN, was = tint;
      tint = Number.isNaN(n) ? null : [(n >> 16) & 255, (n >> 8) & 255, n & 255];
      if (was && tint && String(was) !== String(tint)) this.twirl(); // a new colour: it turns to show it
      pill = o?.pill ?? null;
    },
    /** One turn about the vertical axis. Never restarted or cut: mid-turn, one more is queued. */
    twirl() { if (still) return; if (spin == null) spin = 0; else again = true; },
    /** A kick outward from the centre: the ball bursting, a success pulse. */
    burst(force = 1) {
      if (still) return;
      const v = force * 240 * Math.max(0.6, box.s / 100);
      for (const q of P) { const dx = q.x - box.x, dy = q.y - box.y, d = Math.hypot(dx, dy) || 1; q.vx += (dx / d) * v; q.vy += (dy / d) * v; }
    },
    start() { if (!raf) { last = performance.now(); raf = requestAnimationFrame(frame); } },
    stop() { cancelAnimationFrame(raf); raf = 0; placed = false; frames = 0; g.clearRect(0, 0, cvs.width, cvs.height); },
  };
}
