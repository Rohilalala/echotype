// Echotype settings window. Talks to the Rust core through Tauri commands and events.
const preview = !window.__TAURI__;
if (preview) await import('./mock.js'); // browser preview for design QA

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

// replaceChildren without the stray "false"/"undefined" text of an absent optional child.
const fill = (el, ...kids) => el.replaceChildren(...kids.flat().filter(k => k != null && k !== false && k !== ''));
// Tiny element builder. Strings become text nodes, so user text is never parsed as HTML.
function h(tag, attrs = {}, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k in el && k !== 'list') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  el.append(...kids.flat().filter(k => k != null && k !== false));
  return el;
}
const svg = (d, w = 16) => {
  const el = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  el.setAttribute('viewBox', `0 0 ${w} ${w}`);
  el.setAttribute('aria-hidden', 'true');
  el.innerHTML = d;
  return el;
};
const CHECK = '<path d="M3.5 8.5l3 3 6-7"/>';
const CROSS = '<path d="M4 4l8 8M12 4l-8 8"/>';

let S, presets, models = [], sys, isMac = true;
let status = {}; // shortcut_status: talk key, Input Monitoring, Globe key, launch-time shortcut errors
let devices = [];
const progress = {}, modelErr = {};
let recent = [], dropResults = [], apiStatus = null, tokenShown = false;

// ---------- saving ----------
let saveTimer, savedTimer, saving = Promise.resolve();
// Saves run one after another, each sending S as it is when its turn comes.
function save(quiet) {
  clearTimeout(saveTimer);
  const run = saving.then(() => saveNow(quiet));
  saving = run;
  return run;
}
// The copy Rust last confirmed. Sent with every save so Rust applies only what this window
// changed, and used to keep unsaved local edits when `settings-changed` arrives.
let base;
const clone = o => structuredClone(o);
const eq = (a, b) => a === b || (!!a && !!b && typeof a === 'object' && typeof b === 'object'
  && Object.keys(a).length === Object.keys(b).length && Object.keys(a).every(k => eq(a[k], b[k])));
// Three-way merge: keys `mine` changed relative to `old` win, the rest come from `theirs`.
function merge3(theirs, old, mine) {
  const obj = v => v && typeof v === 'object' && !Array.isArray(v);
  const out = clone(theirs);
  for (const k of Object.keys(mine)) {
    if (obj(mine[k]) && obj(old?.[k]) && obj(out[k])) out[k] = merge3(out[k], old[k], mine[k]);
    else if (!eq(mine[k], old?.[k])) out[k] = clone(mine[k]);
  }
  return out;
}
async function saveNow(quiet) {
  const settings = { ...S, replacements: S.replacements.filter(r => r.from.trim()) };
  const sentBase = base;
  const sentCopy = base = clone(settings); // from here on these values are Rust's to confirm or correct
  try {
    await invoke('save_settings', { settings, base: sentBase });
    flash();
    return null;
  } catch (e) {
    // Nothing was saved unless Rust confirmed with settings-changed (which replaces `base`):
    // put the old base back so this edit still counts as unsaved next time.
    if (base === sentCopy) base = sentBase;
    if (!quiet) flash(String(e));
    return String(e);
  }
}
const saveSoon = () => { clearTimeout(saveTimer); saveTimer = setTimeout(save, 450); };
function flash(err) {
  const el = $('#saved');
  clearTimeout(savedTimer);
  el.classList.toggle('err', !!err);
  el.replaceChildren(...(err ? [`Couldn't save: ${err}`] : [svg(CHECK), 'Saved']));
  el.classList.add('show');
  void el.offsetWidth;
  el.classList.remove('show');
  if (!err) savedTimer = setTimeout(() => el.replaceChildren(), 2000);
}

// ---------- bound controls ----------
const get = p => p.split('.').reduce((o, k) => o?.[k], S);
function set(p, v) {
  const ks = p.split('.'), last = ks.pop();
  ks.reduce((o, k) => (o[k] ??= {}), S)[last] = v;
}
function bindAll() {
  for (const el of $$('[data-bind]')) {
    const p = el.dataset.bind;
    const text = el.tagName === 'TEXTAREA' || ['text', 'url', 'password', 'number'].includes(el.type);
    el.addEventListener(text ? 'input' : 'change', () => {
      if (el.type === 'number' && !el.checkValidity()) return;
      let v = el.type === 'checkbox' ? el.checked : el.type === 'number' ? Number(el.value) : el.value;
      if ('null' in el.dataset && v === '') v = null;
      set(p, v);
      text ? saveSoon() : save();
      changed(p);
    });
  }
}
function syncBound() {
  for (const el of $$('[data-bind]')) {
    const v = get(el.dataset.bind);
    if (el.type === 'checkbox') el.checked = !!v;
    else if (el.type === 'radio') el.checked = el.value === v;
    else el.value = v ?? '';
  }
}
function changed(p) {
  if (p === 'mode' || p === 'hotkey') renderHotkey();
  if (p === 'talk_key') saving.then(refreshStatus); // after the save, or the check re-applies the old key
  if (p === 'input_device') renderMicNote();
  if (p === 'cleanup.enabled') renderRaw();
  if (p === 'translate_to_english') renderLangNote();
  if (p === 'cleanup.enabled') { setCleanupInert(); renderCloud(); refreshCloud(); }
  if (p.startsWith('local_api')) refreshApi();
  if (p === 'history_enabled') renderHistory();
  if (p === 'pill_idle') renderPill();
  renderChips();
}

// ---------- navigation ----------
function go(page) {
  for (const s of $$('main > section')) s.hidden = s.dataset.page !== page;
  for (const b of $$('nav button')) b.toggleAttribute('aria-current', false), b.dataset.page === page && b.setAttribute('aria-current', 'page');
  $('#main').scrollTop = 0;
  if (page === 'history') renderHistory();
  if (page === 'settings') { refreshPerms(); refreshApi(); refreshStatus(); }
  if (page === 'speech' || page === 'cleanup') refreshCloud();
  history.replaceState(null, '', '#' + page);
  window.__TAURI__.event.emit?.('main-page', page); // Home shows the live state, so the pill steps aside there (D14)
}
$('#nav').addEventListener('click', e => { const b = e.target.closest('button'); if (b) go(b.dataset.page); });
$('#nav').addEventListener('keydown', e => {
  if (!['ArrowDown', 'ArrowUp'].includes(e.key)) return;
  const bs = $$('nav button'), i = bs.indexOf(document.activeElement);
  const n = bs[(i + (e.key === 'ArrowDown' ? 1 : -1) + bs.length) % bs.length];
  n.focus(); go(n.dataset.page); e.preventDefault();
});

// ---------- hotkey ----------
const MAC_KEYS = { CommandOrControl: '⌘', CmdOrCtrl: '⌘', Command: '⌘', Cmd: '⌘', Super: '⌘', Meta: '⌘', Control: '⌃', Ctrl: '⌃', Alt: '⌥', Option: '⌥', Shift: '⇧' };
const PC_KEYS = { CommandOrControl: 'Ctrl', CmdOrCtrl: 'Ctrl', Control: 'Ctrl', Super: 'Win', Meta: 'Win' };
const NAMED = { ArrowUp: '↑', ArrowDown: '↓', ArrowLeft: '←', ArrowRight: '→', Enter: '↩', Backspace: '⌫', Backquote: '`', Minus: '-', Equal: '=', Comma: ',', Period: '.', Slash: '/', Semicolon: ';', Quote: "'", BracketLeft: '[', BracketRight: ']', Backslash: '\\' };
const keyName = k => (isMac ? MAC_KEYS : PC_KEYS)[k] ?? NAMED[k] ?? k;
const keycaps = accel => accel.split('+').map(k => h('kbd', {}, keyName(k)));
const hotkeyText = accel => accel.split('+').map(keyName).join(isMac ? ' ' : '+');

