if (!window.__TAURI__) await import('./mock.js');
import { step, angle } from './needle.js';
const { listen } = window.__TAURI__.event;
const { invoke } = window.__TAURI__.core;
const pill = document.getElementById('pill'), label = document.getElementById('label');
const chipEl = document.getElementById('chip'), time = document.getElementById('time'), secs = document.getElementById('secs');
const ctext = document.getElementById('ctext'), ckey = document.getElementById('ckey'), copyBtn = document.getElementById('copy'), ring = document.getElementById('ringLeft');
const ccap = document.getElementById('ccap'), needleEl = document.getElementById('needle'), mtime = document.getElementById('mtime');
const CHIP = { too_quiet: 'Too quiet, move the mic closer', quiet: 'Quiet voice', no_cleanup: 'No cleanup' };
const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
let since = 0, clock, meetClock;
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

// "12:34", or "1:02:03" past the hour.
function clockText(ms) {
  const s = Math.max(0, Math.floor(ms / 1000)), h = Math.floor(s / 3600), m = Math.floor(s / 60) % 60, ss = String(s % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${ss}` : `${m}:${ss}`;
}

// The needle follows the level on every frame while listening (needle.js has the ballistics).
let needle = { x: 0, v: 0 }, level = 0, raf = 0, last = 0;
function frame(t) {
  needle = step(needle, level, Math.min(0.05, Math.max(0, (t - last) / 1000)), still);
  last = t;
  needleEl.style.transform = `rotate(${angle(needle.x)}deg)`;
  raf = requestAnimationFrame(frame);
}
function needleOn(on) {
  if (on && !raf) {
    last = performance.now();
    raf = requestAnimationFrame(frame);
  } else if (!on && raf) {
    cancelAnimationFrame(raf);
    raf = 0;
    needle = { x: 0, v: 0 };
    level = 0;
    needleEl.style.transform = '';
  }
}

function tick() {
  const s = Math.floor((Date.now() - since) / 1000);
  if (cur.state === 'recording') time.textContent = clockText(Date.now() - since);
  // D11: transcribing and cleaning show their seconds once they pass 3 (outside the live region).
  else secs.textContent = s >= 3 ? `${s}s` : '';
}

function render() {
  const active = cur.state !== 'idle' && (view ? view.active : true);
  // A recording meeting shows whenever no dictation state does (pill.rs sizes it the same way).
  const meeting = !active && !!view?.meeting;
  pill.classList.toggle('active', active);
  pill.classList.toggle('meeting', meeting);
  pill.classList.toggle('expand', !active && !meeting && !!view?.expand);
  pill.classList.toggle('hover', !!view?.hover);
  pill.classList.toggle('drag', !!view?.drag);
  pill.classList.toggle('locked', active && !!cur.locked);
  pill.classList.toggle('muted', active && muted);
  pill.dataset.state = active ? cur.state : 'idle';
  const card = active && !!cur.card;
  pill.classList.toggle('card', card);
  ctext.textContent = card ? cur.text || '' : '';
  // Why it is here ("Nowhere to paste", or Accessibility being off), then the paste-last keys as keycaps.
  ccap.textContent = card ? cur.warning || 'Nowhere to paste' : '';
  ckey.replaceChildren(...(card && cur.paste_key ? ['Paste last', ...cur.paste_key.split(/[ +]/).filter(Boolean).map(k => Object.assign(document.createElement('kbd'), { textContent: k }))] : []));
  clearInterval(meetClock);
  if (meeting) {
    const show = () => (mtime.textContent = clockText(Date.now() - view.meeting));
    show();
    meetClock = setInterval(show, 1000);
  }
  const text = active ? (muted ? 'Is your mic muted?' : primary(cur)) : meeting ? 'Meeting' : view?.expand ? view.hint : '';
  label.textContent = text;
  chipEl.hidden = !active || !cur.chip;
  chipEl.dataset.chip = cur.chip || '';
  chipEl.textContent = CHIP[cur.chip] || '';
  const full = card ? `${ccap.textContent}: ${cur.text || ''}` : meeting ? 'Meeting notes are recording. Click to open Meetings, right-click to stop.' : [text, active && CHIP[cur.chip]].filter(Boolean).join(' · ');
  pill.title = full;
  pill.setAttribute('aria-label', active || meeting ? full || 'Dictating' : 'Start dictation');
}

listen('dictation-state', ({ payload: p }) => {
  const was = cur.state;
  cur = p;
  // A new state shows at once; pill.rs confirms (or ends it) with its next `pill` event.
  if (view && p.state !== 'idle') view = { ...view, active: true };
  if (p.state !== 'recording') muted = false;
  if (p.card) {
    // A new card: the ring starts over and Copy says Copy again.
    copyBtn.textContent = 'Copy';
    ring.style.animation = 'none';
    void ring.getBoundingClientRect();
    ring.style.animation = '';
  }
  if (p.state !== was) {
    clearInterval(clock);
    since = Date.now();
    secs.textContent = '';
    if (['recording', 'transcribing', 'cleaning'].includes(p.state)) clock = setInterval(tick, 250);
  }
  render();
  if (p.state === 'recording') tick();
  needleOn(p.state === 'recording');
});

listen('pill', ({ payload }) => { view = payload; render(); });

// ~20 Hz. `meter` is 0..1 above the room's own noise floor, so a whisper still moves the needle.
listen('audio-level', ({ payload }) => {
  if (cur.state !== 'recording') return;
  if (!!payload.muted !== muted) { muted = !!payload.muted; render(); }
  level = Math.max(0, Math.min(1, payload.meter));
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
  if (act === 'copy') return invoke('pill_copy').then(() => (copyBtn.textContent = 'Copied'), () => {});
  if (act === 'close') return invoke('pill_dismiss');
  if (pill.classList.contains('card')) return; // the card's text is to read; even a double-click there leaves the pill where it is
  if (target === pill && e.detail === 2) return invoke('pill_reset');
  const active = pill.classList.contains('active');
  if (act === 'cancel') invoke('pill_cancel');
  else if (pill.classList.contains('meeting')) invoke('pill_open', { page: 'meetings' });
  else if (active && cur.action && cur.state !== 'recording') invoke('pill_open');
  else if (!active || ['recording', 'done', 'hint'].includes(cur.state)) invoke('pill_click');
});
addEventListener('contextmenu', e => {
  e.preventDefault();
  if (!pill.classList.contains('active')) invoke('pill_menu');
});
