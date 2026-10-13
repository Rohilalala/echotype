if (!window.__TAURI__) await import('./mock.js');
const { listen } = window.__TAURI__.event;
const pill = document.getElementById('pill'), label = document.getElementById('label');
const chipEl = document.getElementById('chip'), time = document.getElementById('time');
const bars = [...pill.querySelectorAll('.wave i')];
const WEIGHT = [0.35, 0.55, 0.75, 0.9, 1, 0.95, 1, 0.9, 0.75, 0.55, 0.35];
const CHIP = { too_quiet: 'Too quiet, move the mic closer', quiet: 'Quiet voice', no_cleanup: 'No cleanup' };
const still = matchMedia('(prefers-reduced-motion: reduce)').matches;
let smooth = 0, since = 0, clock;

// One primary line from the state; the chip (already ranked in Rust) is the only extra.
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
  time.textContent = `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

listen('dictation-state', ({ payload: p }) => {
  const wasRecording = pill.dataset.state === 'recording';
  pill.dataset.state = p.state;
  pill.classList.toggle('on', p.state !== 'idle');
  pill.classList.toggle('locked', !!p.locked);
  if (p.state !== 'recording') pill.classList.remove('muted');
  const text = primary(p);
  label.textContent = text;
  chipEl.hidden = !p.chip;
  chipEl.dataset.chip = p.chip || '';
  chipEl.textContent = CHIP[p.chip] || '';
  pill.title = [text, CHIP[p.chip]].filter(Boolean).join(' · ');
  if (p.state === 'recording' && !wasRecording) { since = Date.now(); tick(); clock = setInterval(tick, 250); }
  if (p.state !== 'recording') { clearInterval(clock); bars.forEach(b => (b.style.transform = '')); smooth = 0; }
});

// ~20 Hz. `meter` is 0..1 above the room's own noise floor, so a whisper still moves the bars.
listen('audio-level', ({ payload }) => {
  if (pill.dataset.state !== 'recording') return;
  const muted = !!payload.muted;
  if (muted !== pill.classList.contains('muted')) {
    pill.classList.toggle('muted', muted);
    label.textContent = muted ? 'Is your mic muted?' : '';
  }
  smooth = smooth * 0.45 + Math.max(0, Math.min(1, payload.meter)) * 0.55;
  bars.forEach((b, i) => {
    const jitter = still ? 1 : 0.75 + Math.random() * 0.5;
    b.style.transform = `scaleY(${Math.max(0.12, Math.min(1, smooth * WEIGHT[i] * jitter)).toFixed(3)})`;
  });
});