function accelFrom(e) {
  const mods = [];
  if (e.metaKey) mods.push(isMac ? 'CommandOrControl' : 'Super');
  if (e.ctrlKey) mods.push(isMac ? 'Control' : 'CommandOrControl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  const c = e.code;
  if (!c || /^(Meta|Control|Alt|Shift|OS|Fn|CapsLock)/.test(c)) return { mods };
  const key = c.startsWith('Key') ? c.slice(3) : c.startsWith('Digit') ? c.slice(5) : c;
  // Shift alone is not enough: Shift+A would take capital A away from every app.
  if (!mods.some(m => m !== 'Shift') && !/^F\d+$/.test(key)) return { mods, error: 'Add a modifier such as ⌥, ⌃ or ⌘.' };
  return { mods, accel: [...mods, key].join('+') };
}

// The keys that dictate: the talk key when it is active, else the shortcut.
const TALK = { fn: ['fn'], right_option: ['Right', '⌥'], right_command: ['Right', '⌘'], right_control: ['Right', '⌃'] };
const talkCaps = () => status.talk_key_active && TALK[S.talk_key] ? [h('kbd', {}, TALK[S.talk_key].join(' '))] : keycaps(S.hotkey);
const verb = () => (S.mode === 'toggle' ? 'Press' : 'Hold');
// ponytail: macOS only. Elsewhere the paste keystroke itself would be Ctrl+V while Ctrl+Alt is still
// held (no modifier wait off macOS), re-firing the shortcut; users can bind their own there.
const DEFAULT_LAST = () => isMac ? ['CommandOrControl+Control+V', 'CommandOrControl+Control+C'] : ['', ''];

function renderHotkey() {
  for (const btn of $$('[data-shortcut]')) {
    if (btn.classList.contains('listening')) continue;
    const v = S[btn.dataset.shortcut];
    btn.replaceChildren(...(v ? keycaps(v) : [h('span', { className: 'none' }, 'None')]));
  }
  for (const b of $$('[data-clear]')) { b.hidden = !S[b.dataset.clear]; b.replaceChildren(svg(CROSS)); }
  $('#modeDesc').textContent = S.mode === 'toggle'
    ? 'Press once to start, again to stop. Esc cancels.'
    : 'Hold while you speak, let go to paste. Tap twice to go hands-free.';
  renderHero();
  $('#obTry').replaceChildren(verb(), ...talkCaps(), 'and say a sentence');
}

let recording = null;
function stopRecording() {
  if (!recording) return;
  window.removeEventListener('keydown', recording.fn, true);
  window.removeEventListener('keyup', recording.fn, true);
  recording.btn.classList.remove('listening');
  recording = null;
  renderHotkey();
}
const errEl = field => $(`#${field}Error`);
for (const btn of $$('[data-shortcut]')) {
  const field = btn.dataset.shortcut;
  btn.addEventListener('click', () => {
    if (recording) { const same = recording.btn === btn; stopRecording(); if (same) return; }
    const err = errEl(field);
    err.hidden = true;
    btn.classList.add('listening');
    btn.textContent = 'Press a shortcut…';
    const fn = async e => {
      e.preventDefault(); e.stopPropagation();
      const r = accelFrom(e);
      if (e.type === 'keyup') { if (!r.mods.length) btn.textContent = 'Press a shortcut…'; return; }
      if (e.key === 'Escape' && !r.mods.length) return stopRecording();
      if (!r.accel) {
        if (r.error) { err.textContent = r.error; err.hidden = false; }
        btn.textContent = r.mods.length ? r.mods.map(keyName).join(isMac ? ' ' : '+') + ' …' : 'Press a shortcut…';
        return;
      }
      setShortcut(field, r.accel);
    };
    recording = { btn, fn };
    window.addEventListener('keydown', fn, true);
    window.addEventListener('keyup', fn, true);
  });
  btn.addEventListener('blur', stopRecording);
}
for (const b of $$('[data-clear]')) b.addEventListener('click', () => setShortcut(b.dataset.clear, ''));

async function setShortcut(field, accel) {
  const prev = S[field], err = errEl(field);
  S[field] = accel;
  stopRecording();
  const fail = await save(true);
  if (fail) {
    S[field] = prev;
    err.textContent = accel ? `${hotkeyText(accel)} didn't work: ${fail}` : fail;
    err.hidden = false;
  }
  renderHotkey();
}

// ---------- talk key, Input Monitoring, Globe key ----------
async function refreshStatus() {
  try { status = await invoke('shortcut_status'); } catch { status = {}; }
  renderTalk();
  renderHotkey();
  refreshPerms(); // the Input Monitoring row reads `status`
}
function talkLines() {
  if (S.talk_key === 'accelerator') return [];
  if (status.talk_key_active) return [h('span', {}, `On. ${S.mode === 'toggle' ? 'Press' : 'Hold'} it anywhere to dictate.`)];
  const recheck = h('button', { className: 'btn', type: 'button', onclick: refreshStatus }, 'Check again');
  if (S.talk_key === 'fn' && !status.globe_does_nothing) return [h('span', {}, 'In System Settings, Keyboard, set “Press 🌐 key to” to “Do Nothing”. Until then fn would also open emoji or dictation.'),
    h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind: 'keyboard' }).catch(() => {}) }, 'Open Keyboard Settings'), recheck];
  if (!status.input_monitoring) return [h('span', {}, `Using ${hotkeyText(S.hotkey)} for now: this key needs Input Monitoring.`),
    h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind: 'input_monitoring' }) }, 'Allow Input Monitoring'), recheck];
  if (status.needs_relaunch) return [h('span', {}, 'Relaunch Echotype to finish.'), h('button', { className: 'btn primary', type: 'button', onclick: () => invoke('relaunch') }, 'Relaunch')];
  return [h('span', {}, status.talk_key_error || 'Not active yet.'), recheck];
}
function renderTalk() {
  for (const id of ['#talkStatus', '#obTalkStatus']) {
    const el = $(id), lines = talkLines();
    el.replaceChildren(...lines);
    el.hidden = !lines.length;
    el.classList.toggle('warn', S.talk_key !== 'accelerator' && !status.talk_key_active);
  }
  $('#obTalk').value = S.talk_key;
  const errs = status.shortcut_errors || [];
  if (errs.length && $('#hotkeyError').hidden) { $('#hotkeyError').textContent = 'Shortcut unavailable'; $('#hotkeyError').title = errs.join('\n'); $('#hotkeyError').hidden = false; }
}
$('#obTalk').addEventListener('change', e => { S.talk_key = e.target.value; syncBound(); save().then(refreshStatus); });

// ---------- home ----------
let dictation = { state: 'idle' };
const STATE_LABEL = { idle: 'Ready', recording: 'Listening', transcribing: 'Transcribing', cleaning: 'Cleaning up', done: 'Pasted', error: "Couldn't dictate", hint: 'Ready' };
const CHIP = { too_quiet: 'Too quiet, move the mic closer', quiet: 'Quiet voice', no_cleanup: 'No cleanup' };
// The one action that fixes a dictation error or notice (D7).
const ACTIONS = {
  microphone: ['Open Settings', () => invoke('open_permission_settings', { kind: 'microphone' })],
  accessibility: ['Allow pasting', () => invoke('open_permission_settings', { kind: 'accessibility' })],
  model: ['Choose a model', () => go('speech')],
  cleanup: ['Open Cleanup', () => go('cleanup')],
  settings: ['Open Settings', () => go('settings')],
};
const actionBtn = kind => ACTIONS[kind] && h('button', { className: 'btn', type: 'button', onclick: ACTIONS[kind][1] }, ACTIONS[kind][0]);
let muted = false;
// Per-bar gain of the Home waveform: a soft envelope, tallest in the middle.
const METER = Array.from({ length: 28 }, (_, i) => 0.35 + 0.65 * Math.sin(Math.PI * (i + 0.5) / 28));

// What stops dictation from working right now, with the one action that fixes it.
let perms = null, sttKeySaved = true;
function blocker() {
  if (perms && !perms.accessibility) return { text: "Echotype can't paste yet: Accessibility access is off.", label: 'Allow Accessibility', act: () => invoke('open_permission_settings', { kind: 'accessibility' }) };
  if (perms?.microphone === 'denied') return { text: 'Microphone access is blocked for Echotype.', label: 'Allow microphone', act: () => invoke('open_permission_settings', { kind: 'microphone' }) };
  const p = preset(S.stt.provider);
  if (S.stt.provider === 'cloud') {
    if (cloud.status === 'out' || cloud.status === 'code') return { text: 'Sign in to Echotype Cloud to transcribe.', label: 'Sign in', act: () => go('speech') };
  } else if (S.stt.provider === 'local') {
    const m = models.find(x => x.id === S.stt.local_model);
    if (m && progress[m.id] != null) return { text: `Downloading ${m.label}: ${Math.round(progress[m.id] * 100)}%. You can dictate when it finishes.`, label: 'Speech model', act: () => go('speech') };
    if (!m?.downloaded) return { text: 'No speech model is downloaded yet.', label: 'Choose a model', act: () => go('speech') };
  } else if (p?.needs_key && !sttKeySaved) return { text: `Add your ${p.label} API key to transcribe.`, label: 'Add key', act: () => go('speech') };
  return null;
}

