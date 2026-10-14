if (!window.__TAURI__) await import('./mock.js');
import { resolvePreset, MODE_FRAMES } from './vendor/thinking-orbs.js';
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;
const pill = document.getElementById('pill'), label = document.getElementById('label');
const chipEl = document.getElementById('chip');
const creason = document.getElementById('creason'), fixBtn = document.getElementById('fix');
const ctext = document.getElementById('ctext'), ckey = document.getElementById('ckey'), copyBtn = document.getElementById('copy'), ring = document.getElementById('ringLeft');
const laterBtn = document.getElementById('later');
const CHIP = { too_quiet: 'Too quiet, move the mic closer', quiet: 'Quiet voice', no_cleanup: 'No cleanup' };
const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
// `cur`: the last dictation state. `view`: what pill.rs says to show (idle ball or that state,
// hover, near, the hint); null in the browser preview, where the state alone decides.
let cur = { state: 'idle' }, view = null, muted = false;

// One primary line from the state; the chip (already ranked in Rust) is the only extra.
// pill.rs `line()` sizes the hit area from the same words.
function primary(p) {
  if (p.state === 'error') return (p.error || 'Something went wrong').split('\n')[0];
  if (p.state === 'hint') return p.message || '';
  if (p.state === 'done') return p.copied ? 'Copied, ⌘V to paste' : 'Pasted';
  if (p.state === 'transcribing') return 'Transcribing…';
  if (p.state === 'cleaning') return 'Cleaning up…';
  return '';
}

function reason(w) {
  if (!w || w === 'Nowhere to paste') return 'No text field selected';
  return w.split('\n')[0];
}

function render() {
  const active = cur.state !== 'idle' && (view ? view.active : true);
  pill.classList.toggle('active', active);
  pill.classList.toggle('expand', !active && !!view?.expand);
  pill.classList.toggle('hover', !!view?.hover);
  pill.classList.toggle('drag', !!view?.drag);
  pill.classList.toggle('locked', active && !!cur.locked);
  pill.classList.toggle('muted', active && muted);
  pill.dataset.state = active ? cur.state : 'idle';
  const card = active && !!cur.card;
  pill.classList.toggle('card', card);
  // Listening and thinking are the orb; a plain paste falls straight back into the ball.
  pill.classList.toggle('orb', active && !muted && ['recording', 'transcribing', 'cleaning'].includes(cur.state));
  pill.classList.toggle('collapse', active && cur.state === 'done' && !card && !cur.copied && !cur.chip);
  ctext.textContent = card ? cur.text || '' : '';
  // Why the words are here and not in the app: Accessibility (fixable here) or nothing to type into.
  const fixable = card && cur.action === 'accessibility';
  creason.textContent = !card ? '' : fixable ? 'Accessibility is off, can’t paste' : reason(cur.warning);
  creason.classList.toggle('fixable', fixable);
  fixBtn.hidden = !fixable;
  ckey.textContent = card && cur.paste_key ? `Paste last: ${cur.paste_key}` : '';
  const text = !active ? (view?.expand ? view.hint : '') : muted ? 'Is your mic muted?' : primary(cur);
  label.textContent = text;
  chipEl.hidden = !active || !cur.chip;
  chipEl.dataset.chip = cur.chip || '';
  chipEl.textContent = CHIP[cur.chip] || '';
  const full = card ? `${creason.textContent}: ${cur.text || ''}` : [text, active && CHIP[cur.chip]].filter(Boolean).join(' · ');
  pill.title = full;
  pill.setAttribute('aria-label', active ? full || 'Dictating' : 'Start dictation');
  kick();
}

listen('dictation-state', ({ payload: p }) => {
  cur = p;
  // A new state shows at once; pill.rs confirms (or ends it) with its next `pill` event.
  if (view && p.state !== 'idle') view = { ...view, active: true };
  if (p.state !== 'recording') muted = false;
  if (p.card) {
    // A new card: the ring starts over and Copy says Copy again. A detected call's offer
    // (calls.rs) puts Not now and Start notes (or Stop notes) there instead, for 15 s (pill.rs OFFER_FOR).
    copyBtn.textContent = p.offer || 'Copy';
    copyBtn.dataset.act = p.offer ? 'offer' : 'copy';
    laterBtn.hidden = !p.offer;
    ring.style.animation = 'none';
    void ring.getBoundingClientRect();
    ring.style.animation = '';
    ring.style.animationDuration = p.offer ? '15s' : '';
  }
  render();
});

