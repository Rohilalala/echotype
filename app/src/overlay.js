  if (!window.__TAURI__) await import('./mock.js');
  const { listen } = window.__TAURI__.event;
  const pill = document.getElementById('pill'), label = document.getElementById('label');
  const bars = [...pill.querySelectorAll('.meter i')];
  const WEIGHT = [0.5, 0.75, 0.95, 1, 0.9, 0.7, 0.5];
  const TEXT = { recording: 'Listening', transcribing: 'Transcribing', cleaning: 'Cleaning up', done: 'Pasted' };
  let smooth = 0;

  listen('dictation-state', ({ payload: p }) => {
    pill.dataset.state = p.state;
    pill.classList.toggle('on', p.state !== 'idle');
    label.textContent = p.state === 'error' ? p.error || 'Something went wrong' : p.copied ? 'Copied' : TEXT[p.state] ?? '';
    if (p.state !== 'recording') bars.forEach(b => (b.style.transform = ''));
  });

  // ~20 Hz. Square root lifts quiet speech; the CSS transition fills the gaps between events.
  listen('audio-level', ({ payload }) => {
    if (pill.dataset.state !== 'recording') return;
    smooth = smooth * 0.4 + Math.sqrt(Math.max(0, Math.min(1, payload.level))) * 0.6;
    bars.forEach((b, i) => {
      const s = Math.max(0.14, Math.min(1, smooth * WEIGHT[i] * (0.7 + Math.random() * 0.6)));
      b.style.transform = `scaleY(${s.toFixed(3)})`;
    });
  });