function renderHero() {
  if (!S) return; // a focus event can land before boot finishes
  const st = dictation.state;
  const b = st === 'idle' || st === 'done' ? blocker() : null;
  document.body.dataset.state = st;
  document.body.toggleAttribute('data-blocked', !!b);
  $('#heroState span').textContent = b ? 'Not ready' : dictation.copied ? 'Copied' : STATE_LABEL[st] ?? 'Ready';
  const chip = dictation.chip && h('span', { className: 'tag' }, CHIP[dictation.chip]);
  const line = $('#heroLine'), sub = $('#heroSub');
  const mic = $('#heroMic');
  mic.ariaLabel = st === 'recording' ? 'Finish dictation' : 'Start dictation';
  mic.ariaDisabled = st === 'transcribing' || st === 'cleaning'; // not `disabled`: focus stays on it
  renderChips();
  if (st === 'recording') {
    line.replaceChildren(muted ? 'Is your mic muted?' : 'Listening');
    fill(sub, muted ? 'Echotype hears nothing from the microphone. It keeps recording.'
      : dictation.locked || S.mode === 'toggle' ? 'Press the key again to finish. Esc cancels.' : 'Let go to paste. Esc cancels.');
  } else if (st === 'transcribing' || st === 'cleaning') {
    line.replaceChildren(st === 'cleaning' ? 'Cleaning up…' : 'Transcribing…');
    fill(sub, h('span', {}, 'The text goes wherever your cursor is.'), chip);
  } else {
    line.replaceChildren(verb(), ...talkCaps(), 'to dictate');
    const cleanupFix = dictation.warning?.startsWith('Cleanup failed') && actionBtn('cleanup');
    if (b) fill(sub, h('span', {}, b.text), h('button', { className: 'btn', type: 'button', onclick: b.act }, b.label));
    else if (st === 'error') fill(sub, h('span', {}, `${dictation.error || 'Dictation stopped.'}`), ...keptText(), dictation.retry && retryBtn(), actionBtn(dictation.action));
    else if (st === 'hint') fill(sub, h('span', { className: 'notice-inline' }, dictation.message), actionBtn(dictation.action));
    else if (st === 'done' && dictation.copied) fill(sub, h('span', {}, dictation.warning || 'Press ⌘V to paste it.'), chip, ...keptText(), actionBtn(dictation.action));
    else if (st === 'done' && dictation.warning) fill(sub, h('span', {}, dictation.warning), chip, cleanupFix);
    else fill(sub, h('span', {}, 'Text appears wherever your cursor is.'), chip);
  }
}

// The composer's chips (D15): what Echotype will use right now; each opens its page.
const STYLE_LABEL = { default: 'Default style', casual: 'Casual', formal: 'Formal', verbatim: 'Verbatim' };
function renderChips() {
  const local = S.stt.provider === 'local';
  $('#chipModel').textContent = local ? models.find(m => m.id === S.stt.local_model)?.label ?? 'No model' : preset(S.stt.provider)?.label ?? S.stt.provider;
  const lang = LANGS.find(([v]) => v === S.language)?.[1] ?? S.language;
  $('#chipLang').textContent = S.translate_to_english ? `${lang} to English` : lang;
  $('#chipStyle').textContent = noCleanup() ? 'No cleanup' : STYLE_LABEL[S.cleanup.style] ?? S.cleanup.style;
  for (const b of $$('[data-go]')) b.ariaLabel = `${b.title}: ${b.textContent}`; // the chip opens that page
}
for (const b of $$('[data-go]')) b.addEventListener('click', () => go(b.dataset.go));
$('#heroMic').addEventListener('click', e => e.currentTarget.ariaDisabled !== 'true' && invoke('toggle_dictation'));
$('#heroMeter').append(...METER.map(() => h('i')));

// "No cleanup" (D4): cleanup is off, or on with no key. A dismissible Home banner offers the setup.
let cleanupKeySaved = true;
const noCleanup = () => !S.cleanup.enabled || (preset(S.cleanup.provider)?.needs_key && !cleanupKeySaved);
function renderRaw() {
  if (!S) return;
  $('#rawBanner').hidden = !noCleanup() || S.dismissed.includes('no_cleanup_banner');
  $('#cleanupSetup').hidden = !noCleanup();
  $('#cleanupSetupTitle').textContent = S.cleanup.enabled ? 'Finish setting up cleanup' : 'Turn on cleanup';
}
$('#rawDismiss').append(svg(CROSS));
$('#rawSetup').addEventListener('click', () => go('cleanup'));
$('#rawDismiss').addEventListener('click', () => dismiss('no_cleanup_banner'));
function dismiss(what) {
  if (!S.dismissed.includes(what)) S.dismissed.push(what);
  save();
  renderRaw(); renderUpgrade(); renderMicNote();
}
function setupCleanup(how) {
  S.cleanup.enabled = true;
  syncBound();
  if (how === 'cloud') setProvider('cleanup', 'cloud');
  else if (how === 'local') setProvider('cleanup', preset(S.cleanup.provider)?.needs_key === false && S.cleanup.provider !== 'custom' ? S.cleanup.provider : 'ollama');
  else setProvider('cleanup', preset(S.cleanup.provider)?.needs_key ? S.cleanup.provider : 'openai');
  setCleanupInert();
  renderRaw();
}
for (const b of $$('[data-setup]')) b.addEventListener('click', () => {
  setupCleanup(b.dataset.setup);
  (b.dataset.setup === 'key' ? $('[data-key="cleanup"] input') : $('#cleanupProvider'))?.focus();
});

// Text that was transcribed but not pasted: shown with Copy so it survives even with history off.
function keptText() {
  if (!dictation.text) return [];
  const copy = h('button', { className: 'btn', type: 'button' }, 'Copy');
  copy.onclick = () => copyText(dictation.text, copy);
  return [h('span', { className: 'text kept' }, dictation.text), copy];
}

// The recording is kept when transcription fails: run it again without re-dictating.
const retryBtn = () => h('button', { className: 'btn primary', type: 'button', onclick: () => invoke('retry_last').catch(e => { dictation = { state: 'error', error: String(e) }; renderHero(); }) }, 'Retry');

function renderRecent() {
  const box = $('#recent');
  if (!recent.length) return box.replaceChildren(h('p', { className: 'empty' }, 'Your dictations show up here.'));
  box.replaceChildren(...recent.slice(0, 3).map(r => {
    const copy = h('button', { className: 'btn quiet', type: 'button' }, 'Copy');
    copy.onclick = () => copyText(r.text, copy);
    return h('div', { className: 'row entry' }, h('span', { className: 'text' }, r.text.replace(/\s*\n\s*[-*•]?\s*/g, ' · ')), h('time', { className: 'when', title: fmtTime(r.ts).replace('\n', ' ') }, fmtAgo(r.ts)), copy);
  }));
}

function renderDrops() {
  const dismiss = r => h('button', { className: 'icon-btn', type: 'button', 'aria-label': `Dismiss ${r.name}`, onclick: () => { dropResults = dropResults.filter(x => x !== r); renderDrops(); } }, svg(CROSS));
  $('#dropResults').replaceChildren(...dropResults.map((r, i) => {
    if (r.pending) return h('div', { className: 'result' }, h('div', { className: 'result-head' }, h('span', { className: 'result-name working' }, `Transcribing ${r.name}`)));
    if (r.error) return h('div', { className: 'result err', role: 'alert' },
      h('div', { className: 'result-head' }, h('span', { className: 'result-name' }, `Couldn't transcribe ${r.name}`), dismiss(r)),
      h('p', {}, r.error),
      h('p', { className: 'hint' }, 'Echotype reads .opus, .ogg, .m4a, .mp3, .wav and .flac files.'));
    const copy = h('button', { className: 'btn', type: 'button' }, 'Copy');
    copy.onclick = () => copyText(r.showRaw ? r.raw : r.text, copy);
    const differs = r.raw.trim() !== r.text.trim();
    const seg = differs && h('span', { className: 'seg', role: 'radiogroup', 'aria-label': 'Version' },
      ['Clean', 'Raw'].map(v => h('label', {}, h('input', { type: 'radio', name: 'v' + i, checked: (v === 'Raw') === !!r.showRaw, onchange: () => { r.showRaw = v === 'Raw'; renderDrops(); } }), h('span', {}, v))));
    const secs = ((r.stt_ms || 0) + (r.cleanup_ms || 0)) / 1000;
    return h('div', { className: 'result' },
      h('div', { className: 'result-head' }, h('span', { className: 'result-name', title: r.path }, r.name), h('span', { className: 'result-meta', title: timingTitle(r) }, `${secs.toFixed(1)} s`), seg, copy, dismiss(r)),
      h('p', { className: 'text' }, r.showRaw ? r.raw : r.text),
      r.warning && h('p', { className: 'hint' }, r.warning));
  }));
}
const timingTitle = r => `Speech ${r.stt_ms} ms` + (r.cleanup_ms ? `, cleanup ${r.cleanup_ms} ms` : '');