listen('pill', ({ payload }) => {
  view = payload;
  document.documentElement.style.setProperty('--ball', view.ball ?? 1);
  render();
});

// ~20 Hz. `meter` is 0..1 above the room's own noise floor, so a whisper still moves the orb.
let meter = 0;
listen('audio-level', ({ payload }) => {
  if (cur.state !== 'recording') return;
  if (!!payload.muted !== muted) { muted = !!payload.muted; render(); }
  meter = Math.max(0, Math.min(1, payload.meter));
});

// ---------- the body: one swarm of particles that becomes every shape ----------

// The same particles are the ball (packed tight), the orb (each one a thinking-orbs dot, by id),
// and the outline of the capsule or card; each springs to its next place, so ball → orb →
// listening/thinking → card → ball is one continuous morph. A dark backing chases .pill's box
// underneath so dots and words read on any wallpaper.
const cvs = document.getElementById('body'), g = cvs.getContext('2d');
const W = 260, H = 100, ORB = 36, N = 600;
const LISTEN = resolvePreset('listening', 64), THINK = resolvePreset('working', 64);
// Dots visible in the ball and along an edge; the rest wait, unseen, on those same spots.
const BALL_DOTS = 90, EDGE_GAP = 4;
// The window shows this page zoomed 1.5x (pill.rs ZOOM): draw at 3x so the dots stay crisp.
const dpr = Math.max(3, devicePixelRatio || 1);
cvs.width = W * dpr;
cvs.height = H * dpr;
const box = { x: W / 2 - 5, y: H - 21, w: 10, h: 10, r: 5 };
const hash = i => { const h = Math.sin(i * 12.9898) * 43758.5453; return h - Math.floor(h); };
// Per particle: position, velocity, look, and its own stiffness (a little uneven, so the swarm flows).
const P = Array.from({ length: N }, (_, i) => ({ x: W / 2, y: H - 16, vx: 0, vy: 0, r: 0.4, a: 0, w: 0.1, k: 380 + 260 * hash(i + 1) }));
let alpha = 0.35, level = 0, clockL = 0, clockT = 0, last = 0, raf = 0, prevOrb = null, helloAt = -1e9;

const follow = (v, to, dt, ms) => (still ? to : v + (to - v) * (1 - Math.exp(-dt / ms)));

function kick() {
  if (!raf) { last = performance.now(); raf = requestAnimationFrame(frame); }
}

// A point `u` (0..1) around the edge of rect r with corner radius rad, clockwise from the top.
function edge(r, rad, u) {
  rad = Math.max(0, Math.min(rad, r.w / 2, r.h / 2));
  const sw = r.w - 2 * rad, sh = r.h - 2 * rad, arc = (Math.PI / 2) * rad;
  const segs = [sw, arc, sh, arc, sw, arc, sh, arc];
  let d = (u % 1) * segs.reduce((x, y) => x + y, 0);
  const corner = (ox, oy, a0, f) => [ox + rad * Math.cos(a0 + f * Math.PI / 2), oy + rad * Math.sin(a0 + f * Math.PI / 2)];
  const L = r.x + rad, R = r.x + r.w - rad, Tp = r.y + rad, B = r.y + r.h - rad;
  const parts = [
    f => [L + f * sw, r.y], f => corner(R, Tp, -Math.PI / 2, f),
    f => [r.x + r.w, Tp + f * sh], f => corner(R, B, 0, f),
    f => [R - f * sw, r.y + r.h], f => corner(L, B, Math.PI / 2, f),
    f => [r.x, B - f * sh], f => corner(L, Tp, Math.PI, f)
  ];
  for (let k = 0; k < 8; k++) {
    if (d <= segs[k] || k === 7) return parts[k](segs[k] ? Math.min(1, d / segs[k]) : 0);
    d -= segs[k];
  }
}

