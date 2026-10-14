if (!window.__TAURI__) await import('./mock.js');
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;
const pill = document.getElementById('pill'), label = document.getElementById('label');
const chipEl = document.getElementById('chip'), time = document.getElementById('time'), secs = document.getElementById('secs');
const bars = [...pill.querySelectorAll('.wave i')];
const WEIGHT = [0.35, 0.55, 0.75, 0.9, 1, 0.95, 1, 0.9, 0.75, 0.55, 0.35];
const CHIP = { too_quiet: 'Too quiet, move the mic closer', quiet: 'Quiet voice', no_cleanup: 'No cleanup' };
const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
let smooth = 0, since = 0, clock;
// `cur`: the last dictation state. `view`: what pill.rs says to show (idle bar or that state,
// hover, the hint); null in the browser preview, where the state alone decides.
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

function tick() {
  const s = Math.floor((Date.now() - since) / 1000);
  if (cur.state === 'recording') time.textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  // D11: transcribing and cleaning show their seconds once they pass 3 (outside the live region).
  else secs.textContent = s >= 3 ? `${s}s` : '';
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
  const text = !active ? (view?.expand ? view.hint : '') : muted ? 'Is your mic muted?' : primary(cur);
  label.textContent = text;
  chipEl.hidden = !active || !cur.chip;
  chipEl.dataset.chip = cur.chip || '';
  chipEl.textContent = CHIP[cur.chip] || '';
  const full = [text, active && CHIP[cur.chip]].filter(Boolean).join(' · ');
  pill.title = full;
  pill.setAttribute('aria-label', active ? full || 'Dictating' : 'Start dictation');
}

listen('dictation-state', ({ payload: p }) => {
  const was = cur.state;
  cur = p;
  // A new state shows at once; pill.rs confirms (or ends it) with its next `pill` event.
  if (view && p.state !== 'idle') view = { ...view, active: true };
  if (p.state !== 'recording') muted = false;
  if (p.state !== was) {
    clearInterval(clock);
    since = Date.now();
    secs.textContent = '';
    if (['recording', 'transcribing', 'cleaning'].includes(p.state)) clock = setInterval(tick, 250);
  }
  render();
  if (p.state === 'recording') tick();
  else { bars.forEach(b => (b.style.transform = '')); smooth = 0; }
});

listen('pill', ({ payload }) => { view = payload; render(); });

// ~20 Hz. `meter` is 0..1 above the room's own noise floor, so a whisper still moves the bars.
listen('audio-level', ({ payload }) => {
  if (cur.state !== 'recording') return;
  if (!!payload.muted !== muted) { muted = !!payload.muted; render(); }
  smooth = smooth * 0.45 + Math.max(0, Math.min(1, payload.meter)) * 0.55;
  bars.forEach((b, i) => {
    const jitter = still ? 1 : 0.75 + Math.random() * 0.5;
    b.style.transform = `scaleY(${Math.max(0.12, Math.min(1, smooth * WEIGHT[i] * jitter)).toFixed(3)})`;
  });
});

// A click counts only when the button goes down and up on the capsule (D1). pill.rs lets the
// mouse through to this window only over the capsule's hit area (48x14 around the 4 px idle
// bar), so anywhere in the window counts as the capsule; never focus, so the app being
// dictated into keeps it. Pressing anywhere but ✕/✓ may start a drag, which pill.rs runs (and
// whose mouse-up it does not count as a click); a double-click puts the pill back.
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