async function transcribe(path) {
  const r = { path, name: path.split(/[\\/]/).pop(), pending: true };
  dropResults.unshift(r);
  dropResults = dropResults.slice(0, 5);
  renderDrops();
  try { Object.assign(r, await invoke('transcribe_file', { path })); }
  catch (e) { r.error = String(e); }
  r.pending = false;
  renderDrops();
}

// ---------- providers & keys ----------
const preset = id => presets.find(p => p.id === id);
const secretName = kind => `${kind === 'stt' ? 'stt' : 'llm'}:${S[kind].provider}`;

function setProvider(kind, id) {
  const p = preset(id), cfg = S[kind];
  cfg.provider = id;
  if (id !== 'custom') cfg.base_url = ''; // presets carry their own URL
  const def = p?.[kind === 'stt' ? 'default_stt_model' : 'default_cleanup_model'];
  if (def) cfg.model = def;
  save();
  if (kind === 'stt') renderStt(); else fillRemote('cleanup');
  renderLangNote();
  renderCloud();
  refreshCloud();
}

function fillRemote(kind) {
  const cfg = S[kind], p = preset(cfg.provider), custom = cfg.provider === 'custom';
  const url = $(`#${kind}Url`);
  url.readOnly = !custom;
  url.value = custom ? cfg.base_url : p?.base_url || '';
  url.placeholder = custom ? 'http://localhost:8000/v1' : '';
  $(`#${kind}UrlDesc`).textContent = custom
    ? `Any OpenAI-compatible ${kind === 'stt' ? '/audio/transcriptions' : '/chat/completions'} endpoint.`
    : `Set by the ${p?.label ?? ''} preset.`;
  const model = $(`#${kind}Model`);
  model.value = cfg.model;
  model.placeholder = p?.[kind === 'stt' ? 'default_stt_model' : 'default_cleanup_model'] || 'model name';
  renderKey(kind);
}
for (const kind of ['stt', 'cleanup']) {
  $(`#${kind}Url`).addEventListener('input', e => { if (S[kind].provider === 'custom') { S[kind].base_url = e.target.value.trim(); saveSoon(); } });
}

async function renderKey(kind) {
  const row = $(`[data-key="${kind}"]`);
  const p = preset(S[kind].provider), name = secretName(kind);
  // Local servers such as Ollama take no key; hide the row rather than show an optional field.
  // Echotype Cloud signs in on its own panel instead.
  row.hidden = (p?.needs_key === false && p.id !== 'custom') || p?.id === 'cloud';
  if (p?.id === 'cloud' && kind === 'cleanup') { cleanupKeySaved = await invoke('has_secret', { name: 'cloud' }).catch(() => false); renderRaw(); }
  else if (row.hidden && kind === 'cleanup') { cleanupKeySaved = true; renderRaw(); }
  if (row.hidden) return;
  const saved = await invoke('has_secret', { name }).catch(() => false);
  if (secretName(kind) !== name) return; // provider changed while we waited
  if (kind === 'stt') { sttKeySaved = saved; renderHero(); } else { cleanupKeySaved = saved; renderRaw(); }
  const title = h('span', { className: 'row-text' },
    h('span', { className: 'row-title' }, p?.needs_key === false ? 'API key (optional)' : 'API key'),
    h('span', { className: 'row-desc' }, 'Stored in your keychain, never in the settings file.'));
  if (saved) {
    row.replaceChildren(title, h('span', { className: 'actions' },
      h('span', { className: 'key-state' }, svg(CHECK), 'Key saved'),
      h('button', { className: 'btn', type: 'button', onclick: async () => { await invoke('delete_secret', { name }); renderKey(kind); } }, 'Remove')));
    return;
  }
  const input = h('input', { type: 'password', placeholder: 'Paste key', autocomplete: 'off', spellcheck: false, 'aria-label': `${p?.label ?? ''} API key` });
  const btn = h('button', { className: 'btn', type: 'button' }, 'Save');
  const store = async () => {
    const value = input.value.trim();
    if (!value) return input.focus();
    try { await invoke('set_secret', { name, value }); flash(); renderKey(kind); }
    catch (e) { flash(String(e)); }
  };
  btn.onclick = store;
  input.onkeydown = e => e.key === 'Enter' && store();
  row.replaceChildren(title, h('span', { className: 'actions' }, input, btn));
}

// ---------- speech model ----------
function renderStt() {
  const seg = $('#sttProvider');
  seg.replaceChildren(...[{ id: 'local', label: 'Local' }, ...presets.filter(p => p.stt)].map(p =>
    h('label', {}, h('input', { type: 'radio', name: 'sttp', value: p.id, checked: p.id === S.stt.provider, onchange: () => setProvider('stt', p.id) }), h('span', {}, p.label))));
  const local = S.stt.provider === 'local', viaCloud = S.stt.provider === 'cloud';
  $('#sttLocal').hidden = !local;
  $('#sttRemote').hidden = local || viaCloud;
  if (local) renderModels(); else if (!viaCloud) fillRemote('stt');
}

const fmtMB = mb => mb >= 1000 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;

const WAVE = '<path d="M2.5 6.5v3M5.25 4v8M8 2v12M10.75 5v6M13.5 7v2"/>';
const DOWN = '<path d="M8 2.75v8.5M4.5 7.75 8 11.25l3.5-3.5"/>';
const MORE = '<circle cx="3.5" cy="8" r=".9"/><circle cx="8" cy="8" r=".9"/><circle cx="12.5" cy="8" r=".9"/>';

// One model, in one of the D8 states: not downloaded (round ↓), downloading (ring), failed (reason +
// Retry), downloaded (Use, delete in the overflow menu), in use (✓). ponytail: no cancel while
// downloading, the core has no command for it yet.
function modelRow(m) {
  const inUse = S.stt.provider === 'local' && S.stt.local_model === m.id;
  const p = progress[m.id], err = modelErr[m.id];
  let actions;
  if (p != null) {
    actions = [h('span', { className: 'pct', 'data-pct': m.id }, `${Math.round(p * 100)}%`),
      h('span', { className: 'ring', role: 'progressbar', 'aria-label': `Downloading ${m.label}`, 'aria-valuenow': Math.round(p * 100), 'data-progress': m.id, style: `--p:${p}` })];
  } else if (!m.downloaded) {
    actions = [err ? h('button', { className: 'btn', type: 'button', onclick: () => download(m.id) }, 'Retry')
      : !m.fits ? h('button', { className: 'btn', type: 'button', onclick: () => download(m.id) }, 'Download anyway')
      : h('button', { className: 'round', type: 'button', title: 'Download', 'aria-label': `Download ${m.label}, ${fmtMB(m.size_mb)}`, onclick: () => download(m.id) }, svg(DOWN))];
  } else if (inUse) {
    actions = [h('span', { className: 'in-use' }, svg(CHECK), 'In use')];
  } else {
    // Delete asks twice: the model can be gigabytes to download again.
    const canPop = 'popover' in HTMLElement.prototype; // older WebViews get the button in the row instead
    const del = h('button', { className: canPop ? 'menu-item danger' : 'btn quiet danger', type: 'button' }, 'Delete model');
    del.onclick = async () => {
      if (!del.dataset.armed) { del.dataset.armed = 1; del.textContent = 'Click again to delete'; return setTimeout(() => { del.textContent = 'Delete model'; delete del.dataset.armed; }, 3000); }
      if (menu) menu.hidePopover();
      try { await invoke('delete_model', { id: m.id }); } catch (e) { modelErr[m.id] = String(e); }
      await refreshModels();
    };
    const menu = canPop && h('div', { id: `menu-${m.id}`, className: 'menu', popover: 'auto', onbeforetoggle: e => {
      if (e.newState !== 'open') return;
      const r = more.getBoundingClientRect();
      menu.style.top = `${r.bottom + 4}px`;
      menu.style.right = `${innerWidth - r.right}px`;
    } }, del);
    const more = canPop && h('button', { className: 'icon-btn', type: 'button', popovertarget: menu.id, 'aria-label': `More for ${m.label}` }, svg(MORE));
    actions = [h('button', { className: 'btn', type: 'button', onclick: () => useModel(m.id) }, 'Use'), ...(canPop ? [more, menu] : [del])];
  }
  return h('div', { className: 'row model' },
    h('span', { className: 'tile', 'aria-hidden': 'true' }, svg(WAVE)),
    h('span', { className: 'row-text' },
      h('span', { className: 'model-name' }, m.label,
        m.recommended && h('span', { className: 'tag rec' }, 'Recommended'),
        m.english_only && h('span', { className: 'tag' }, 'English only'),
        m.translates && h('span', { className: 'tag' }, 'Translates')),
      m.note && h('span', { className: 'row-desc' }, m.note),
      h('span', { className: 'model-meta' },
        h('span', {}, fmtMB(m.size_mb)),
        h('span', {}, `${fmtMB(m.ram_mb)} memory`),
        !m.fits && h('span', { className: 'badge warn' }, 'Needs more memory')),
      err && h('span', { className: 'row-desc error', role: 'alert' }, err)),
    h('span', { className: 'actions' }, actions));
}