// Where particle i goes now: [x, y, radius, alpha, ink (0 bright .. 1 dark on this substrate)].
function targets(kind, cx, cy) {
  const T = new Array(N);
  if (kind === 'orb') {
    const p = cur.state === 'recording' ? LISTEN : THINK;
    const t = cur.state === 'recording' ? clockL : clockT;
    const s = 1 + 0.22 * level;
    const { dots } = MODE_FRAMES[p.mode](ORB, t, p.opts);
    for (const d of dots) if (d.i < N) T[d.i] = [cx + (d.x - ORB / 2) * s, cy + (d.y - ORB / 2) * s, d.r * Math.min(1.15, s), d.a ?? 1, d.white, d.z];
    // Ids this orb does not use gather, unseen, on one it does.
    for (let i = 0; i < N; i++) if (!T[i]) { const o = dots[i % dots.length]; T[i] = [cx + (o.x - ORB / 2) * s, cy + (o.y - ORB / 2) * s, 0.3, 0, 0.2, -9]; }
    return T;
  }
  if (kind === 'ball') {
    // A sunflower disc filling the ball.
    const R = Math.max(1, Math.min(box.w, box.h) / 2 - 0.8);
    for (let i = 0; i < N; i++) {
      const j = i % BALL_DOTS, rr = R * Math.sqrt((j + 0.5) / BALL_DOTS), th = j * 2.39996;
      T[i] = [cx + rr * Math.cos(th), cy + rr * Math.sin(th), 0.45, i < BALL_DOTS ? 0.9 : 0, 0.12, 0];
    }
    return T;
  }
  // The capsule's or card's edge, a dot every EDGE_GAP points, just inside the backing.
  const inset = { x: box.x + 1.5, y: box.y + 1.5, w: box.w - 3, h: box.h - 3 };
  const n = Math.max(8, Math.round((2 * (inset.w + inset.h)) / EDGE_GAP));
  for (let i = 0; i < N; i++) {
    const [x, y] = edge(inset, box.r - 1.5, (i % n) / n);
    T[i] = [x, y, 0.45, i < n ? 0.55 : 0, 0.25, 0];
  }
  return T;
}

function frame(now) {
  // rAF's timestamp can be a little earlier than the performance.now() kick() started from.
  const dt = Math.max(0, Math.min(64, now - last));
  last = now;
  const b = pill.getBoundingClientRect();
  const orb = pill.classList.contains('orb'), card = pill.classList.contains('card');
  const idle = !pill.classList.contains('active') && !pill.classList.contains('expand');
  const ball = idle || pill.classList.contains('collapse');
  for (const [k, to] of [['x', b.x], ['y', b.y], ['w', b.width], ['h', b.height], ['r', card ? 10 : Math.min(b.height / 2, b.width / 2, 18)]]) box[k] = follow(box[k], to, dt, 70);
  // Idle, the ball fades back unless the cursor is near it (or holding it).
  const hello = now - helloAt < 1400; // onboarding just finished: the ball says hello at full strength
  const want = idle && view && view.fade !== false && !view.near && !hello ? 0.35 : 1;
  alpha = follow(alpha, want, dt, 140);
  level = follow(level, cur.state === 'recording' ? meter : 0, dt, 70);
  if (!still) {
    clockL += (dt / 1000) * LISTEN.speed * (1 + 1.6 * level);
    clockT += (dt / 1000) * THINK.speed;
  }
  // Recording and thinking orbs are centred on the body; while listening ✕ and ✓ widen it.
  const cx = box.x + box.w / 2, cy = box.y + box.h / 2;
  const kind = orb ? 'orb' : ball ? 'ball' : 'edge';
  const T = targets(kind, cx, cy);
  if (orb !== prevOrb) {
    // Bursting out of the ball: a kick outward so the dots explode rather than slide.
    if (orb && prevOrb === false && !still) for (const q of P) { const dx = q.x - cx, dy = q.y - cy, d = Math.hypot(dx, dy) || 1; q.vx += (dx / d) * 120; q.vy += (dy / d) * 120; }
    prevOrb = orb;
  }
  const s = dt / 1000;
  let moving = false;
  for (let i = 0; i < N; i++) {
    const q = P[i], [tx, ty, tr, tap, tw] = T[i];
    if (still) { q.x = tx; q.y = ty; q.vx = q.vy = 0; }
    else {
      // Slightly under-damped: a touch of overshoot, then still.
      const c = 2 * Math.sqrt(q.k) * 0.62;
      q.vx += (q.k * (tx - q.x) - c * q.vx) * s;
      q.vy += (q.k * (ty - q.y) - c * q.vy) * s;
      q.x += q.vx * s;
      q.y += q.vy * s;
    }
    // An edge is only drawn by its dots on the way there: once one lands it fades into the outline.
    const landed = kind === 'edge' && Math.hypot(q.x - tx, q.y - ty) < 1.5 && Math.abs(q.vx) + Math.abs(q.vy) < 12;
    const ta = landed ? 0 : tap;
    q.r = follow(q.r, tr, dt, 90);
    q.a = follow(q.a, ta, dt, 110);
    q.w = follow(q.w, tw, dt, 120);
    if (Math.abs(q.vx) + Math.abs(q.vy) > 0.5 || Math.abs(q.a - ta) > 0.01 || Math.hypot(q.x - tx, q.y - ty) > 0.2) moving = true;
  }

  g.setTransform(dpr, 0, 0, dpr, 0, 0);
  g.clearRect(0, 0, W, H);
  g.globalAlpha = alpha;
  g.beginPath();
  g.roundRect(box.x, box.y, Math.max(0, box.w), Math.max(0, box.h), Math.max(0, box.r));
  g.shadowColor = 'rgba(0,0,0,0.16)';
  g.shadowBlur = idle ? 4 : 8;
  g.shadowOffsetY = idle ? 1 : 2;
  g.fillStyle = 'rgba(0,0,0,0.75)';
  g.fill();
  g.shadowColor = 'transparent';
  g.lineWidth = 1;
  g.strokeStyle = 'rgba(255,255,255,0.14)';
  g.stroke();
  // Far dots first (the orb's depth); everything else in pool order.
  const order = orb ? [...P.keys()].sort((i, j) => T[i][5] - T[j][5]) : P.keys();
  for (const i of order) {
    const q = P[i];
    if (q.a < 0.02) continue;
    const gray = Math.round((1 - Math.min(1, Math.max(0, q.w))) * 255);
    g.fillStyle = `rgba(${gray},${gray},${gray},${q.a})`;
    g.beginPath();
    g.arc(q.x, q.y, Math.max(0.3, q.r), 0, Math.PI * 2);
    g.fill();
  }

  const settled = !orb && !hello && !moving && Math.abs(box.w - b.width) < 0.1 && Math.abs(box.h - b.height) < 0.1 && Math.abs(box.x - b.x) < 0.1 && Math.abs(box.y - b.y) < 0.1 && Math.abs(alpha - want) < 0.005;
  raf = settled ? 0 : requestAnimationFrame(frame);
}

render();

// The end of onboarding (main.js): the ball pulses once, so the eye finds it.
listen('pill-hello', () => {
  helloAt = performance.now();
  if (!still) for (const q of P) { const dx = q.x - (box.x + box.w / 2), dy = q.y - (box.y + box.h / 2), d = Math.hypot(dx, dy) || 1; q.vx += (dx / d) * 45; q.vy += (dy / d) * 45; }
  kick();
});

// A click counts only when the button goes down and up on the capsule (D1). pill.rs lets the
// mouse through to this window only over the capsule's hit area (24x22 around the idle ball),
// so anywhere in the window counts as the capsule; never focus, so the app being dictated into
// keeps it. Pressing anywhere but ✕/✓ may start a drag, which pill.rs runs (and whose mouse-up
// it does not count as a click); a double-click puts the pill back.
let downOn = null;
addEventListener('mousedown', e => {
  if (e.button !== 0) return;
  downOn = e.target.closest('[data-act]') || pill;
  if (downOn === pill) invoke('pill_drag');
});
addEventListener('mouseup', e => {
  const target = downOn;
  downOn = null;
  if (e.button !== 0 || !target || (e.target.closest('[data-act]') || pill) !== target) return;
  const act = target.dataset?.act;
  if (act === 'copy') return invoke('pill_copy').then(() => (copyBtn.textContent = 'Copied'), () => {});
  if (act === 'offer') return invoke('pill_offer');
  if (act === 'close') return invoke('pill_dismiss');
  if (act === 'fix') return invoke('pill_open'); // Home, with the Accessibility fix
  if (pill.classList.contains('card')) return; // the card's text is to read; even a double-click there leaves the pill where it is
  if (target === pill && e.detail === 2) return invoke('pill_reset');
  const active = pill.classList.contains('active');
  if (act === 'cancel') invoke('pill_cancel');
  else if (active && cur.action && cur.state !== 'recording') invoke('pill_open');
  else if (!active || ['recording', 'done', 'hint'].includes(cur.state)) invoke('pill_click');
});
addEventListener('contextmenu', e => {
  e.preventDefault();
  if (!pill.classList.contains('active')) invoke('pill_menu');
});