function renderModels() {
  $('#memline').replaceChildren('Free memory now ', h('b', {}, `${sys.available_ram_gb.toFixed(1)} GB`), ` of ${Math.round(sys.total_ram_gb)} GB. A model fits when it leaves 2 GB for everything else.`);
  $('#modelList').replaceChildren(...models.map(m => modelRow(m)));
  renderObModel();
}

async function refreshModels() {
  [models, sys] = await Promise.all([invoke('list_models'), invoke('get_system_info')]);
  renderModels();
  renderLangNote();
  renderHero();
}

async function download(id) {
  delete modelErr[id];
  progress[id] = 0;
  renderModels();
  try { await invoke('download_model', { id }); }
  catch (e) { delete progress[id]; modelErr[id] = String(e); renderModels(); }
}

function useModel(id) {
  S.stt.provider = 'local';
  S.stt.local_model = id;
  save();
  renderStt();
  renderObModel();
  renderLangNote();
  renderHero();
}

// ---------- cleanup ----------
function setCleanupInert() {
  const off = !S.cleanup.enabled;
  $('#cleanupBody').hidden = off; // nothing to configure until cleanup is on
}
function renderCleanup() {
  $('#cleanupProvider').replaceChildren(...presets.filter(p => p.cleanup).map(p => h('option', { value: p.id, selected: p.id === S.cleanup.provider }, p.label)));
  setCleanupInert();
  fillRemote('cleanup');
}
$('#cleanupProvider').addEventListener('change', e => setProvider('cleanup', e.target.value));

// ---------- Echotype Cloud ----------
// status: loading | out (email step) | code (code step) | in (signed in) | error (unreachable, no account yet).
// The API key never reaches this window; Rust keeps it in the keychain.
let cloud = { status: 'loading' }, cloudEmail = '';
const usesCloud = () => S.stt.provider === 'cloud' || (S.cleanup.enabled && S.cleanup.provider === 'cloud');
const SIGNED_OUT = 'signed out';

function cloudFailed(e) {
  if (String(e) === SIGNED_OUT) cloud = { status: 'out' };
  else cloud.error = String(e);
  renderCloud();
}

async function refreshCloud() {
  if (!S || !usesCloud()) return;
  try {
    const account = await invoke('cloud_account');
    cloud = { status: 'in', account, note: account.plan === cloud.account?.plan ? cloud.note : null };
  } catch (e) {
    // Mid sign-in, leave the form alone: re-rendering would wipe what was typed, and the form shows its own errors.
    if (cloud.status === 'out' || cloud.status === 'code') return;
    cloud = String(e) === SIGNED_OUT ? { status: 'out' } : { ...cloud, status: cloud.account ? 'in' : 'error', error: String(e) };
  }
  renderCloud();
}

const fmtNum = n => Number(n || 0).toLocaleString();
function usageLine(a) {
  if (a.plan === 'pro') {
    const hours = s => (s / 3600).toFixed(1).replace(/\.0$/, '');
    return `Unlimited · ${hours(a.audio_seconds_used)} of ${hours(a.audio_seconds_limit)} audio hours this month`;
  }
  const day = new Date(a.resets_at).toLocaleDateString([], { weekday: 'long' });
  return `${fmtNum(a.words_used)} of ${fmtNum(a.words_limit)} words this week · resets ${day}`;
}

const cloudRow = (title, desc, actions, ...extra) => h('div', { className: 'row' },
  h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, title), desc && h('span', { className: 'row-desc' }, desc), ...extra),
  actions && h('span', { className: 'actions' }, actions));
const cloudErr = () => cloud.error && h('span', { className: 'row-desc error', role: 'alert' }, cloud.error);

// One form row: label text on the left, field and submit button on the right.
function cloudForm(title, desc, input, label, run) {
  const btn = h('button', { className: 'btn primary', type: 'submit' }, label);
  return h('form', { className: 'row', onsubmit: async e => {
    e.preventDefault();
    if (!input.value.trim()) return input.focus();
    btn.disabled = input.disabled = true;
    cloud.error = null;
    try { await run(input.value.trim()); }
    catch (err) { cloud.error = String(err); renderCloud(); $('#cloudAccount input')?.focus(); }
  } },
  h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, title), h('span', { className: 'row-desc' }, desc), cloudErr()),
  h('span', { className: 'actions' }, input, btn));
}

function cloudPanel() {
  if (cloud.status === 'loading') return [h('p', { className: 'empty working' }, 'Checking your account')];
  if (cloud.status === 'error') return [h('p', { className: 'empty', role: 'alert' }, `${cloud.error} `, h('button', { className: 'btn', type: 'button', onclick: () => { cloud = { status: 'loading' }; renderCloud(); refreshCloud(); } }, 'Try again'))];
  if (cloud.status === 'out') {
    const email = h('input', { type: 'text', inputMode: 'email', autocomplete: 'email', spellcheck: false, placeholder: 'you@example.com', 'aria-label': 'Email', value: cloudEmail });
    return [cloudForm('Sign in with your email', 'Echotype emails you a 6-digit code. There is no password.', email, 'Send code', async v => {
      cloudEmail = v;
      await invoke('cloud_start_login', { email: v });
      cloud = { status: 'code' };
      renderCloud();
      $('#cloudCode')?.focus();
    })];
  }
  if (cloud.status === 'code') {
    const code = h('input', { type: 'text', id: 'cloudCode', className: 'code', inputMode: 'numeric', autocomplete: 'one-time-code', maxLength: 6, spellcheck: false, placeholder: '6 digits', 'aria-label': 'Sign-in code' });
    return [
      cloudForm('Enter the code', `Sent to ${cloudEmail}. It works for 10 minutes.`, code, 'Sign in', async v => {
        const who = await invoke('cloud_verify', { email: cloudEmail, code: v });
        cloud = { status: 'in', account: who };
        renderCloud();
        refreshCloud();
      }),
      h('div', { className: 'row' }, h('button', { className: 'btn quiet', type: 'button', onclick: () => { cloud = { status: 'out' }; renderCloud(); $('#cloudAccount input')?.focus(); } }, 'Use a different email')),
    ];
  }
  const a = cloud.account, pro = a.plan === 'pro';
  const billing = (label, cmd, args, primary) => {
    const btn = h('button', { className: primary ? 'btn primary' : 'btn', type: 'button' }, label);
    btn.onclick = async () => {
      btn.disabled = true;
      cloud.error = cloud.note = null;
      try { await invoke(cmd, args); cloud.note = 'Opened in your browser. Your plan updates here when you come back.'; renderCloud(); }
      catch (e) { cloudFailed(e); }
    };
    return btn;
  };
  const signOut = h('button', { className: 'btn quiet', type: 'button', onclick: async () => {
    try { await invoke('cloud_logout'); cloud = { status: 'out' }; renderCloud(); } catch (e) { cloudFailed(e); }
  } }, 'Sign out');
  const free = !pro && a.words_limit > 0;
  const status = [cloud.note && h('span', { className: 'row-desc' }, cloud.note), cloudErr()];
  return [
    cloudRow(a.email, pro ? 'Pro plan' : 'Free plan', [signOut]),
    a.words_used != null && cloudRow('Usage', usageLine(a),
      free && [h('span', { className: 'bar', role: 'progressbar', 'aria-label': 'Words used this week', 'aria-valuenow': Math.round(Math.min(1, a.words_used / a.words_limit) * 100) }, h('i', { style: `transform:scaleX(${Math.min(1, a.words_used / a.words_limit)})` }))]),
    pro
      ? cloudRow('Billing', 'Change your plan, update your card or cancel.', [billing('Manage billing', 'cloud_portal')], ...status)
      : cloudRow('Upgrade to Pro', 'Unlimited words, with a fair-use limit of 40 audio hours a month.', [billing('$6 a month', 'cloud_checkout', { plan: 'pro_monthly' }), billing('$60 a year', 'cloud_checkout', { plan: 'pro_yearly' }, true)], ...status),
  ];
}

function renderCloud() {
  if (!S) return;
  $('#cloudBox').hidden = !usesCloud();
  if (usesCloud()) $('#cloudAccount').replaceChildren(...cloudPanel().filter(Boolean));
  // Cleanup through the cloud uses the same account: no URL, model or key to set.
  const viaCloud = S.cleanup.provider === 'cloud';
  for (const id of ['#cleanupUrl', '#cleanupModel']) $(id).closest('.row').hidden = viaCloud;
  if (viaCloud) $('[data-key="cleanup"]').hidden = true;
  const row = $('#cleanupCloud');
  row.hidden = !viaCloud;
  if (viaCloud) {
    const st = cloud.status;
    fill(row, h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, 'Uses your Echotype Cloud account'),
      h('span', { className: st === 'error' ? 'row-desc error' : 'row-desc' },
        st === 'in' ? `Signed in as ${cloud.account.email}.` : st === 'loading' ? 'Checking your account…' : st === 'error' ? cloud.error : 'You are not signed in. Sign in on the Speech model page.')),
      (st === 'out' || st === 'code') && h('span', { className: 'actions' }, h('button', { className: 'btn', type: 'button', onclick: () => go('speech') }, 'Open Speech model')));
  }
  renderHero();
}
window.addEventListener('focus', refreshCloud); // back from the browser after checkout
window.addEventListener('focus', () => S && renderRecent()); // relative times move on while the window is away

// ---------- language ----------
const LANGS = [['auto', 'Auto-detect'], ['en', 'English'], ['hi', 'Hindi'], ['ar', 'Arabic'], ['bn', 'Bengali'], ['zh', 'Chinese'], ['nl', 'Dutch'], ['fr', 'French'], ['de', 'German'], ['gu', 'Gujarati'], ['id', 'Indonesian'], ['it', 'Italian'], ['ja', 'Japanese'], ['kn', 'Kannada'], ['ko', 'Korean'], ['ml', 'Malayalam'], ['mr', 'Marathi'], ['pa', 'Punjabi'], ['pl', 'Polish'], ['pt', 'Portuguese'], ['ru', 'Russian'], ['es', 'Spanish'], ['ta', 'Tamil'], ['te', 'Telugu'], ['tr', 'Turkish'], ['uk', 'Ukrainian'], ['ur', 'Urdu'], ['vi', 'Vietnamese']];
$('#language').append(...LANGS.map(([v, l]) => h('option', { value: v }, l)));

function renderLangNote() {
  const note = $('#translateNote');
  const m = models.find(x => x.id === S.stt.local_model);
  note.hidden = !(S.translate_to_english && S.stt.provider === 'local' && m && !m.translates);
  if (note.hidden) return;
  const alt = models.find(x => x.translates && x.downloaded);
  note.classList.toggle('warn', !alt);
  fill(note, alt
    ? `${m.label} can't translate, so Echotype uses ${alt.label} while this is on.`
    : `${m.label} can't translate, and no multilingual model is downloaded yet. Medium works best for Hinglish.`,
  !alt && h('button', { className: 'btn', type: 'button', onclick: () => go('speech') }, 'Choose a model'));
}

// ---------- dictionary ----------
function renderDict() {
  $('#chips').replaceChildren(...S.dictionary.map((w, i) =>
    h('span', { className: 'chip' }, w, h('button', { type: 'button', 'aria-label': `Remove ${w}`, onclick: () => { S.dictionary.splice(i, 1); save(); renderDict(); $('#newWord').focus(); } }, svg(CROSS)))));
  $('#reps').replaceChildren(...S.replacements.map((r, i) => {
    const field = (k, label) => h('input', { type: 'text', value: r[k], 'aria-label': label, spellcheck: false, placeholder: k === 'from' ? 'my email' : 'me@example.com', oninput: e => { r[k] = e.target.value; saveSoon(); } });
    return h('div', { className: 'row rep' }, field('from', 'When you say'), field('to', 'Type'),
      h('button', { className: 'icon-btn', type: 'button', 'aria-label': 'Remove replacement', onclick: () => { S.replacements.splice(i, 1); save(); renderDict(); } }, svg(CROSS)));
  }));
}
$('#addWord').addEventListener('submit', e => {
  e.preventDefault();
  const w = $('#newWord').value.trim();
  if (w && !S.dictionary.some(x => x.toLowerCase() === w.toLowerCase())) { S.dictionary.push(w); save(); renderDict(); }
  $('#newWord').value = '';
});
$('#addRep').addEventListener('click', () => {
  S.replacements.push({ from: '', to: '' });
  renderDict();
  $$('#reps input').at(-2).focus();
});

// ---------- history ----------
function fmtTime(ts) {
  const d = new Date(typeof ts === 'number' && ts < 1e12 ? ts * 1000 : ts);
  if (isNaN(d)) return '';
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.toDateString() === new Date().toDateString()) return time;
  const day = d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  return `${day}\n${time}`;
}

// "2m", "1h" under a day, then a date (D15).
function fmtAgo(ts) {
  const d = new Date(typeof ts === 'number' && ts < 1e12 ? ts * 1000 : ts);
  if (isNaN(d)) return '';
  const min = Math.max(0, Math.floor((Date.now() - d) / 60000));
  if (min < 1) return 'now';
  if (min < 60) return `${min}m`;
  if (min < 1440) return `${Math.floor(min / 60)}h`;
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

async function renderHistory() {
  const list = $('#historyList');
  if (!list.children.length) list.replaceChildren(h('p', { className: 'empty' }, 'Loading history…'));
  let items;
  try { items = await invoke('get_history', { limit: 200 }); }
  catch (e) {
    list.replaceChildren(h('p', { className: 'empty', role: 'alert' }, `Couldn't read history: ${e}`,
      h('button', { className: 'btn', type: 'button', onclick: renderHistory }, 'Try again')));
    return;
  }
  $('#clearHistory').disabled = !items.length;
  if (!items.length) {
    list.replaceChildren(S.history_enabled
      ? h('p', { className: 'empty' }, 'Your dictations show up here. ', verb(), ' ', hotkeyText(S.hotkey), ' in any text field to start.')
      : h('p', { className: 'empty' }, 'History is off, so nothing is kept.', h('button', { className: 'btn', type: 'button', onclick: () => go('settings') }, 'Turn on in Settings')));
    return;
  }
  list.replaceChildren(...items.map(it => {
    const copy = h('button', { className: 'btn quiet', type: 'button' }, 'Copy');
    copy.onclick = e => { e.preventDefault(); copyText(it.text, copy); };
    return h('details', {},
      h('summary', {}, h('span', { className: 'text' }, it.text), it.no_cleanup && h('span', { className: 'tag' }, 'No cleanup'), h('time', { className: 'when', title: fmtTime(it.ts).replace('\n', ' ') }, fmtAgo(it.ts)), copy),
      h('div', { className: 'detail' },
        it.raw !== it.text && h('p', { className: 'raw text' }, h('b', {}, 'Heard: '), it.raw),
        h('span', { className: 'timing' },
          h('span', {}, `Speech ${it.stt_ms} ms${it.stt_provider ? ` (${it.stt_provider})` : ''}`),
          it.cleanup_provider && h('span', {}, `Cleanup ${it.cleanup_ms} ms (${it.cleanup_provider})`)),
        it.no_cleanup && noCleanup() && h('p', { className: 'hint' }, 'Kept as you said it because cleanup is off. ', h('button', { className: 'btn quiet', type: 'button', onclick: () => go('cleanup') }, 'Set up cleanup'))));
  }));
}
const confirmClear = on => { $('#clearHistory').hidden = on; $('#clearConfirm').hidden = !on; (on ? $('#clearCancel') : $('#clearHistory')).focus(); };
$('#clearHistory').addEventListener('click', () => confirmClear(true));
$('#clearCancel').addEventListener('click', () => confirmClear(false));
$('#clearYes').addEventListener('click', async () => {
  await invoke('clear_history');
  recent = [];
  renderRecent();
  confirmClear(false);
  renderHistory();
});

// ---------- settings: mic, local API, permissions ----------
async function renderMics() {
  try { devices = await invoke('list_input_devices'); $('#micError').hidden = true; }
  catch (e) { devices = []; $('#micError').textContent = `Couldn't list microphones (${e}). Echotype uses the system default.`; $('#micError').hidden = false; }
  const def = devices.find(d => d.default);
  $('#mic').replaceChildren(h('option', { value: '' }, def ? `System default (${def.name})` : 'System default'), ...devices.map(d => h('option', { value: d.name }, d.name)));
  $('#mic').value = S.input_device ?? '';
  renderMicNote();
}
// D13: a persistent info glyph for a Bluetooth mic, plus a one-time tip.
function renderMicNote() {
  const d = devices.find(x => (S.input_device ? x.name === S.input_device : x.default));
  const bt = !!d?.bluetooth;
  $('#btInfo').hidden = !bt;
  $('#btTip').hidden = !bt || S.dismissed.includes('bluetooth_tip');
}
$('#btTipOk').addEventListener('click', () => dismiss('bluetooth_tip'));

// Upgraders: the new shortcuts and launch at login are offered once, never switched on silently.
function renderUpgrade() {
  const box = $('#upgradeOffer');
  const [p, c] = DEFAULT_LAST();
  box.hidden = !S.onboarded || S.dismissed.includes('upgrade_offer') || (!p && !sys.can_autostart);
  if (box.hidden) return;
  fill(box.querySelector('.notice-text'), h('b', {}, 'New: '),
    p ? ['paste your last dictation again with ', h('span', { className: 'keys' }, keycaps(p)), ', copy it with ', h('span', { className: 'keys' }, keycaps(c)), sys.can_autostart ? ', and start Echotype at login.' : '.'] : 'start Echotype at login.');
}
$('#upgradeYes').addEventListener('click', () => {
  const [p, c] = DEFAULT_LAST();
  S.paste_last_hotkey ||= p;
  S.copy_last_hotkey ||= c;
  if (sys.can_autostart) S.autostart = true;
  syncBound();
  dismiss('upgrade_offer');
  renderHotkey();
});
$('#upgradeNo').addEventListener('click', () => dismiss('upgrade_offer'));

function renderStartup() {
  const ok = sys.can_autostart;
  $('#autostart').disabled = !ok && !S.autostart;
  $('#autostartDesc').textContent = ok ? 'Echotype starts quietly in the menu bar.' : 'Move Echotype to your Applications folder first.';
  $('#obAutostartRow').hidden = !ok;
  $('#talkRow').hidden = $('#obTalkRow').hidden = $('#soundsRow').hidden = !isMac; // talk keys and sounds are macOS-only
  $('#pillTitle').hidden = $('#pillGroup').hidden = !isMac; // so is the idle pill
}

// "Hide for 1 hour" shows until when (D12); the hour ends by itself, so re-render then.
let pillTimer;
const HIDE_MS = 3600 * 1000; // pill.rs HIDE_FOR
function renderPill() {
  const until = S.pill_hidden_until || 0, hidden = until > Date.now();
  const at = new Date(until).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }); // as the tray says it
  $('#pillHidden').textContent = !S.pill_idle ? 'The pill shows only while you dictate.'
    : hidden ? `Hidden until ${at}. It still shows while you dictate.` : 'The pill still shows while you dictate.';
  $('#pillHide').textContent = hidden ? 'Show now' : 'Hide';
  $('#pillHide').disabled = !S.pill_idle;
  clearTimeout(pillTimer);
  if (hidden) pillTimer = setTimeout(renderPill, Math.min(until - Date.now() + 500, 2 ** 31 - 1));
}
$('#pillHide').addEventListener('click', () => {
  S.pill_hidden_until = S.pill_hidden_until > Date.now() ? 0 : Date.now() + HIDE_MS;
  save();
  renderPill();
});

async function refreshApi() {
  $('#apiBody').hidden = !S.local_api.enabled;
  if (!S.local_api.enabled) return;
  let err = '';
  try { apiStatus = await invoke('local_api_status'); } catch (e) { apiStatus = null; err = String(e); }
  const tok = apiStatus?.token || '';
  const port = apiStatus?.port || S.local_api.port;
  const shown = tokenShown ? tok : tok.slice(0, 6) + '•'.repeat(Math.max(0, Math.min(18, tok.length - 6)));
  $('#token').textContent = tok ? shown : err ? `Couldn't start the API: ${err}. Try another port.` : 'Starting the server…';
  $('#token').classList.toggle('error', !!err);
  $('#showToken').textContent = tokenShown ? 'Hide' : 'Show';
  $('#curl').textContent = curl(port, shown);
}
const curl = (port, tok) => `curl http://127.0.0.1:${port}/v1/audio/transcriptions \\\n  -H "Authorization: Bearer ${tok}" \\\n  -F file=@voice-note.m4a`;
$('#showToken').addEventListener('click', () => { tokenShown = !tokenShown; refreshApi(); });
$('#copyToken').addEventListener('click', e => copyText(apiStatus?.token || '', e.currentTarget));
$('#copyCurl').addEventListener('click', e => copyText(curl(apiStatus?.port || S.local_api.port, apiStatus?.token || ''), e.currentTarget));

// [badge, badge style, show the Open Settings button]
const MIC = { granted: ['Allowed', '', false], denied: ['Blocked', 'bad', true], unknown: ['Asked on first use', 'muted', false] };
async function refreshPerms() {
  if (!S) return; // a focus event can land before boot finishes
  let failed = false;
  try { perms = await invoke('check_permissions'); }
  catch { failed = true; perms = null; }
  const p = perms ?? { microphone: 'unknown', accessibility: true };
  const row = (title, desc, [label, cls, fix], kind) => h('div', { className: 'row' },
    h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, title), h('span', { className: 'row-desc' }, desc)),
    h('span', { className: 'actions' }, h('span', { className: `badge ${cls}` }, label),
      fix && h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind }) }, 'Open Settings')));
  const rows = () => failed
    ? [h('p', { className: 'empty', role: 'alert' }, "Couldn't check permissions. Open System Settings, Privacy & Security, to review Microphone and Accessibility.")]
    : [
      row('Microphone', p.microphone === 'unknown' ? 'macOS asks the first time you dictate.' : 'To hear you while the shortcut is down.', MIC[p.microphone] ?? MIC.unknown, 'microphone'),
      row('Accessibility', 'Echotype uses it only to press ⌘V in the app you are using.', p.accessibility ? ['Allowed', '', false] : ['Needed to paste', 'warn', true], 'accessibility'),
    ];
  const im = isMac && S.talk_key !== 'accelerator' && [row('Input Monitoring', 'Only for the talk key: Echotype watches that one key, and never records what you type.',
    status.input_monitoring ? ['Allowed', '', false] : ['Needed for the talk key', 'warn', true], 'input_monitoring')];
  $('#perms').replaceChildren(...rows(), ...(im || []));
  $('#obPerms').replaceChildren(rows()[1] ?? rows()[0]);
  $('#obMic').replaceChildren(rows()[0]);
  $('#obPasteNext').textContent = p.accessibility ? 'Continue' : 'Continue without pasting';
  $('#axLine').hidden = failed || p.accessibility;
  renderHero();
}
window.addEventListener('focus', refreshPerms); // coming back from System Settings
window.addEventListener('focus', () => S && refreshStatus()); // Input Monitoring or the Globe key may have changed
for (const b of $$('[data-perm]')) b.addEventListener('click', () => invoke('open_permission_settings', { kind: b.dataset.perm }));

// ---------- onboarding ----------
// D9: the model downloads in the background while the mic is set up; the first dictation lands in
// a practice field (never pasted); Accessibility comes after; extras last. Nothing blocks the next step.
let obStep = 1;
function obShow(n) {
  obStep = n;
  $$('.ob-step').forEach(s => (s.hidden = +s.dataset.step !== n));
  $$('#obSteps li').forEach((li, i) => { li.className = i + 1 === n ? 'on' : i + 1 < n ? 'done' : ''; li.toggleAttribute('aria-current', i + 1 === n); });
  renderObModel();
  if (n === 4) refreshStatus();
  $(`.ob-step[data-step="${n}"] h2`).focus();
}
function showOnboarding(on) {
  $('#onboard').hidden = !on;
  $('aside').inert = $('main').inert = on; // keep Tab inside the dialog
}
// Start the recommended model downloading as soon as onboarding opens.
function obStartModel() {
  if (S.stt.provider !== 'local') return;
  const chosen = models.find(m => m.id === S.stt.local_model);
  if (chosen?.downloaded) return;
  const pick = chosen && chosen.fits ? chosen : models.find(m => m.recommended);
  if (!pick) return;
  if (S.stt.local_model !== pick.id) { S.stt.local_model = pick.id; save(); }
  if (progress[pick.id] == null) download(pick.id);
}
function renderObModel() {
  if ($('#onboard').hidden) return;
  const m = models.find(x => x.id === S.stt.local_model), p = m && progress[m.id];
  const text = S.stt.provider !== 'local' ? `Using ${preset(S.stt.provider)?.label ?? S.stt.provider} for speech.`
    : !m ? 'None of the local models fit in the memory free right now. You can use an API instead.'
    : m.downloaded ? `${m.label} is ready.`
    : modelErr[m.id] ? `Couldn't download ${m.label}: ${modelErr[m.id]}`
    : `Downloading ${m.label} (${fmtMB(m.size_mb)}) · ${Math.round((p || 0) * 100)}%`;
  $('#obModel').textContent = text;
  const ready = S.stt.provider !== 'local' || m?.downloaded;
  if (dictation.state === 'idle' || dictation.state === 'done' || dictation.state === 'hint') {
    $('#obTryState').textContent = ready ? 'Say a sentence. Your words show up below.' : `Model downloading ${Math.round((p || 0) * 100)}%. You can try as soon as it finishes.`;
  }
  $('#obToggle').disabled = !ready && dictation.state !== 'recording';
}
$$('[data-next]').forEach(b => b.addEventListener('click', () => obShow(obStep + 1)));
$$('[data-recheck]').forEach(b => b.addEventListener('click', refreshPerms));
async function finishOnboarding(page) {
  // New installs get the new shortcuts and (when allowed) launch at login; upgraders are only offered them.
  const [p, c] = DEFAULT_LAST();
  S.paste_last_hotkey ||= p;
  S.copy_last_hotkey ||= c;
  S.autostart = !!sys.can_autostart && $('#obAutostart').checked;
  if (!S.dismissed.includes('upgrade_offer')) S.dismissed.push('upgrade_offer');
  S.onboarded = true;
  syncBound();
  await save();
  showOnboarding(false);
  renderHotkey();
  go(page);
}
$('#obSkip').addEventListener('click', () => finishOnboarding('home'));
$('#onboard').addEventListener('keydown', e => { if (e.key === 'Escape') finishOnboarding('home'); });
$('#obApi').addEventListener('click', () => finishOnboarding('speech'));
$('#obDone').addEventListener('click', () => {
  const how = $('#obCleanup').value;
  if (how !== 'off') setupCleanup(how);
  finishOnboarding(how === 'off' ? 'home' : 'cleanup');
});
$('#obToggle').addEventListener('click', () => invoke('toggle_dictation'));

// ---------- clipboard ----------
async function copyText(text, btn) {
  try { await navigator.clipboard.writeText(text); }
  catch {
    const t = h('textarea', { value: text });
    document.body.append(t); t.select(); document.execCommand('copy'); t.remove();
  }
  if (!btn) return;
  const label = btn.textContent;
  btn.textContent = 'Copied';
  setTimeout(() => (btn.textContent = label), 1200);
}

// ---------- events ----------
listen('dictation-state', ({ payload }) => {
  dictation = payload;
  renderHero();
  if ((payload.state === 'done' || payload.state === 'error') && payload.text) { // error + text = transcribed but not pasted
    recent.unshift({ ts: Date.now(), text: payload.text });
    renderRecent();
  }
  if (payload.state === 'done' || payload.state === 'error') refreshCloud(); // words used went up
  if (payload.state !== 'recording') muted = false;
  // The practice field fills from the result itself; nothing is pasted during onboarding.
  const st = $('#obTryState'), res = $('#obResult');
  st.textContent = payload.state === 'error' ? payload.error || 'Something went wrong. Try again.'
    : payload.state === 'hint' ? payload.message
    : payload.state === 'done' ? 'That is what Echotype heard. Try another, or continue.'
    : payload.state === 'idle' ? 'Say a sentence. Your words show up below.'
    : `${STATE_LABEL[payload.state]}…`;
  if (payload.state === 'done' && payload.text) res.textContent = payload.text;
  $('#obToggle').textContent = payload.state === 'recording' ? 'Stop' : 'Start with a click';
  if (payload.state === 'idle') renderObModel();
});

// Rust is the source of truth; keep any edit made here that has not been saved yet.
listen('settings-changed', ({ payload }) => {
  if (!S) return;
  const echo = eq(payload, base); // our own save coming back
  S = merge3(payload, base, S);
  base = clone(payload);
  if (echo) return; // re-rendering would steal focus from the field being typed in
  syncBound();
  renderAll();
});

listen('audio-level', ({ payload }) => {
  if (!!payload.muted !== muted) { muted = !!payload.muted; renderHero(); }
  const bars = $$('#heroMeter i');
  const lvl = Math.max(0, Math.min(1, payload.meter));
  bars.forEach((b, i) => { b.style.transform = `scaleY(${Math.max(0.15, Math.min(1, lvl * METER[i] * (0.75 + Math.random() * 0.5)))})`; });
});

listen('model-download', ({ payload: p }) => {
  if (p.error) { delete progress[p.id]; modelErr[p.id] = p.error; renderModels(); renderObModel(); return; }
  if (p.done) {
    delete progress[p.id];
    refreshModels().then(renderObModel);
    return;
  }
  const frac = p.total ? p.downloaded / p.total : 0;
  const had = progress[p.id] != null;
  progress[p.id] = frac;
  const bars = $$(`[data-progress="${p.id}"]`);
  if (!had || !bars.length) { renderModels(); return renderHero(); }
  bars.forEach(b => { b.style.setProperty('--p', frac); b.setAttribute('aria-valuenow', Math.round(frac * 100)); });
  $$(`[data-pct="${p.id}"]`).forEach(el => (el.textContent = `${Math.round(frac * 100)}%`));
  renderObModel();
  renderHero(); // the readiness line counts the download
});

// The pill's menu (History, Settings) and its notices open a page here.
listen('navigate', ({ payload }) => { if ($('#onboard').hidden) go(payload); });

listen('tauri://drag-enter', () => $('#drop').classList.add('over'));
listen('tauri://drag-leave', () => $('#drop').classList.remove('over'));
listen('tauri://drag-drop', ({ payload }) => {
  $('#drop').classList.remove('over');
  if (!payload?.paths?.length) return;
  if ($('#onboard').hidden) go('home');
  payload.paths.forEach(transcribe);
});

// ---------- boot ----------
try {
  [S, presets, models, sys] = await Promise.all([invoke('get_settings'), invoke('get_presets'), invoke('list_models'), invoke('get_system_info')]);
} catch (e) {
  $('#main').replaceChildren(h('h1', {}, "Echotype couldn't load its settings"),
    h('p', { className: 'lede boot-error', role: 'alert' }, `${e}. Quit Echotype from the menu bar and open it again. If this keeps happening, the settings file may be damaged.`));
  throw e;
}
isMac = /mac|darwin/i.test(sys.os);
// macOS gets the translucent sidebar (windowEffects in tauri.conf.json); elsewhere it stays opaque (D19).
document.documentElement.dataset.os = isMac ? 'mac' : 'other';
if (isMac && !preview) document.documentElement.dataset.vibrant = '';
S.dictionary ??= [];
S.replacements ??= [];
S.dismissed ??= [];
base = clone(S);
recent = await invoke('get_history', { limit: 3 }).catch(() => []);

function renderAll() {
  renderHotkey();
  renderPill();
  renderRaw();
  renderUpgrade();
  renderStartup();
  renderMicNote();
  renderTalk();
  renderStt();
  renderCleanup();
  renderCloud();
  renderLangNote();
  renderDict();
}

bindAll();
syncBound();
renderAll();
renderRecent();
refreshCloud();
renderMics();
refreshPerms();
refreshStatus();
const start = location.hash.slice(1);
go($(`main > section[data-page="${start}"]`) ? start : 'home');
if (!S.onboarded) { showOnboarding(true); obStartModel(); obShow(+new URLSearchParams(location.search).get('step') || 1); }
