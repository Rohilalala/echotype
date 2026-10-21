// Echotype settings window. Talks to the Rust core through Tauri commands and events.
const preview = !window.__TAURI__;
if (preview) await import('./mock.js'); // browser preview for design QA
const meetingsPage = await import('./meetings.js');
const { h, copyText } = await import('./dom.js');
const { createOrb } = await import('./orb.js'); // onboarding's orb

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

// replaceChildren without the stray "false"/"undefined" text of an absent optional child.
const fill = (el, ...kids) => el.replaceChildren(...kids.flat().filter(k => k != null && k !== false && k !== ''));
// Tiny element builder. Strings become text nodes, so user text is never parsed as HTML.
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
// Windows and Linux: the same words without macOS names (the paste keys, the menu bar, "this Mac").
const PC_COPY = [
  ['With Accessibility on, Echotype presses ⌘V in the app you are using. Without it, your text waits on the clipboard.', 'Echotype presses Ctrl+V in the app you are using.'],
  ['⌘V', 'Ctrl+V'], ['this Mac', 'this computer'], ['the menu bar', 'the system tray'],
];
const pc = t => isMac ? t : PC_COPY.reduce((s, [a, b]) => s.replaceAll(a, b), t);
let status = {}; // shortcut_status: talk key, Input Monitoring, Globe key, shortcuts another app holds
let devices = [];
const progress = {}, modelErr = {};
let recent = [], dropResults = [], apiStatus = null, tokenShown = false;

// ---------- saving ----------
let saveTimer, saving = Promise.resolve();
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
// Settings save as you change them, silently; only a failed save says so.
function flash(err) {
  const el = $('#saved');
  el.classList.add('err');
  el.replaceChildren(`Couldn't save: ${err}`);
  el.classList.add('show');
  void el.offsetWidth;
  el.classList.remove('show');
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
      let v = el.type === 'checkbox' ? el.checked : ['number', 'range'].includes(el.type) || 'num' in el.dataset ? Number(el.value) : el.value;
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
  if (p === 'input_device' || p === 'prefer_builtin_mic') renderMicNote();
  if (p === 'cleanup.enabled') renderRaw();
  if (p === 'languages' || p === 'script') { renderLanguages(); if (obStep === 2 && !$('#onboard').hidden) obLangsRender(); }
  if (p === 'cleanup.enabled') { setCleanupInert(); renderCloud(); refreshCloud(); }
  if (p === 'cleanup.match_app') refreshLastApp();
  if (p === 'cleanup.level') renderLevel();
  if (p.startsWith('local_api')) refreshApi();
  if (p === 'history_enabled') { renderHistory(); $('#clipDaysRow').hidden = !S.history_enabled; }
  if (p === 'clip_days') renderHistory();
  if (p === 'pill_idle' || p === 'show_in') renderPill();
  if (p === 'pill_size' || p === 'ball_size') sizePreview();
  if (p === 'sounds') $('#soundPackRow').hidden = !S.sounds;
  if (p === 'detect_meetings') renderStartup();
  if (p === 'greetings' || p === 'display_name') renderGreeting();
  if (p === 'sound_pack') invoke('preview_sounds', { pack: S.sound_pack }).catch(() => {}); // hear it as you pick it
  if (p === 'advanced' || p === 'cleaner_memory') { renderAdvanced(); renderStt(); fillRemote('cleanup'); renderLangNote(); renderHero(); }
  renderChips();
}

// ---------- navigation ----------
function go(page) {
  for (const s of $$('main > section')) s.hidden = s.dataset.page !== page;
  for (const b of $$('nav button')) b.toggleAttribute('aria-current', false), b.dataset.page === page && b.setAttribute('aria-current', 'page');
  $('#main').scrollTop = 0;
  document.documentElement.classList.remove('pane-scrolled');
  if (page === 'home') renderGreeting();
  if (page === 'history') renderHistory();
  if (page === 'meetings') meetingsPage.show();
  if (page === 'settings') { refreshPerms(); refreshApi(); refreshStatus(); }
  if (page === 'account' || page === 'cleanup') refreshCloud(); // the account box moved to Account
  if (page === 'cleanup') { refreshLastApp(); refreshInstalled(); }
  history.replaceState(null, '', '#' + page);
  document.dispatchEvent(new CustomEvent('echotype:page', { detail: page })); // stats.js refreshes on its page
  window.__TAURI__.event.emit?.('main-page', page); // Home shows the live state, so the pill steps aside there (D14)
}
for (const n of ['#nav', '#navFoot']) $(n).addEventListener('click', e => { const b = e.target.closest('button'); if (b) go(b.dataset.page); });
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
const talkCaps = () => status.talk_key_active && S.talk_key !== 'accelerator' ? [h('kbd', {}, status.key_label)] : keycaps(S.hotkey);
const verb = () => (S.mode === 'toggle' ? 'Press' : 'Hold');
// ponytail: macOS only. Elsewhere the paste keystroke itself would be Ctrl+V while Ctrl+Alt is still
// held (no modifier wait off macOS), re-firing the shortcut; users can bind their own there.
const DEFAULT_LAST = () => isMac ? ['CommandOrControl+Control+V', 'CommandOrControl+Control+C'] : ['', ''];

function renderHotkey() {
  for (const btn of $$('[data-shortcut]')) {
    if (btn.classList.contains('listening')) continue;
    const v = get(btn.dataset.shortcut); // a field, or a transform's "transforms.2.shortcut"
    btn.replaceChildren(...(v ? keycaps(v) : [h('span', { className: 'none' }, 'None')]));
  }
  for (const b of $$('[data-clear]')) { b.hidden = !get(b.dataset.clear); b.replaceChildren(svg(CROSS)); }
  $('#commandNote').hidden = !S.command_hotkey || !needsGeneral();
  $('#transformsNote').hidden = !needsGeneral();
  $('#modeDesc').textContent = S.mode === 'toggle'
    ? 'Press once to start, again to stop. Esc cancels.'
    : 'Hold while you speak, let go to paste. Tap twice to go hands-free.';
  renderHero();
  if (!$('#onboard').hidden) renderObKeys();
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
// The built-in model can't follow an instruction (cleanup.rs command_blocked): Command Mode and transforms need another.
// The Echotype model edits with Echotype Writer (Models page); until it is downloaded, edits can't run.
const needsGeneral = () => S.cleanup.provider === 'echotype' && !sys?.writer?.installed;
// Click `btn`, press a shortcut: it goes to setting `path` (see renderHotkey), errors to `err`.
function listenShortcut(btn, path, err) {
  btn.addEventListener('click', () => {
    if (recording) { const same = recording.btn === btn; stopRecording(); if (same) return; }
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
      setShortcut(path, r.accel, err);
    };
    recording = { btn, fn };
    window.addEventListener('keydown', fn, true);
    window.addEventListener('keyup', fn, true);
  });
  btn.addEventListener('blur', stopRecording);
}
for (const btn of $$('[data-shortcut]')) listenShortcut(btn, btn.dataset.shortcut, errEl(btn.dataset.shortcut));
for (const b of $$('[data-clear]')) b.addEventListener('click', () => setShortcut(b.dataset.clear, ''));

async function setShortcut(field, accel, err = errEl(field)) {
  const prev = get(field);
  set(field, accel);
  stopRecording();
  const fail = await save(true);
  if (fail) {
    set(field, prev);
    err.textContent = accel ? `${hotkeyText(accel)} didn't work: ${fail}` : fail;
    err.hidden = false;
  }
  renderHotkey();
}

// ---------- transforms ----------
const TRANSLATE = 'Translate to English'; // settings.rs TRANSLATE
const MAX_TRANSFORMS = 12; // settings.rs MAX_TRANSFORMS
// Each: its name, its instruction, its shortcut. Names are unique (settings.rs migrate drops a
// repeated one), so a name another transform has is not saved until it is changed.
function renderTransforms() {
  const list = () => S.transforms; // S is replaced when a save comes back
  fill($('#transforms'), list().map((t, i) => {
    const path = `transforms.${i}.shortcut`, taken = status.unregistered_shortcuts?.includes(`transform:${t.name}`);
    const err = h('span', { className: 'row-desc error', role: 'alert', hidden: !taken }, taken ? `${hotkeyText(t.shortcut)} isn't working: another app may be using it. Pick another.` : '');
    const name = h('input', { type: 'text', value: t.name, maxLength: 40, spellcheck: false, 'aria-label': 'Transform name', oninput: e => {
      const v = e.target.value.trim(), clash = list().some((o, j) => j !== i && o.name.toLowerCase() === v.toLowerCase());
      err.textContent = !v ? 'Give it a name.' : clash ? 'Another transform has this name.' : '';
      err.hidden = !err.textContent;
      if (err.textContent) return;
      if (S.auto_transform === list()[i].name) S.auto_transform = v;
      list()[i].name = v;
      saveSoon();
      renderAutoTransform();
    } });
    const instruction = h('textarea', { rows: 3, value: t.instruction, 'aria-label': `What ${t.name} does`, placeholder: 'What to do with the selected text, for example: turn this into bullet points.',
      oninput: e => { list()[i].instruction = e.target.value; saveSoon(); } });
    const key = h('button', { className: 'hotkey', type: 'button', 'data-shortcut': path, 'aria-label': `${t.name} shortcut` });
    listenShortcut(key, path, err);
    return h('div', { className: 'row transform' }, h('span', { className: 'row-text' }, name, instruction, err),
      h('span', { className: 'actions' }, key,
        h('button', { className: 'icon-btn', type: 'button', 'data-clear': path, 'aria-label': `Turn off the ${t.name} shortcut`, onclick: () => setShortcut(path, '', err) }),
        h('button', { className: 'btn quiet danger', type: 'button', 'aria-label': `Remove ${t.name}`, onclick: () => {
          if (S.auto_transform === t.name && t.name !== TRANSLATE) S.auto_transform = ''; // Translate to English stays built in
          list().splice(i, 1);
          save();
          renderTransforms();
          renderLangNote();
          renderChips();
        } }, 'Remove')));
  }));
  $('#addTransform').disabled = list().length >= MAX_TRANSFORMS;
  renderAutoTransform();
  renderHotkey();
}
// "Apply to every dictation": none, or one transform (TRANSLATE stays offered once its row is gone).
function renderAutoTransform() {
  const names = S.transforms.map(t => t.name);
  if (S.auto_transform === TRANSLATE && !names.includes(TRANSLATE)) names.push(TRANSLATE);
  $('#autoTransform').replaceChildren(h('option', { value: '' }, 'None'), ...names.map(n => h('option', { value: n, selected: n === S.auto_transform }, n)));
  // cleanup.rs run: it only runs with cleanup on, the user's choice to send text to that provider.
  $('#autoTransformDesc').textContent = S.cleanup.enabled ? 'Runs after cleanup on everything you dictate.' : 'Runs after cleanup, so turn Cleanup on to use it.';
  $('#autoTransformDesc').classList.toggle('warn', !S.cleanup.enabled && !!S.auto_transform);
}
$('#autoTransform').addEventListener('change', e => { S.auto_transform = e.target.value; save(); renderLangNote(); renderChips(); });
$('#addTransform').addEventListener('click', () => {
  let name = 'New transform';
  for (let n = 2; S.transforms.some(t => t.name.toLowerCase() === name.toLowerCase()); n++) name = `New transform ${n}`;
  S.transforms.push({ name, instruction: '', shortcut: '' });
  save();
  renderTransforms();
  $$('#transforms textarea').at(-1).focus();
});

// ---------- talk key, Input Monitoring, Globe key ----------
async function refreshStatus() {
  try { status = await invoke('shortcut_status'); } catch { status = {}; }
  renderTalk();
  renderHotkey();
  refreshPerms(); // the Input Monitoring row reads `status`
}
function talkLines() {
  if (S.talk_key === 'accelerator') return [];
  if (status.talk_key_active) return [h('span', {}, S.mode === 'toggle' ? 'On. Press it alone to start, again to stop.' : 'On. Hold it alone to dictate, or tap it twice to go hands-free.')];
  const recheck = h('button', { className: 'btn', type: 'button', onclick: refreshStatus }, 'Check again');
  if (S.talk_key === 'fn' && !status.globe_does_nothing) return [h('span', {}, 'In System Settings, Keyboard, set “Press 🌐 key to” to “Do Nothing”. Until then fn would also open emoji or dictation.'),
    h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind: 'keyboard' }).catch(() => {}) }, 'Open Keyboard Settings'), recheck];
  if (!status.input_monitoring) return [h('span', {}, `Using ${hotkeyText(S.hotkey)} for now: this key needs Input Monitoring.`),
    h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind: 'input_monitoring' }) }, 'Allow Input Monitoring'), recheck];
  if (status.needs_relaunch) return [h('span', {}, 'Relaunch Echotype to finish.'), h('button', { className: 'btn primary', type: 'button', onclick: () => invoke('relaunch') }, 'Relaunch')];
  return [h('span', {}, status.talk_key_error || 'Not active yet.'), recheck];
}
// Allowed, not blocked: say why ⌘C and capitals keep working.
const SHORTCUT_KEYS = { left_command: 'Left ⌘', left_shift: 'Left ⇧' };
const shortcutNote = () => SHORTCUT_KEYS[S.talk_key] ? [h('span', {}, `${SHORTCUT_KEYS[S.talk_key]} is used in many shortcuts. Echotype only reacts when it's pressed alone. `)] : [];
function renderTalk() {
  for (const id of ['#talkStatus', '#obTalkStatus']) {
    const el = $(id), lines = [...shortcutNote(), ...talkLines()];
    el.replaceChildren(...lines);
    el.hidden = !lines.length;
    el.classList.toggle('warn', S.talk_key !== 'accelerator' && !status.talk_key_active);
  }
  $('#obTalk').value = S.talk_key;
  if (!$('#onboard').hidden) obFoot(); // "Finish and relaunch" once the talk key needs it
  renderTransforms(); // a transform's shortcut another app holds
  for (const field of (status.unregistered_shortcuts || []).filter(f => !f.startsWith('transform:'))) {
    const err = errEl(field);
    if (err.hidden) { err.textContent = `${hotkeyText(S[field])} isn't working: another app may be using it. Pick another.`; err.hidden = false; }
  }
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
  writer: ['Get Echotype Writer', () => go('speech')],
  automatic: ['Switch to automatic', () => { S.advanced = false; syncBound(); save(); changed('advanced'); }],
  settings: ['Open Settings', () => go('settings')],
  update: ['Update', () => { go('account'); checkUpdate(); }],
  dictionary: ['Review', () => go('dictionary')],
};
const actionBtn = kind => ACTIONS[kind] && h('button', { className: 'btn', type: 'button', onclick: ACTIONS[kind][1] }, ACTIONS[kind][0]);
let muted = false;
// Per-bar gain of the Home waveform: a soft envelope, tallest in the middle.
const METER = Array.from({ length: 28 }, (_, i) => 0.35 + 0.65 * Math.sin(Math.PI * (i + 0.5) / 28));

// What stops dictation from working right now, with the one action that fixes it.
let perms = null, sttKeySaved = true;
function blocker() {
  if (perms && !perms.accessibility) return { text: "Echotype can't paste yet: Accessibility access is off.", label: 'Allow Accessibility', act: () => invoke('open_permission_settings', { kind: 'accessibility' }) };
  if (status.unregistered_shortcuts?.includes('hotkey') && !status.talk_key_active) return { text: `${hotkeyText(S.hotkey)} isn't working: another app may be using it.`, label: 'Change shortcut', act: () => go('settings') };
  if (perms?.microphone === 'denied') return { text: 'Microphone access is blocked for Echotype.', label: 'Allow microphone', act: () => invoke('open_permission_settings', { kind: 'microphone' }) };
  const p = preset(S.stt.provider);
  if (S.stt.provider === 'cloud') {
    if (cloud.status === 'out' || cloud.status === 'code') return { text: 'Sign in to Echotype Cloud to transcribe.', label: 'Sign in', act: () => go('speech') };
  } else if (S.stt.provider === 'local') {
    const m = sttModel();
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
    line.replaceChildren(muted ? 'No sound from your microphone' : 'Listening');
    fill(sub, muted ? [h('span', {}, 'Echotype hears nothing from it. It keeps recording.'), h('button', { className: 'btn', type: 'button', onclick: () => { go('settings'); $('#mic').focus(); } }, 'Choose microphone')]
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
    else if (st === 'done' && dictation.copied) fill(sub, h('span', {}, dictation.warning || pc('Press ⌘V to paste it.')), chip, ...keptText(), cleanedBtn(), actionBtn(dictation.action));
    else if (st === 'done' && dictation.warning) fill(sub, h('span', {}, dictation.warning), chip, cleanedBtn(), actionBtn(dictation.action) || cleanupFix);
    else fill(sub, h('span', {}, 'Text appears wherever your cursor is.'), chip);
  }
}

// The composer's chips (D15): what Echotype will use right now; each opens its page.
const STYLE_LABEL = { default: 'Default style', casual: 'Casual', formal: 'Formal', verbatim: 'Verbatim' };
function renderChips() {
  const local = S.stt.provider === 'local';
  $('#chipModel').textContent = local ? sttModel()?.label ?? 'No model' : preset(S.stt.provider)?.label ?? S.stt.provider;
  const lang = S.languages.length ? S.languages.map(langName).join(', ') : 'Auto-detect';
  $('#chipLang').textContent = S.auto_transform === TRANSLATE ? `${lang} to English` : lang;
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
  $('#cloudMoved').hidden = !S.cloud_moved;
  $('#rawBanner').hidden = !noCleanup() || S.dismissed.includes('no_cleanup_banner');
  $('#cleanupSetup').hidden = !S.cleanup.enabled || !noCleanup(); // on, but the provider still needs a key
}
$('#rawDismiss').append(svg(CROSS));
$('#cloudMovedOk').append(svg(CROSS));
$('#cloudMovedOk').addEventListener('click', () => { S.cloud_moved = false; save(); renderRaw(); });
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

// The guard pasted the words as spoken: the cleanup it rejected, one tap away.
function cleanedBtn() {
  if (!dictation.cleaned) return null;
  const b = h('button', { className: 'btn', type: 'button' }, 'Copy cleaned version');
  b.onclick = () => copyText(dictation.cleaned, b);
  return b;
}

// The recording is kept when transcription fails: run it again without re-dictating.
const retryBtn = () => h('button', { className: 'btn primary', type: 'button', onclick: () => invoke('retry_last').catch(e => { dictation = { state: 'error', error: String(e) }; renderHero(); }) }, 'Retry');

function renderRecent() {
  const box = $('#recent');
  if (!recent.length) return box.replaceChildren(h('p', { className: 'empty' }, 'Your dictations show up here.'));
  box.replaceChildren(...recent.slice(0, 3).map(r => {
    // A recovered dictation has no text yet: History transcribes it.
    if (r.recovered) return h('div', { className: 'row entry' }, h('span', { className: 'text' }, 'Recovered dictation'), h('button', { className: 'btn quiet', type: 'button', onclick: () => go('history') }, 'Open History'));
    const copy = h('button', { className: 'btn quiet', type: 'button' }, 'Copy');
    copy.onclick = () => copyText(r.text, copy);
    return h('div', { className: 'row entry' }, h('span', { className: 'text' }, r.text.replace(/\s*\n\s*[-*•]?\s*/g, ' · ')), h('time', { className: 'when', title: fmtTime(r.ts).replace('\n', ' ') }, fmtAgo(r.ts)), copy);
  }));
}

// Transcribed files: the queue in files.rs reports each on "file-job" (queued, working, done,
// error) and runs them one at a time; Save as writes any export format to Downloads.
const FILE_FORMATS = [['txt', 'Text (.txt)'], ['md', 'Markdown (.md)'], ['docx', 'Word (.docx)'], ['html', 'Web page (.html)'], ['srt', 'Subtitles (.srt)'], ['vtt', 'WebVTT (.vtt)'], ['json', 'JSON (.json)']];
const fileName = path => path.split(/[\\/]/).pop();
function renderDrops() {
  const dismiss = r => h('button', { className: 'icon-btn', type: 'button', 'aria-label': `Dismiss ${r.name}`, onclick: () => { dropResults = dropResults.filter(x => x !== r); renderDrops(); } }, svg(CROSS));
  const waiting = dropResults.filter(r => r.state === 'queued' && !r.watched).length;
  const left = dropResults.filter(r => r.state === 'queued' || r.state === 'working').length;
  const head = left > 1 && h('div', { className: 'result-head batch-head' },
    h('span', { className: 'result-name working' }, `Transcribing ${left} files, one at a time`),
    waiting > 0 && h('button', { className: 'btn quiet', type: 'button', onclick: cancelFiles }, waiting === 1 ? 'Cancel the next file' : `Cancel the next ${waiting}`));
  $('#dropResults').replaceChildren(head || '', ...dropResults.map((r, i) => {
    if (r.state === 'queued') return h('div', { className: 'result' }, h('div', { className: 'result-head' }, h('span', { className: 'result-name', title: r.path }, r.name), h('span', { className: 'result-meta' }, 'Waiting')));
    if (r.state === 'working') return h('div', { className: 'result' }, h('div', { className: 'result-head' }, h('span', { className: 'result-name working', title: r.path }, `Transcribing ${r.name}`)));
    if (r.state === 'cancelled') return h('div', { className: 'result' }, h('div', { className: 'result-head' }, h('span', { className: 'result-name', title: r.path }, r.name), h('span', { className: 'result-meta' }, 'Cancelled'), dismiss(r)));
    if (r.error) return h('div', { className: 'result err', role: 'alert' },
      h('div', { className: 'result-head' }, h('span', { className: 'result-name' }, `Couldn't transcribe ${r.name}`), dismiss(r)),
      h('p', {}, r.error),
      h('p', { className: 'hint' }, 'Echotype reads .opus, .ogg, .m4a, .mp3, .wav and .flac files, and the sound of .mp4 and .mov videos.'));
    const copy = h('button', { className: 'btn', type: 'button' }, 'Copy');
    copy.onclick = () => copyText(r.showRaw ? r.raw : r.text, copy);
    const saveAs = h('select', { 'aria-label': `Save ${r.name} as`, onchange: e => {
      const format = e.target.value;
      e.target.value = '';
      if (format) invoke('file_save', { path: r.path, text: r.showRaw ? r.raw : r.text, secs: r.secs || 0, format }).then(p => { r.savedTo = p; r.saveError = ''; renderDrops(); }, err => { r.saveError = String(err); renderDrops(); });
    } }, h('option', { value: '' }, 'Save as…'), FILE_FORMATS.map(([v, l]) => h('option', { value: v }, l)));
    const differs = r.raw.trim() !== r.text.trim();
    const seg = differs && h('span', { className: 'seg', role: 'radiogroup', 'aria-label': 'Version' },
      ['Clean', 'Raw'].map(v => h('label', {}, h('input', { type: 'radio', name: 'v' + i, checked: (v === 'Raw') === !!r.showRaw, onchange: () => { r.showRaw = v === 'Raw'; renderDrops(); } }), h('span', {}, v))));
    const secs = ((r.stt_ms || 0) + (r.cleanup_ms || 0)) / 1000;
    const saved = r.savedTo || r.saved;
    return h('div', { className: 'result' },
      h('div', { className: 'result-head' }, h('span', { className: 'result-name', title: r.path }, r.name), h('span', { className: 'result-meta', title: timingTitle(r) }, `${secs.toFixed(1)} s`), seg, copy, saveAs, dismiss(r)),
      h('p', { className: 'text' }, r.showRaw ? r.raw : r.text),
      r.warning && h('p', { className: 'hint' }, r.warning),
      saved && h('p', { className: 'hint' }, `${r.watched && !r.savedTo ? 'From the watched folder. Transcript saved' : 'Saved'} to ${saved}`),
      r.saveError && h('p', { className: 'hint error', role: 'alert' }, r.saveError));
  }));
}
const timingTitle = r => `Speech ${r.stt_ms} ms` + (r.cleanup_ms ? `, cleanup ${r.cleanup_ms} ms` : '');

// A file the queue reports on: new ones go on top; finished ones beyond the latest 20 drop off.
function fileJob(job) {
  let r = dropResults.find(x => x.id === job.id);
  if (!r) dropResults.unshift(r = { name: fileName(job.path), state: 'queued' });
  Object.assign(r, job);
  const busy = x => x.state === 'queued' || x.state === 'working';
  let kept = 0;
  dropResults = dropResults.filter(x => busy(x) || kept++ < 20);
  renderDrops();
}
async function transcribe(paths) {
  try { (await invoke('transcribe_files', { paths })).reverse().forEach(j => dropResults.some(x => x.id === j.id) || fileJob({ ...j, state: 'queued' })); }
  catch (e) { fileJob({ id: `e${Date.now()}`, path: paths[0], state: 'error', error: String(e) }); }
}
async function cancelFiles() {
  const ids = await invoke('cancel_files').catch(() => []);
  for (const r of dropResults) if (ids.includes(r.id)) r.state = 'cancelled';
  renderDrops();
}
listen('file-job', ({ payload }) => fileJob(payload));
$('#pickFiles').onclick = async () => {
  const paths = await invoke('pick_files', { folder: false }).catch(e => { fileJob({ id: `e${Date.now()}`, path: 'Files', state: 'error', error: String(e) }); return []; });
  if (paths.length) transcribe(paths);
};

// ---------- providers & keys ----------
const preset = id => presets.find(p => p.id === id);
const secretName = kind => `${kind === 'stt' ? 'stt' : 'llm'}:${S[kind].provider}`;

function setProvider(kind, id) {
  const p = preset(id), cfg = S[kind];
  cfg.provider = id;
  if (id !== 'custom') cfg.base_url = ''; // presets carry their own URL
  cfg.model = ''; // the user names the model; the preset's shows as a faded example
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
  url.closest('.row').hidden = cfg.provider === 'echotype'; // the built-in model has no URL to show
  url.value = custom ? cfg.base_url : p?.base_url || '';
  url.placeholder = custom ? 'http://localhost:8000/v1' : '';
  $(`#${kind}UrlDesc`).textContent = custom
    ? `Any OpenAI-compatible ${kind === 'stt' ? '/audio/transcriptions' : '/chat/completions'} endpoint.`
    : `Set by the ${p?.label ?? ''} preset.`;
  if (kind === 'cleanup') renderCleaner();
  const model = $(`#${kind}Model`);
  model.value = cfg.model;
  const example = p?.[kind === 'stt' ? 'default_stt_model' : 'default_cleanup_model'];
  model.placeholder = example ? `e.g. ${example}` : 'Model name';
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
    try { await invoke('set_secret', { name, value }); renderKey(kind); }
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
// The speech model in use: the manual pick with advanced settings on, else Automatic's (core: stt::local_model).
const sttModel = () => models.find(m => S.advanced ? m.id === S.stt.local_model : m.automatic);

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
        m.english_only && h('span', { className: 'tag' }, 'English only')),
      m.note && h('span', { className: 'row-desc' }, m.note),
      h('span', { className: 'model-meta' },
        h('span', {}, fmtMB(m.size_mb)),
        h('span', {}, `about ${fmtMB(m.ram_mb)} RAM`),
        !m.fits && h('span', { className: 'badge warn' }, 'Needs more memory')),
      m.warning && h('span', { className: 'row-desc warn' }, m.warning),
      err && h('span', { className: 'row-desc error', role: 'alert' }, err)),
    h('span', { className: 'actions' }, actions));
}

// Advanced settings off: one plain row, "Automatic (recommended)", with what it uses and its download.
function renderAuto() {
  const box = $('#sttAuto');
  box.hidden = S.advanced;
  if (S.advanced) return;
  const rec = models.find(m => m.recommended), cur = sttModel();
  const p = rec && progress[rec.id], err = rec && modelErr[rec.id];
  const desc = !rec ? 'No model fits this Mac.'
    : rec.downloaded ? `Uses ${rec.label}, picked for this Mac.`
    : cur?.downloaded ? `Uses ${cur.label} until ${rec.label}, the pick for this Mac, is downloaded (${fmtMB(rec.size_mb)}).`
    : `${rec.label} suits this Mac. Download it to dictate (${fmtMB(rec.size_mb)}).`;
  const action = !rec || rec.downloaded ? h('span', { className: 'in-use' }, svg(CHECK), 'In use')
    : p != null ? h('span', { className: 'pct', 'data-pct': rec.id }, `${Math.round(p * 100)}%`)
    : h('button', { className: 'btn', type: 'button', onclick: () => download(rec.id) }, err ? 'Retry' : 'Download');
  box.replaceChildren(h('div', { className: 'row' },
    h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, 'Automatic (recommended)'), h('span', { className: 'row-desc' }, desc),
      err && h('span', { className: 'row-desc error', role: 'alert' }, err)),
    h('span', { className: 'actions' }, action)));
}

// Models → Cleanup models: Echotype's cleaners, downloadable once they are hosted (llm::DOWNLOAD_URL).
// The one in use mirrors llm::cleaner: the manual pick with advanced settings on, else Automatic's.
function cleanerRow(c) {
  const list = sys?.cleaners ?? [];
  const current = (S.advanced && list.find(x => x.id === S.cleanup.model)) || list.find(x => x.automatic);
  const inUse = S.cleanup.enabled && S.cleanup.provider === 'echotype' && current?.id === c.id;
  const p = progress[c.id], err = modelErr[c.id];
  const use = () => { S.cleanup.enabled = true; setProvider('cleanup', 'echotype'); S.cleanup.model = c.id; syncBound(); save(); renderModels(); renderRaw(); };
  const del = h('button', { className: 'btn quiet danger', type: 'button' }, 'Delete');
  del.onclick = async () => {
    if (!del.dataset.armed) { del.dataset.armed = 1; del.textContent = 'Click again to delete'; return setTimeout(() => { del.textContent = 'Delete'; delete del.dataset.armed; }, 3000); }
    try { await invoke('delete_model', { id: c.id }); } catch (e) { modelErr[c.id] = String(e); }
    await refreshModels();
  };
  const actions = p != null ? [h('span', { className: 'pct', 'data-pct': c.id }, `${Math.round(p * 100)}%`),
      h('span', { className: 'ring', role: 'progressbar', 'aria-label': `Downloading ${c.label}`, 'aria-valuenow': Math.round(p * 100), 'data-progress': c.id, style: `--p:${p}` })]
    : !c.installed ? [c.downloadable ? h('button', { className: err ? 'btn' : 'round', type: 'button', title: 'Download', 'aria-label': `Download ${c.label}, ${fmtMB(c.size_mb)}`, onclick: () => download(c.id) }, err ? 'Retry' : svg(DOWN))
      : h('span', { className: 'tag' }, 'Download coming soon')]
    : inUse ? [h('span', { className: 'in-use' }, svg(CHECK), 'In use')]
    : [S.advanced && h('button', { className: 'btn', type: 'button', onclick: use }, 'Use'), del];
  return h('div', { className: 'row model' },
    h('span', { className: 'tile', 'aria-hidden': 'true' }, svg(WAVE)),
    h('span', { className: 'row-text' },
      h('span', { className: 'model-name' }, c.label, c.recommended && h('span', { className: 'tag rec' }, 'Recommended')),
      h('span', { className: 'row-desc' }, c.note),
      h('span', { className: 'model-meta' }, h('span', {}, fmtMB(c.size_mb)), h('span', {}, `about ${fmtMB(c.ram_mb)} RAM`)),
      c.warning && h('span', { className: 'row-desc warn' }, c.warning),
      err && h('span', { className: 'row-desc error', role: 'alert' }, err)),
    h('span', { className: 'actions' }, actions));
}

// Models → Writing model: Echotype Writer, which Command Mode and Transforms use with the
// built-in provider. Nothing to pick: installed is ready.
function writerRow(c) {
  const p = progress[c.id], err = modelErr[c.id];
  const del = h('button', { className: 'btn quiet danger', type: 'button' }, 'Delete');
  del.onclick = async () => {
    if (!del.dataset.armed) { del.dataset.armed = 1; del.textContent = 'Click again to delete'; return setTimeout(() => { del.textContent = 'Delete'; delete del.dataset.armed; }, 3000); }
    try { await invoke('delete_model', { id: c.id }); } catch (e) { modelErr[c.id] = String(e); }
    await refreshModels();
  };
  const actions = p != null ? [h('span', { className: 'pct', 'data-pct': c.id }, `${Math.round(p * 100)}%`),
      h('span', { className: 'ring', role: 'progressbar', 'aria-label': `Downloading ${c.label}`, 'aria-valuenow': Math.round(p * 100), 'data-progress': c.id, style: `--p:${p}` })]
    : !c.installed ? [c.downloadable ? h('button', { className: err ? 'btn' : 'round', type: 'button', title: 'Download', 'aria-label': `Download ${c.label}, ${fmtMB(c.size_mb)}`, onclick: () => download(c.id) }, err ? 'Retry' : svg(DOWN))
      : h('span', { className: 'tag' }, 'Download coming soon')]
    : [h('span', { className: 'in-use' }, svg(CHECK), 'Ready'), del];
  return h('div', { className: 'row model' },
    h('span', { className: 'tile', 'aria-hidden': 'true' }, svg(WAVE)),
    h('span', { className: 'row-text' },
      h('span', { className: 'model-name' }, c.label),
      h('span', { className: 'row-desc' }, c.note),
      h('span', { className: 'model-meta' }, h('span', {}, fmtMB(c.size_mb)), h('span', {}, `about ${fmtMB(c.ram_mb)} RAM, only while editing`)),
      c.warning && h('span', { className: 'row-desc warn' }, c.warning),
      err && h('span', { className: 'row-desc error', role: 'alert' }, err)),
    h('span', { className: 'actions' }, actions));
}

function renderModels() {
  $('#cleanerList').replaceChildren(...(sys?.cleaners ?? []).map(cleanerRow));
  $('#writerList').replaceChildren(...(sys?.writer ? [writerRow(sys.writer)] : []));
  renderAuto();
  $('#memline').hidden = $('#modelList').hidden = !S.advanced;
  $('#memline').replaceChildren('Free memory now ', h('b', {}, `${sys.available_ram_gb.toFixed(1)} GB`), ` of ${Math.round(sys.total_ram_gb)} GB. A model fits when it leaves 2 GB for everything else.`);
  $('#modelList').replaceChildren(...models.filter(m => m.id !== 'tiny.en' || m.id === S.stt.local_model).map(m => modelRow(m))); // tiny.en: onboarding's practice model
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
  catch (e) { delete progress[id]; if (String(e) !== 'Download cancelled') modelErr[id] = String(e); renderModels(); }
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
  renderAppStyles();
  renderLevel();
}
// cleanup.rs: None skips the model; Light keeps the model's text only when it barely changed the words.
const LEVEL_DESC = { none: 'No model: your words as heard, with voice commands, snippets and capitals.', light: 'Removes fillers and repeats and fixes punctuation. Never rewords.', medium: 'Also applies “no wait” corrections, lists and grammar.' };
const renderLevel = () => { $('#levelDesc').textContent = LEVEL_DESC[S.cleanup.level] ?? ''; };

// "Match the app you're typing in": the user's style per app, and the app the last dictation went to.
const APP_STYLE_OPTS = [['', 'Your style'], ['default', 'Default'], ['casual', 'Casual'], ['formal', 'Formal'], ['verbatim', 'Verbatim']];
let lastApp = null;
function renderAppStyles() {
  const box = $('#appStyles'), list = () => S.cleanup.app_styles; // S is replaced when a save comes back
  box.hidden = !S.cleanup.match_app;
  const add = lastApp?.id && !list().some(a => a.id === lastApp.id) ? lastApp : null;
  const label = v => APP_STYLE_OPTS.find(o => o[0] === v)?.[1].toLowerCase() ?? v;
  fill(box, list().flatMap((a, i) => [h('div', { className: 'row' },
    h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, a.name), appExtras(a) && h('span', { className: 'row-desc' }, appExtras(a))),
    h('select', { 'aria-label': `Style in ${a.name}`, onchange: e => { list()[i].style = e.target.value; save(); } }, APP_STYLE_OPTS.map(([v, l]) => h('option', { value: v, selected: v === a.style }, l))),
    h('button', { className: 'btn quiet', type: 'button', 'aria-expanded': String(appMore.has(a.id)), onclick: () => { appMore.has(a.id) ? appMore.delete(a.id) : appMore.add(a.id); renderAppStyles(); } }, 'More'),
    h('button', { className: 'icon-btn', type: 'button', 'aria-label': `Remove ${a.name}`, onclick: () => { list().splice(i, 1); save(); renderAppStyles(); } }, svg(CROSS))),
    appMore.has(a.id) && appMoreRow(a, () => list()[i])]).filter(Boolean),
  h('div', { className: 'row' },
    h('span', { className: 'row-text' }, h('span', { className: 'row-desc' }, add ? `Your last dictation went to ${add.name}, which uses ${label(add.style)}.` : 'Pick an app to set its style.')),
    add && h('button', { className: 'btn', type: 'button', onclick: () => { list().push({ id: add.id, name: add.name, style: add.style }); save(); renderAppStyles(); } }, `Add ${add.name}`),
    installed.length > 0 && h('select', { 'aria-label': 'Add an app', onchange: e => {
      const a = installed.find(x => x.id === e.target.value);
      if (a) { list().push({ id: a.id, name: a.name, style: a.style }); save(); }
      renderAppStyles();
    } }, h('option', { value: '' }, 'Add an app…'), installed.filter(a => !list().some(x => x.id === a.id)).map(a => h('option', { value: a.id }, a.name)))));
}
// "More" on an app's row: cleanup on or off and the languages spoken there, over the user's (settings::apply_app).
const appMore = new Set();
const appExtras = a => [a.cleanup === false && 'No cleanup', a.cleanup === true && 'Cleanup on', a.languages?.length && a.languages.map(c => c === 'auto' ? 'Any language' : langName(c)).join(', ')].filter(Boolean).join(', ');
function appMoreRow(a, row) {
  const langs = (a.languages ?? []).join(','), cleanup = a.cleanup == null ? '' : a.cleanup ? 'on' : 'off';
  const changed = () => { save(); renderAppStyles(); };
  return h('div', { className: 'row app-more' },
    h('select', { 'aria-label': `Cleanup in ${a.name}`, onchange: e => { row().cleanup = { on: true, off: false }[e.target.value] ?? null; changed(); } },
      [['', 'Cleanup as set'], ['on', 'Cleanup on'], ['off', 'No cleanup']].map(([v, l]) => h('option', { value: v, selected: v === cleanup }, l))),
    h('select', { 'aria-label': `Languages in ${a.name}`, onchange: e => { row().languages = e.target.value ? e.target.value.split(',') : []; changed(); } },
      h('option', { value: '' }, 'Your languages'),
      langs.includes(',') && h('option', { value: langs, selected: true }, a.languages.map(langName).join(', ')),
      LANGS.map(([v, l]) => h('option', { value: v, selected: v === langs }, v === 'auto' ? 'Any language' : l))));
}
let installed = [];
async function refreshInstalled() {
  installed = await invoke('installed_apps').catch(() => []);
  renderAppStyles();
}
async function refreshLastApp() {
  lastApp = await invoke('last_app').catch(() => null);
  renderAppStyles();
}
$('#cleanupProvider').addEventListener('change', e => setProvider('cleanup', e.target.value));

// The built-in model: "Automatic (recommended)", or with advanced settings a pick of the
// installed Echotype cleaners with their RAM and the low-memory warning. Others are downloaded
// on the Models page; one that was picked and then deleted stays listed so the pick is visible.
function renderCleaner() {
  const builtIn = S.cleanup.provider === 'echotype', list = sys?.cleaners ?? [];
  $('#cleanerRow').hidden = !builtIn;
  $('#cleanupModel').closest('.row').hidden = builtIn;
  if (!builtIn) return;
  const pick = $('#cleanerPick'), auto = list.find(c => c.automatic);
  const label = c => `${c.label}, about ${fmtMB(c.ram_mb)} RAM${c.installed ? '' : ' (not installed)'}`;
  const shown = list.filter(c => c.installed || c.id === S.cleanup.model);
  pick.disabled = !S.advanced;
  pick.replaceChildren(...(S.advanced ? shown.map(c => h('option', { value: c.id }, label(c))) : [h('option', { value: '' }, 'Automatic (recommended)')]));
  const cur = S.advanced ? list.find(c => c.id === S.cleanup.model) ?? auto : auto;
  pick.value = S.advanced ? cur?.id ?? '' : '';
  $('#cleanerDesc').textContent = S.advanced ? cur?.warning ?? ''
    : !auto ? '' : auto.installed ? `Uses ${auto.label}. Runs on this Mac.` : `Uses ${auto.label}, which isn't downloaded yet. Get it on the Models page.`;
  $('#cleanerDesc').classList.toggle('warn', S.advanced && !!cur?.warning);
}
$('#cleanerPick').addEventListener('change', e => { S.cleanup.model = e.target.value; save(); renderCleaner(); });

// Settings → Advanced: the built-in cleaner's memory, idle unload and context, shown only when on.
const MEM_DESC = {
  save: "macOS can free the model's memory when it needs it. The next dictation after that can take up to a second longer.",
  ready: 'Always fast. Keeps about 1 GB in memory while Echotype runs.',
};
function renderAdvanced() {
  $('#advBody').hidden = !S.advanced;
  $('#memDesc').textContent = MEM_DESC[S.cleaner_memory] ?? '';
  $('#ctxPick').value = String(S.cleaner_context);
}
$('#ctxPick').addEventListener('change', e => { S.cleaner_context = Number(e.target.value); save(); });

// ---------- Echotype Cloud ----------
// status: loading | out (email step) | code (code step) | browser (Google/Apple sign-in open in the
// browser) | in (signed in) | error (unreachable, no account yet).
// The API key never reaches this window; Rust keeps it in the keychain.
let cloud = { status: 'loading' }, cloudEmail = '', cloudAttempt = 0;
const usesCloud = () => !!sys?.cloud && (S.stt.provider === 'cloud' || (S.cleanup.enabled && S.cleanup.provider === 'cloud'));
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
    if (cloud.status === 'out' || cloud.status === 'code' || cloud.status === 'browser') return;
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
  if (cloud.status === 'browser') {
    return [cloudRow('Finish signing in in your browser', `Echotype opened ${cloud.provider} sign-in. Come back here once the page says you are signed in.`,
      [h('button', { className: 'btn quiet', type: 'button', onclick: () => { cloudAttempt++; cloud = { status: 'out' }; renderCloud(); } }, 'Cancel')])];
  }
  if (cloud.status === 'out') {
    // A sign-in cancelled (or superseded) here still finishes in Rust; its answer is dropped.
    const provider = (id, name) => h('button', { className: 'btn', type: 'button', onclick: async () => {
      const mine = ++cloudAttempt;
      cloud = { status: 'browser', provider: name };
      renderCloud();
      try {
        const who = await invoke('cloud_sign_in_with', { provider: id });
        if (mine !== cloudAttempt) return;
        cloud = { status: 'in', account: who };
        renderCloud();
        refreshCloud();
      } catch (e) {
        if (mine === cloudAttempt) { cloud = { status: 'out', providerError: String(e) }; renderCloud(); }
      }
    } }, `Continue with ${name}`);
    const email = h('input', { type: 'text', inputMode: 'email', autocomplete: 'email', spellcheck: false, placeholder: 'you@example.com', 'aria-label': 'Email', value: cloudEmail });
    return [cloudRow('Sign in', 'With Google or Apple, or with a code sent to your email. There is no password.', [provider('google', 'Google'), provider('apple', 'Apple')],
      cloud.providerError && h('span', { className: 'row-desc error', role: 'alert' }, cloud.providerError)),
    cloudForm('Or use your email', 'Echotype emails you a 6-digit code.', email, 'Send code', async v => {
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
  renderCleaner(); // the built-in model swaps the Model field for its own picker
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

// Wispr Flow import (onboarding's first question and Settings → Startup): what its data on this
// Mac would bring over, read by the core (wispr.rs). null when Wispr Flow is not installed.
let wispr = null;
const talkLabel = k => $(`#talkKey option[value="${k}"]`)?.textContent ?? k;
function wisprLines(f) {
  const few = (a, n = 4) => a.slice(0, n).join(', ') + (a.length > n ? ` and ${a.length - n} more` : '');
  return [
    f.words.length && ['Dictionary', `${f.words.length} ${f.words.length === 1 ? 'word' : 'words'}: ${few(f.words)}`],
    f.replacements.length && ['Snippets', few(f.replacements.map(r => `“${r.from}”`))],
    f.languages.length && ['Languages', f.languages.map(langName).join(', ')],
    f.app_styles.length && ['Styles', few(f.app_styles.map(a => `${a.style} in ${a.name}`), 3)],
    f.talk_key && ['Talk key', `hold ${talkLabel(f.talk_key)} (quit Wispr Flow so only one listens)`],
    [f.sounds != null && `sounds ${f.sounds ? 'on' : 'off'}`, f.autostart && 'open at login', f.look && `${f.look} look`].some(Boolean)
      && ['Also', [f.sounds != null && `sounds ${f.sounds ? 'on' : 'off'}`, f.autostart && 'open at login', f.look && `${f.look} look`].filter(Boolean).join(', ')],
  ].filter(Boolean);
}
async function wisprImport() {
  const done = await invoke('wispr_import');
  return done.length ? `Imported ${done.join(', ')}.` : 'Nothing new to import: it is all here already.';
}
function renderWispr() {
  $('#wisprRow').hidden = !wispr;
  if (wispr && !$('#wisprImport').disabled) $('#wisprDesc').textContent = wisprLines(wispr).map(([k, v]) => `${k}: ${v}`).join(' · ');
}
invoke('wispr_import_preview').then(f => { wispr = f || null; renderWispr(); }).catch(() => {});
$('#wisprImport').addEventListener('click', async e => {
  const b = e.currentTarget;
  b.disabled = true;
  try { $('#wisprDesc').textContent = await wisprImport(); } catch (err) { $('#wisprDesc').textContent = `Couldn't import: ${err}`; b.disabled = false; }
});
async function obWisprPick(v) {
  ob.wispr = v;
  obTilesRender();
  const lede = $('#obWisprLede');
  if (v !== 'yes') return (lede.textContent = 'Found on this Mac. Wispr Flow itself is not changed.');
  lede.textContent = 'Importing…';
  try {
    lede.textContent = await wisprImport();
    // The name question comes next: start it from the imported name once the save is back.
    setTimeout(() => { if (!$('#obName').value) { $('#obName').value = S.display_name || ''; obHello(); } }, 400);
  } catch (err) { lede.textContent = `Couldn't import: ${err}`; ob.wispr = ''; obTilesRender(); }
}

// Languages: detect any, or listen for up to 3 (each clip goes to the likeliest of them).
const LANG_MAX = 3;
const langName = c => LANGS.find(([v]) => v === c)?.[1] ?? c;
let langFull = false; // a 4th was tapped: say why nothing happened
function langToggle(code) {
  const i = S.languages.indexOf(code);
  langFull = i < 0 && S.languages.length >= LANG_MAX;
  if (i >= 0) S.languages.splice(i, 1);
  else if (!langFull) S.languages.push(code);
  if (!langFull) save();
  changed('languages');
}
function langChips(box) {
  const list = LANGS.filter(([v]) => v !== 'auto');
  box.replaceChildren(...list.map(([v, l]) => h('button', { type: 'button', 'aria-pressed': String(S.languages.includes(v)), 'data-v': v }, l)));
}
function langDesc() {
  return langFull ? `Up to ${LANG_MAX}. Tap one of yours to swap it out.`
    : S.languages.length ? 'Each dictation is typed in whichever of these you spoke.'
    : `Up to ${LANG_MAX}. Picking yours stops short clips coming out in another language.`;
}
// How should your words look: a line in each script for the languages stt.rs ROMANISED can type
// in English letters. The others are always typed in their own script.
const SCRIPT_SAMPLE = {
  hi: ['Namaste, aap kaise hain?', 'नमस्ते, आप कैसे हैं?'],
  mr: ['Namaskar, tumhi kase aahat?', 'नमस्कार, तुम्ही कसे आहात?'],
  ur: ['Assalam alaikum, aap kaise hain?', 'السلام علیکم، آپ کیسے ہیں؟'],
  bn: ['Nomoskar, apni kemon achhen?', 'নমস্কার, আপনি কেমন আছেন?'],
};
const scriptLang = () => S.languages.find(c => SCRIPT_SAMPLE[c]);
function renderLanguages() {
  $('#langAuto').checked = !S.languages.length;
  langChips($('#langChips'));
  $('#langPickDesc').textContent = langDesc();
  $('#langPickDesc').classList.toggle('warn', langFull);
  const c = scriptLang();
  $('#scriptRow').hidden = !c;
  if (c) $('#scriptDesc').textContent = SCRIPT_SAMPLE[c][S.script === 'latin' ? 0 : 1]
    + (S.script === 'latin' && S.cleanup.provider === 'echotype' ? ' The built-in cleanup model is skipped: it was trained on English.' : '');
}
$('#langChips').addEventListener('click', e => { const b = e.target.closest('button'); if (b) langToggle(b.dataset.v); });
$('#langAuto').addEventListener('change', e => {
  langFull = false;
  // Off with nothing picked: start from this Mac's language, so "off" means something.
  const mine = LANGS.find(([v]) => v !== 'auto' && navigator.language?.toLowerCase().startsWith(v))?.[0] ?? 'en';
  S.languages = e.target.checked ? [] : S.languages.length ? S.languages : [mine];
  save();
  changed('languages');
});

// "Type in English whatever language I speak": the Translate to English transform on every
// dictation (cleanup.rs auto_transform). It needs a model that follows instructions.
function renderLangNote() {
  $('#langEnglish').checked = S.auto_transform === TRANSLATE;
  $('#translateNote').hidden = !(S.auto_transform === TRANSLATE && needsGeneral());
}
$('#langEnglish').addEventListener('change', e => {
  S.auto_transform = e.target.checked ? TRANSLATE : S.auto_transform === TRANSLATE ? '' : S.auto_transform;
  save();
  renderLangNote();
  renderTransforms();
  renderChips();
});

// ---------- dictionary ----------
// Placeholders only: what a snippet can be. Nothing is filled in for the user.
const SNIPPET_EXAMPLES = [['my email address', 'you@example.com'], ['my sign-off', 'Thanks, talk soon'], ['what it hears', 'what you meant'], ['my phone number', '+91 98765 43210']];
function renderDict() {
  $('#chips').replaceChildren(...S.dictionary.map((w, i) =>
    h('span', { className: 'chip' }, w, h('button', { type: 'button', 'aria-label': `Remove ${w}`, onclick: () => { S.ignored_words.push(...S.dictionary.splice(i, 1)); save(); renderDict(); $('#newWord').focus(); } }, svg(CROSS))))); // removed: never suggested again
  $('#reps').replaceChildren(...S.replacements.map((r, i) => {
    const field = (k, label) => h('input', { type: 'text', value: r[k], 'aria-label': label, spellcheck: false, placeholder: SNIPPET_EXAMPLES[i % SNIPPET_EXAMPLES.length][k === 'from' ? 0 : 1], oninput: e => { S.replacements[i][k] = e.target.value; saveSoon(); } }); // S, not r: a save's echo replaces S without re-rendering
    return h('div', { className: 'row rep' }, field('from', 'When you say'), field('to', 'Type'),
      h('button', { className: 'icon-btn', type: 'button', 'aria-label': 'Remove snippet', onclick: () => { S.replacements.splice(i, 1); save(); renderDict(); } }, svg(CROSS)));
  }));
}
$('#addWord').addEventListener('submit', e => {
  e.preventDefault();
  const w = $('#newWord').value.trim();
  if (w && !S.dictionary.some(x => x.toLowerCase() === w.toLowerCase())) {
    S.dictionary.push(w);
    S.ignored_words = S.ignored_words.filter(x => x.toLowerCase() !== w.toLowerCase());
    save(); renderDict();
  }
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
  renderRerun();
  const clips = new Set(await invoke('history_clips').catch(() => []));
  if (!items.length) {
    list.replaceChildren(S.history_enabled
      ? h('p', { className: 'empty' }, 'Your dictations show up here. ', verb(), ' ', hotkeyText(S.hotkey), ' in any text field to start.')
      : h('p', { className: 'empty' }, 'History is off, so nothing is kept.', h('button', { className: 'btn', type: 'button', onclick: () => go('settings') }, 'Turn on in Settings')));
    return;
  }
  list.replaceChildren(...items.map(it => {
    if (it.recovered) return recoveredEntry(it, clips.has(it.ts));
    const copy = h('button', { className: 'btn quiet', type: 'button' }, 'Copy');
    copy.onclick = e => { e.preventDefault(); copyText(it.text, copy); };
    const text = h('span', { className: 'text' }, it.text);
    const fix = h('button', { className: 'btn', type: 'button', onclick: () => startFix(it, text, fix) }, 'Fix');
    const restore = h('button', { className: 'btn quiet', type: 'button', onclick: async () => {
      try { await invoke('update_history', { ts: it.ts, text: it.original }); renderHistory(); }
      catch { restore.textContent = "Couldn't restore"; }
    } }, 'Restore');
    return h('details', { ontoggle: e => e.target.open && addRerun(it, fix) },
      h('summary', {}, text, it.no_cleanup && h('span', { className: 'tag' }, 'No cleanup'), h('time', { className: 'when', title: fmtTime(it.ts).replace('\n', ' ') }, fmtAgo(it.ts)), copy),
      h('div', { className: 'detail' }, fix, clips.has(it.ts) && playButton(it.ts),
        it.original && it.original !== it.text && h('p', { className: 'raw text' }, h('b', {}, 'Before re-run: '), it.original, ' ', restore),
        it.raw !== it.text && h('p', { className: 'raw text' }, h('b', {}, 'Heard: '), it.raw, ' ', cleanedCopy(it.raw, 'Copy original')),
        it.cleaned && h('p', { className: 'raw text' }, h('b', {}, 'Cleanup had (not pasted): '), it.cleaned, ' ', cleanedCopy(it.cleaned)),
        h('span', { className: 'timing' },
          h('span', {}, `Speech ${it.stt_ms} ms${it.stt_provider ? ` (${it.stt_provider})` : ''}`),
          it.cleanup_provider && h('span', {}, `Cleanup ${it.cleanup_ms} ms (${it.cleanup_provider})`)),
        it.no_cleanup && noCleanup() && h('p', { className: 'hint' }, 'Kept as you said it because cleanup is off. ', h('button', { className: 'btn quiet', type: 'button', onclick: () => go('cleanup') }, 'Set up cleanup'))));
  }));
}
// Play: the entry's kept audio (settings.rs clips, Settings → Keep dictation audio), fetched as
// bytes and played inline. ponytail: each play's blob URL lives until the window reloads.
function playButton(ts) {
  const b = h('button', { className: 'btn', type: 'button', onclick: async () => {
    let buf;
    try { buf = await invoke('history_audio', { ts }); }
    catch (e) { b.textContent = String(e); b.disabled = true; return; }
    const player = h('audio', { controls: true, src: URL.createObjectURL(new Blob([buf], { type: 'audio/wav' })), 'aria-label': 'Recording' });
    b.replaceWith(player);
    player.play().catch(() => {});
  } }, 'Play');
  return b;
}
// A dictation the app quit or crashed during (settings.rs recover): its audio, to transcribe now.
function recoveredEntry(it, audio) {
  const run = h('button', { className: 'btn primary', type: 'button', onclick: () => {
    rerunPending = true;
    run.disabled = true;
    invoke('rerun', { id: 'same', ts: it.ts }).catch(err => { rerunPending = false; run.disabled = false; dictation = { state: 'error', error: String(err) }; renderHero(); });
  } }, 'Transcribe');
  return h('details', { open: true },
    h('summary', {}, h('span', { className: 'text' }, 'Recovered dictation'), h('span', { className: 'tag' }, 'Not transcribed'), h('time', { className: 'when', title: fmtTime(it.ts).replace('\n', ' ') }, fmtAgo(it.ts))),
    h('div', { className: 'detail' },
      h('p', { className: 'hint' }, audio ? 'Echotype closed while you were dictating. It kept what it recorded.' : 'Echotype closed while you were dictating. The recording is no longer kept.'),
      audio && h('span', { className: 'actions' }, run, playButton(it.ts))));
}
// Fix: edit an entry's text in place, save it, then offer to learn the names it corrected.
function startFix(it, textEl, fixBtn) {
  const area = h('textarea', { value: it.text, rows: Math.min(8, it.text.split('\n').length + 1), 'aria-label': 'Corrected text' });
  const err = h('p', { className: 'row-desc error', role: 'alert', hidden: true });
  const close = () => { form.remove(); fixBtn.hidden = false; fixBtn.focus(); };
  area.onkeydown = e => { if (e.key === 'Escape') close(); else if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) form.requestSubmit(); };
  const form = h('form', { className: 'fix', onsubmit: async e => {
    e.preventDefault();
    const before = it.text, after = area.value.trim();
    if (!after || after === before) return close();
    try { await invoke('update_history', { ts: it.ts, text: after }); }
    catch (e) { err.textContent = `Couldn't save: ${e}`; err.hidden = false; return; }
    it.text = textEl.textContent = after;
    close();
    fixBtn.parentElement.querySelector('.learn')?.remove();
    const tips = fixSuggestions(before, after);
    if (tips.length) fixBtn.after(learnBox(tips));
    invoke('get_history', { limit: 3 }).then(r => { recent = r; renderRecent(); }).catch(() => {});
  } }, area, err, h('div', { className: 'fix-actions' },
    h('button', { className: 'btn', type: 'button', onclick: close }, 'Cancel'),
    h('button', { className: 'btn primary', type: 'submit' }, 'Save')));
  fixBtn.hidden = true;
  fixBtn.after(form);
  area.focus();
}

function learnBox(tips) {
  const ask = (q, label, done, act) => {
    const row = h('p', {}, h('span', {}, q), h('button', { className: 'btn', type: 'button', onclick: () => {
      act(); save(); renderDict();
      row.replaceChildren(h('span', { className: 'key-state' }, svg(CHECK), done));
    } }, label));
    return row;
  };
  return h('div', { className: 'learn' }, tips.flatMap(t => [
    !S.dictionary.some(w => w.toLowerCase() === t.word.toLowerCase())
      && ask(`Add ${t.word} to your Dictionary?`, 'Add', `${t.word} is in your Dictionary.`, () => S.dictionary.push(t.word)),
    t.from && !S.replacements.some(r => r.from.trim().toLowerCase() === t.from.toLowerCase())
      && ask(`Always replace “${t.from}” with “${t.word}”?`, 'Add rule', `“${t.from}” now becomes “${t.word}”.`, () => S.replacements.push({ from: t.from, to: t.word })),
  ]));
}

// Function words: correcting one is grammar, not a name, and a whole-word rule on one
// ("the" → "a") would rewrite nearly every dictation, so neither is ever offered.
const STOP = new Set('a an the and or but if of to in on at by for from with as is are was were be been am i you he she it we they me him her us them my your his its our their this that these those so not no do does did have has had will would can could should just um uh oh ok okay yes yeah hey'.split(' '));
const bare = w => w.replace(/^[^\p{L}\p{N}]+|[^\p{L}\p{N}]+$/gu, ''); // "Xoxoday." → "Xoxoday"
const wkey = w => w.toLowerCase().replace(/[^\p{L}\p{N}]/gu, ''); // "Let's" and "lets" are the same word
// What a fix taught: each substituted run of up to three words whose new form looks like a name
// or term (a capital, or letters with digits like "GPT4", outside the function words; a bare
// number fix is not a term), as { word, from }. `from` is set
// when the wrong form is itself worth a replacement rule. Case-, punctuation- and spacing-only
// edits compare equal under wkey and are never offered.
// ponytail: a capital at the start of a sentence counts as a name; skip it if that proves noisy.
function fixSuggestions(before, after) {
  const a = before.split(/\s+/).filter(Boolean), b = after.split(/\s+/).filter(Boolean);
  const ka = a.map(wkey), kb = b.map(wkey);
  // ponytail: O(n·m) LCS table; a dictation is a few hundred words at most.
  const L = Array.from({ length: a.length + 1 }, () => new Array(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--)
    L[i][j] = ka[i] === kb[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
  const out = [];
  for (let i = 0, j = 0; i < a.length || j < b.length;) {
    if (ka[i] === kb[j]) { i++; j++; continue; }
    const i0 = i, j0 = j;
    while ((i < a.length || j < b.length) && !(i < a.length && ka[i] === kb[j]))
      if (j < b.length && (i === a.length || L[i][j + 1] >= L[i + 1][j])) j++; else i++;
    const from = a.slice(i0, i).map(bare).filter(Boolean), to = b.slice(j0, j).map(bare).filter(Boolean);
    if (!from.length || !to.length || from.length > 3 || to.length > 3) continue;
    if (wkey(from.join('')) === wkey(to.join(''))) continue;
    if (!to.some(w => !STOP.has(w.toLowerCase()) && (/\p{Lu}/u.test(w) || /\p{L}/u.test(w) && /\p{N}/u.test(w)))) continue;
    const word = to.join(' '), wrong = from.join(' ');
    if (out.some(t => t.word === word)) continue;
    out.push({ word, from: from.every(w => STOP.has(w.toLowerCase())) || wkey(wrong).length < 3 ? null : wrong });
  }
  return out;
}

function cleanedCopy(text, label = 'Copy') {
  const b = h('button', { className: 'btn quiet', type: 'button' }, label);
  b.onclick = () => copyText(text, b);
  return b;
}

// Re-run last dictation: the clip Echotype still holds in memory, with one thing changed. The result
// is copied and shown on the pill, and replaces the entry's text here (Restore goes back).
let rerunPending = false;
// An entry's own Re-run, offered when it opens and its audio is still kept (the newest 20).
async function addRerun(it, fix) {
  if (fix.dataset.rerun) return;
  fix.dataset.rerun = '1';
  const opts = await invoke('rerun_options', { ts: it.ts }).catch(() => []);
  if (!opts.length) return;
  const sel = h('select', { className: 'rerun', 'aria-label': 'Re-run this dictation' }, h('option', { value: '' }, 'Re-run…'), ...opts.map(o => h('option', { value: o.id }, o.label)));
  sel.onchange = () => {
    const id = sel.value;
    sel.value = '';
    if (!id) return;
    rerunPending = true;
    invoke('rerun', { id, ts: it.ts }).catch(err => { rerunPending = false; dictation = { state: 'error', error: String(err) }; renderHero(); });
  };
  fix.after(sel);
}
async function renderRerun() {
  const sel = $('#rerunLast');
  const opts = await invoke('rerun_options').catch(() => []);
  sel.replaceChildren(h('option', { value: '' }, 'Re-run last dictation…'), ...opts.map(o => h('option', { value: o.id }, o.label)));
  sel.hidden = !opts.length;
}
$('#rerunLast').addEventListener('change', e => {
  const id = e.target.value;
  e.target.value = '';
  if (!id) return;
  rerunPending = true;
  invoke('rerun', { id }).catch(err => { rerunPending = false; dictation = { state: 'error', error: String(err) }; renderHero(); });
});

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
  for (const sel of [$('#mic'), $('#obMicPick')]) { // Settings, and onboarding's "Wrong microphone?"
    sel.replaceChildren(h('option', { value: '' }, def ? `System default (${def.name})` : 'System default'), ...devices.map(d => h('option', { value: d.name }, d.name)));
    sel.value = S.input_device ?? '';
  }
  renderMicNote();
}
// D13: a persistent info glyph for a Bluetooth mic, plus a one-time tip.
function renderMicNote() {
  let d = devices.find(x => (S.input_device ? x.name === S.input_device : x.default));
  if (!S.input_device && d?.bluetooth && S.prefer_builtin_mic) d = devices.find(x => x.builtin) || d; // audio.rs pick_input
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

// Settings → Files: new recordings in the watched folder get a transcript (files.rs).
function renderWatch() {
  const on = !!S.watch_folder;
  $('#watchDesc').textContent = on
    ? `${S.watch_folder}. New recordings that appear here get a transcript; what was already there is left alone, and your recordings are never changed.`
    : 'Off. Pick a folder and every new audio or video file that appears in it is transcribed. Echotype only reads your recordings.';
  $('#watchOff').hidden = $('#watchFormatRow').hidden = $('#watchSubRow').hidden = !on;
}
$('#watchPick').onclick = async () => {
  $('#watchError').hidden = true;
  try {
    const [dir] = await invoke('pick_files', { folder: true });
    if (dir) { S.watch_folder = dir; save(); renderWatch(); }
  } catch (e) { $('#watchError').textContent = String(e); $('#watchError').hidden = false; }
};
$('#watchOff').onclick = () => { S.watch_folder = ''; save(); renderWatch(); };

function renderStartup() {
  const ok = sys.can_autostart;
  $('#autostart').disabled = !ok && !S.autostart;
  $('#autostartDesc').textContent = ok ? pc('Echotype starts quietly in the menu bar.') : 'Move Echotype to your Applications folder first.';
  $('#obAutostartRow').hidden = !ok;
  $('#talkRow').hidden = $('#obTalkRow').hidden = !isMac; // talk keys are macOS-only; sounds play everywhere
  $('#pillTitle').hidden = $('#pillGroup').hidden = !isMac; // so is the idle pill
  $('#meetingsTitle').hidden = $('#meetingsGroup').hidden = !isMac; // and call detection (calls.rs)
  $('#callAppsTitle').hidden = $('#callAppsGroup').hidden = $('#autoStopRow').hidden = !isMac || !S.detect_meetings;
  $('#preferBuiltinRow').hidden = $('#pauseMusicRow').hidden = !isMac; // and both (audio.rs pick_input, music.rs)
}

// "Hide for 1 hour" shows until when (D12); the hour ends by itself, so re-render then.
let pillTimer;
const HIDE_MS = 3600 * 1000; // pill.rs HIDE_FOR
function renderPill() {
  const bar = S.show_in === 'menubar';
  $('#showInDesc').textContent = bar ? 'No pill on screen. The menu bar icon shows when Echotype listens; click it to dictate, right-click for options.'
    : 'Drag the pill anywhere on any display; double-click it to put it back.';
  $('#pillFollowRow').hidden = $('#pillIdleRow').hidden = $('#pillHideRow').hidden = $('#pillRecRow').hidden = $('#pillLiveRow').hidden = bar;
  $('#pillSizeRow').hidden = $('#ballSizeRow').hidden = $('#pillPreviewRow').hidden = $('#pillFadeRow').hidden = bar;
  for (const id of ['orbListenRow', 'orbThinkRow', 'orbDetailRow', 'orbColorRow']) $('#' + id).hidden = bar;
  $('#soundPackRow').hidden = !S.sounds;
  sizePreview();
  const until = S.pill_hidden_until || 0, hidden = until > Date.now();
  const at = new Date(until).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }); // as the tray says it
  $('#pillHidden').textContent = !S.pill_idle ? 'The pill shows only while you dictate.'
    : hidden ? `Hidden until ${at}. It still shows while you dictate.` : 'The pill still shows while you dictate.';
  $('#pillHide').textContent = hidden ? 'Show now' : 'Hide';
  $('#pillHide').disabled = !S.pill_idle;
  clearTimeout(pillTimer);
  if (hidden) pillTimer = setTimeout(renderPill, Math.min(until - Date.now() + 500, 2 ** 31 - 1));
}
$('#soundPlay').addEventListener('click', () => invoke('preview_sounds', { pack: S.sound_pack || 'classic' }).catch(() => {}));

// Settings → Pill size: the labels follow the sliders; letting go (or Show on screen) has the real
// pill act out a dictation at the new size.
function sizePreview() {
  $('#pillSizeOut').textContent = `${Math.round((S.pill_size ?? 1.5) * 100)}%`;
  $('#ballSizeOut').textContent = `${Math.round((S.ball_size ?? 1) * 100)}%`;
  $('#orbDetailOut').textContent = `${Math.round((S.orb_detail ?? 1) * 100)}%`;
}
const showPill = () => saving.then(() => invoke('show_pill_demo')).catch(() => {});
for (const id of ['pill_size', 'ball_size', 'orb_detail']) {
  const el = $(`[data-bind="${id}"]`);
  el.addEventListener('input', e => { S[id] = Number(e.target.value); sizePreview(); });
  el.addEventListener('change', showPill);
}
$('#pillShow').addEventListener('click', showPill);
// A new orb style or colour plays on the real pill at once.
for (const el of $$('[data-bind="orb_listen"], [data-bind="orb_think"], [data-bind="orb_color"]')) el.addEventListener('change', showPill);

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

// Settings → Use with AI apps: `echotype --mcp` (mcp.rs), as Claude Desktop and Cursor configure a server.
const MCP_CONFIG = JSON.stringify({ mcpServers: { echotype: { command: '/Applications/Echotype.app/Contents/MacOS/echotype', args: ['--mcp'] } } }, null, 2);
$('#mcpConfig').textContent = MCP_CONFIG;
$('#copyMcp').addEventListener('click', e => copyText(MCP_CONFIG, e.currentTarget));

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
  const isWin = /windows/i.test(sys.os);
  const rows = () => failed
    ? [h('p', { className: 'empty', role: 'alert' }, isMac ? "Couldn't check permissions. Open System Settings, Privacy & Security, to review Microphone and Accessibility." : "Couldn't check permissions.")]
    : !isMac ? [
      row('Microphone', isWin ? 'If Echotype hears nothing, turn on microphone access for desktop apps in Windows Settings.' : 'If Echotype hears nothing, pick another microphone in Settings.',
        isWin ? ['Set in Windows', 'muted', true] : ['No permission needed', 'muted', false], 'microphone'),
      // Wayland: no app may press keys in another, and the shortcut reaches Echotype only from X11 apps.
      ...(p.wayland ? [
        row('Pasting on Wayland', p.paste_tool ? `Echotype presses Ctrl+V with ${p.paste_tool}.` : 'Install wtype (or ydotool with its daemon) so Echotype can paste. Until then your text waits on the clipboard.',
          p.paste_tool ? ['Ready', '', false] : ['Needs wtype', 'warn', false]),
        row('Shortcut on Wayland', 'The shortcut works while an X11 app is focused. To dictate anywhere, bind the command echotype --toggle to a key in your desktop’s keyboard settings.', ['Limited', 'muted', false]),
      ] : []),
    ] : [
      row('Microphone', p.microphone === 'unknown' ? 'macOS asks the first time you dictate.' : 'To hear you while the shortcut is down.', MIC[p.microphone] ?? MIC.unknown, 'microphone'),
      row('Accessibility', 'Echotype uses it only to press ⌘V in the app you are using.', p.accessibility ? ['Allowed', '', false] : ['Needed to paste', 'warn', true], 'accessibility'),
    ];
  const im = isMac && S.talk_key !== 'accelerator' && [row('Input Monitoring', 'Only for the talk key: Echotype watches that one key, and never records what you type.',
    status.input_monitoring ? ['Allowed', '', false] : ['Needed for the talk key', 'warn', true], 'input_monitoring')];
  $('#perms').replaceChildren(...rows(), ...(im || []));
  $('#axLine').hidden = failed || p.accessibility;
  renderHero();
}
window.addEventListener('focus', refreshPerms); // coming back from System Settings
window.addEventListener('focus', () => S && refreshStatus()); // Input Monitoring or the Globe key may have changed
for (const b of $$('[data-perm]')) b.addEventListener('click', () => invoke('open_permission_settings', { kind: b.dataset.perm }));

// ---------- onboarding ----------
// Six steps in one card (.notes/ONBOARDING.md): the microphone with a sound check, the user's
// languages, a practice dictation into a field here, Accessibility and a real paste in another app, a few personal
// touches, then the recap. The orb (orb.js) moves between the steps' .ob-orb slots. The speech
// model downloads from the first frame, tiny.en first when nothing is on this Mac yet, so the
// practice run needn't wait for the big one (Automatic uses whatever is downloaded).
const obStill = matchMedia('(prefers-reduced-motion: reduce)').matches;
const SAY = 'Echotype, meet my Mac.';
const BUSY = ['recording', 'transcribing', 'cleaning'];
const wait = ms => new Promise(r => setTimeout(r, ms));
// Per-viewer memory that may be missing (private storage): only what makes cleanup tidier.
const local = (k, v) => { try { if (v === undefined) return localStorage.getItem(k); if (v === null) localStorage.removeItem(k); else localStorage.setItem(k, v); } catch { return null; } };
let obStep = 1, orb = null, obPoll = 0, obSeq = 0;
const obT = {}; // timers by name
const ob = { cleanHow: 'off', mic: null, heard: 0, checked: false, quiet: false, paused: false, ax: null, axSince: 0, asked: false, pressed: false, offerClick: false, watched: false, recAt: 0, secs: 0, wispr: '' };
const later = (name, ms, fn) => { clearTimeout(obT[name]); obT[name] = setTimeout(fn, ms); };
// Show something that appears within a step with a short rise.
function pop(el, on = true) {
  if (el.hidden === !on) return;
  el.hidden = !on;
  el.classList.remove('ob-pop');
  if (on) { void el.offsetWidth; el.classList.add('ob-pop'); }
}

function showOnboarding(on) {
  $('#onboard').hidden = !on;
  $('aside').inert = $('main').inert = on; // keep Tab inside the dialog
  document.documentElement.classList.toggle('ob-on', on);
}

// ---------- notices and retired versions (notices.rs, app.json in the models bucket) ----------
let notices = { notices: [], blocked: false };
async function refreshNotices() {
  notices = await invoke('app_notices').catch(() => notices);
  renderNotice();
}
// One notice at a time above Account and Settings: its button and a ✕ that dismisses it for good.
function renderNotice() {
  const box = $('#sideNotice');
  const n = languageOffer() || (notices.notices || []).find(x => !S.dismissed.includes(`notice:${x.id}`));
  $('#retired').hidden = !notices.blocked;
  if (S.learned.length) return learnedNotice(box);
  box.hidden = !n;
  if (!n) return box.replaceChildren();
  box.replaceChildren(
    h('button', { className: 'icon-btn notice-x', type: 'button', 'aria-label': `Dismiss: ${n.title}`, onclick: () => { n.dismiss ? n.dismiss() : dismiss(`notice:${n.id}`); renderNotice(); } }, svg(CROSS)),
    h('b', {}, n.title),
    n.body && h('p', {}, n.body),
    n.action_label && n.action && h('button', { className: 'btn', type: 'button', onclick: () => noticeAction(n.action) }, n.action_label));
}
// Words learn.rs picked up from the user's own corrections: kept into the Dictionary or, on Undo,
// never offered again. Shown before any app.json notice.
function learnedNotice(box) {
  const w = S.learned, one = w.length === 1;
  const done = keep => {
    if (keep) S.dictionary.push(...w.filter(x => !S.dictionary.some(d => d.toLowerCase() === x.toLowerCase())));
    else S.ignored_words.push(...w);
    S.learned = [];
    save(); renderDict(); renderNotice();
  };
  box.hidden = false;
  box.replaceChildren(
    h('b', {}, `Echotype learned ${one ? 'a word' : `${w.length} words`}`),
    h('p', {}, `${w.join(', ')}. Keep ${one ? 'it' : 'them'}?`),
    h('div', { className: 'notice-btns' },
      h('button', { className: 'btn', type: 'button', onclick: () => done(true) }, 'Keep'),
      h('button', { className: 'btn quiet', type: 'button', onclick: () => done(false) }, 'Undo')));
}

// stt.rs language_to_offer: a language the detector heard clearly, offered once as a notice of our own.
function languageOffer() {
  const c = S.language_offer;
  if (!c || S.languages.includes(c)) return null;
  let name = langName(c);
  if (name === c) try { name = new Intl.DisplayNames(['en'], { type: 'language' }).of(c); } catch {}
  const done = add => { if (add && S.languages.length < LANG_MAX) S.languages.push(c); S.language_offer = ''; save(); renderNotice(); renderLanguages(); renderChips(); };
  return { title: `Sounds like you speak ${name}`, body: 'Add it to your languages?', action_label: 'Add', action: () => done(true), dismiss: () => done(false) };
}
// "update", "page:<page>" or an https link; anything else does nothing.
function noticeAction(a) {
  if (typeof a === 'function') return a();
  if (a === 'update') return ACTIONS.update[1]();
  if (a.startsWith('page:')) return go(a.slice(5));
  if (/^https:\/\//.test(a)) invoke('plugin:opener|open_url', { url: a }).catch(() => {});
}
listen('notices', refreshNotices);
$('#retiredUpdate').addEventListener('click', async e => {
  e.target.disabled = true; $('#retiredText').textContent = 'Downloading the update…';
  try { await invoke('install_update'); }
  catch (err) { $('#retiredText').textContent = String(err); e.target.disabled = false; }
});

// Account → Version → Updates: check GitHub Releases, then install and restart on request.
async function checkUpdate() {
  const desc = $('#updateDesc'), btn = $('#checkUpdate');
  btn.disabled = true; desc.textContent = 'Checking…'; desc.classList.remove('error');
  try {
    const u = await invoke('check_update');
    desc.textContent = u.version ? `Echotype ${u.version} is available (you have ${u.current}).` : `Echotype ${u.current} is the latest version.`;
    $('#installUpdate').hidden = !u.version;
  } catch (e) { desc.textContent = String(e); desc.classList.add('error'); }
  btn.disabled = false;
}
$('#checkUpdate').addEventListener('click', checkUpdate);
$('#installUpdate').addEventListener('click', async e => {
  const desc = $('#updateDesc');
  e.target.disabled = true; desc.textContent = 'Downloading the update…';
  try { await invoke('install_update'); } catch (err) { desc.textContent = String(err); desc.classList.add('error'); e.target.disabled = false; }
});
$('#rerunSetup').addEventListener('click', () => startOnboarding(1));

function startOnboarding(step) {
  showOnboarding(true);
  window.__TAURI__.event.emit?.('main-page', 'onboarding'); // the real pill may show over this window
  orb ??= createOrb($('#obCanvas'), obStill);
  // Setup from the start on macOS (a first run, or Run setup again) opens with the film, which
  // brings the card in (?film plays it in a browser).
  if (step === 1 && !obStill && ((isMac && !preview) || new URLSearchParams(location.search).has('film'))) return obFilm();
  orb.set(obStill ? 'orb' : 'ball');
  orb.start();
  // The ball bursts into the orb once the card has come in, then breathes.
  if (!obStill) later('burst', 520, () => orb.kind === 'ball' && orb.set('orb'));
  $('.ob-card').classList.add('intro');
  obStartModel();
  obShow(step, true);
}

// ----- the first-run film (film.js) -----
// The window turns into a clear stage over the whole screen: stars over the desktop collapse into
// the ball, the ball becomes the orb, the card grows out from behind it and says welcome, and the
// orb settles into the card, where setup takes over. At the end the card shrinks away and the dots
// fall to the pill's place, and the pill takes over from them. Only the card takes clicks.
let film = null, filmHome = null;
const filming = () => document.documentElement.classList.contains('film');
const filmDrag = [];
// The pill's ball in this page's px (the stage is unzoomed; Rust knows where the stage is).
async function homeSpot() {
  const h = await invoke('film_home').catch(() => null);
  return h ? { x: h[0], y: h[1], r: h[2] } : null;
}
function filmHit(on = true) {
  const r = $('.ob-card').getBoundingClientRect();
  invoke('film_hit', { rect: on ? [r.left, r.top, r.width, r.height] : null }).catch(() => {});
}
// The card's orb in or out over ms, as the film's dots hand over to it (and back).
function orbFade(from, to, ms) {
  return new Promise(done => {
    const t0 = performance.now();
    const tick = now => { const k = Math.min(1, (now - t0) / ms); orb.fade(from + (to - from) * k); k < 1 ? requestAnimationFrame(tick) : done(); };
    requestAnimationFrame(tick);
  });
}
async function obFilm() {
  const root = document.documentElement;
  root.classList.add('film', 'film-pre');
  for (const el of $$('#onboard [data-tauri-drag-region]')) { filmDrag.push(el); el.removeAttribute('data-tauri-drag-region'); } // the stage never moves
  await invoke('film_window', { on: true, show: true }).catch(() => {});
  await wait(80); // the stage's size reaches the page
  const { createFilm, CONTENT } = await import('./film.js');
  filmHome = await homeSpot();
  film = createFilm({
    card: $('.ob-card'), slot: () => $('.ob-step:not([hidden]) .ob-orb'), shell: $('#filmShell'), welcome: $('#filmWelcome'),
    home: () => filmHome, light: !matchMedia('(prefers-color-scheme: dark)').matches,
  });
  obStartModel();
  obShow(1, true);
  const played = film.play();
  later('film', CONTENT * 1000, () => { root.classList.remove('film-pre'); filmHit(); });
  addEventListener('resize', () => filming() && !root.classList.contains('film-pre') && filmHit());
  await played;
  // Setup takes over: the card's own orb, exactly where the film's dots settled.
  orb.fade(0);
  orb.set('orb');
  orb.start();
  film.show(false);
  orbFade(0, 1, 300);
}
// The end: the card's content and orb go, the film takes the dots home to the pill.
async function obFilmHome() {
  filmHit(false);
  document.documentElement.classList.add('film-out');
  filmHome = (await homeSpot()) ?? filmHome; // the pill may be a new size since
  await orbFade(1, 0, 250);
  orb.stop();
  await film.home();
}
// The stage becomes the settings window again (hidden; `show` brings it up), and the pill shows.
async function obFilmEnd(show) {
  film?.destroy();
  film = null;
  await invoke('film_window', { on: false, show: false }).catch(() => {});
  document.documentElement.classList.remove('film', 'film-pre', 'film-out');
  for (const el of filmDrag.splice(0)) el.setAttribute('data-tauri-drag-region', '');
  fitZoom();
  if (show) invoke('focus_main').catch(() => {});
}

// ----- moving between steps -----
// View Transitions where the WebView has them: the old step leaves as a snapshot while the new
// one's children come in, staggered, live. Without them the same in two beats; reduced motion
// swaps at once and fades. The orb is outside the steps and follows its slot either way.
function obShow(n, first = false) {
  n = Math.max(1, Math.min(6, n));
  // Step 4 is macOS's Accessibility switch; elsewhere pasting needs nothing, so the flow steps over it.
  if (n === 4 && !isMac) n = n > obStep ? 5 : 3;
  const from = $(`.ob-step[data-step="${obStep}"]`), to = $(`.ob-step[data-step="${n}"]`);
  document.documentElement.classList.toggle('ob-back', n < obStep);
  obStep = n;
  const seq = ++obSeq;
  const swap = () => {
    if (seq !== obSeq) return;
    for (const s of $$('.ob-step')) { s.hidden = s !== to; s.classList.remove('enter', 'leave'); }
    let i = 0;
    for (const el of to.querySelectorAll(':scope > :not(.ob-orb), :scope > .ob-body > *')) el.style.setProperty('--i', i++);
    void to.offsetWidth;
    to.classList.add('enter');
    obEnter(n);
  };
  if (first || from === to || obStill) swap();
  else if (document.startViewTransition) document.startViewTransition(swap);
  else { from.classList.add('leave'); setTimeout(swap, 140); }
}

function obEnter(n) {
  const to = $(`.ob-step[data-step="${n}"]`);
  if (S.onboard_step !== n) { S.onboard_step = n; save(true); } // a relaunch resumes here
  if (filming()) invoke('film_float', { on: n !== 4 }).catch(() => {}); // step 4 brings System Settings in front
  $$('#obDots li').forEach((li, i) => { li.className = i + 1 === n ? 'on' : i + 1 < n ? 'done' : ''; });
  $('#obDots').ariaLabel = `Setup, step ${n} of 6`;
  orb.slot(to.querySelector('.ob-orb'));
  orb.progress(null);
  for (const k of ['adv', 'click', 'quiet', 'pause']) clearTimeout(obT[k]);
  if (n !== 1) obListen(false);
  // Permissions are polled while their step shows, so a grant in System Settings carries on by itself.
  clearInterval(obPoll);
  if (n === 1 || n === 4) { obPoll = setInterval(obTick, n === 1 ? 700 : 500); obTick(); }
  if (n === 1) { if (ob.mic === 'granted' && !ob.checked && !ob.listening) obListen(true); obMicRender(); } // back here before the check finished
  if (n === 2) obLangsRender();
  if (n === 3) {
    renderObKeys();
    obTryRender();
    if (!ob.pressed) later('click', 8000, () => { ob.offerClick = true; obFoot(); }); // the hotkey never fired
  }
  if (n === 4) { ob.axSince = Date.now(); $('#obStale').hidden = true; renderObKeys(); obAxRender(); }
  if (n === 5) obPrefsRender();
  else orb.style(null); // step 5 wears the user's orb; the other steps the default, in ink
  if (n === 6) obDoneRender();
  if (n !== 1 && !BUSY.includes(dictation.state)) orb.set('orb');
  obFoot();
  renderObModel();
  to.querySelector('h2').focus({ preventScroll: true });
}

// The footer: Back (Quit on the first step), one quiet extra, and the primary action.
function obFoot() {
  if ($('#onboard').hidden) return;
  const next = $('#obNext'), alt = $('#obAlt');
  $('#obBack').textContent = obStep === 1 ? 'Quit' : 'Back';
  $('#obSkip').hidden = obStep === 1 || obStep === 6;
  let label = 'Continue', extra = '';
  next.hidden = next.disabled = false;
  if (obStep === 1) {
    if (ob.mic === 'denied') label = 'Open System Settings';
    next.hidden = ob.mic === 'restricted'; // an administrator decides; nothing here can help
    next.disabled = ob.mic === 'asking' || ob.mic == null;
    if (ob.mic === 'granted' && !ob.checked) extra = ob.paused ? 'Listen again' : $('#obMicPickRow').hidden ? 'Wrong microphone?' : '';
  } else if (obStep === 2) {
    extra = 'Skip for now';
  } else if (obStep === 3) {
    if (ob.offerClick) extra = dictation.state === 'recording' ? 'Click to finish' : 'Click to start instead';
  } else if (obStep === 4) {
    if (!ob.ax) { label = 'Open System Settings'; extra = 'Not now: keep text on the clipboard'; }
  } else if (obStep === 5) {
    if (obQ < obQs().length - 1) extra = 'Skip the rest';
  } else if (obStep === 6) label = obRelaunch() ? 'Finish and relaunch' : 'Finish';
  next.textContent = label;
  alt.textContent = extra;
  alt.hidden = !extra;
}
const obRelaunch = () => isMac && S.talk_key !== 'accelerator' && !!status.needs_relaunch;

$('#obNext').addEventListener('click', async () => {
  if (obStep === 1) {
    if (ob.mic === 'denied') return invoke('open_permission_settings', { kind: 'microphone' }).catch(() => {});
    if (ob.mic === 'unknown') {
      ob.mic = 'asking';
      obMicRender();
      await invoke('request_microphone').catch(() => {});
      if (!isMac) { ob.mic = 'granted'; obListen(true); obMicRender(); } // no prompt to wait for
      return obTick();
    }
  }
  if (obStep === 4 && !ob.ax) {
    ob.asked = true;
    obAxRender();
    return invoke('open_permission_settings', { kind: 'accessibility' }).catch(() => {});
  }
  if (obStep === 6) return obFinish();
  if (obStep === 5 && obQ < obQs().length - 1) return obQShow(obQ + 1);
  obShow(obStep + 1);
});
$('#obAlt').addEventListener('click', () => {
  if (obStep === 1 && ob.paused) { obListen(true); return obMicRender(); }
  if (obStep === 1) { pop($('#obMicPickRow')); $('#obMicPick').focus(); return obFoot(); }
  if (obStep === 3) return invoke('toggle_dictation').catch(() => {});
  if (obStep === 2 || obStep === 4 || obStep === 5) obShow(obStep + 1); // skip for now, not now, skip the rest
});
// Back (and Esc) walk back through step 5's questions before leaving it.
const obBackOne = () => (obStep === 5 && obQ > 0 ? obQShow(obQ - 1) : obShow(obStep - 1));
$('#obBack').addEventListener('click', () => (obStep === 1 ? invoke('quit_app').catch(() => {}) : obBackOne()));
$('#obSkip').addEventListener('click', () => finishOnboarding('home'));
// Enter is the primary action; Esc and ⌘[ go back. Esc never closes onboarding, and does
// nothing while a dictation is running (Rust has Esc then, to cancel it).
$('#onboard').addEventListener('keydown', e => {
  if (document.documentElement.classList.contains('film-pre')) return e.preventDefault(); // the film is still bringing the card in
  const back = e.key === 'Escape' || (e.metaKey && e.key === '[');
  if (back) {
    e.preventDefault();
    if (obStep > 1 && !BUSY.includes(dictation.state)) obBackOne();
  } else if (e.key === 'Enter' && !e.isComposing && !e.target.closest('button, select, a, textarea, input[type="checkbox"], input[type="radio"]')) {
    e.preventDefault();
    const n = $('#obNext');
    if (!n.hidden && !n.disabled) n.click();
  }
});

async function obTick() {
  let p;
  try { p = await invoke('check_permissions'); } catch { return; }
  if (obStep === 1) obMicStatus(p);
  if (obStep === 4) obAxStatus(!!p.accessibility);
}

// ----- 1: the microphone -----
function obMicStatus(p) {
  const was = ob.mic;
  let m = p.microphone_restricted ? 'restricted' : p.microphone;
  if (m === 'unknown' && was === 'asking') m = 'asking'; // the system prompt is up
  if (m === was) return;
  ob.mic = m;
  if (m === 'granted') {
    if (was === 'denied') invoke('focus_main').catch(() => {}); // granted in System Settings: come back
    obListen(true);
  } else obListen(false);
  obMicRender();
}

// The sound check: the orb follows the mic's level; about 0.8 s of voice and it's done.
function obListen(on) {
  for (const k of ['quiet', 'pause']) clearTimeout(obT[k]);
  if (!on) { if (ob.listening) invoke('mic_check', { on: false }).catch(() => {}); ob.listening = false; orb?.level(0); return; }
  ob.listening = true;
  ob.heard = 0;
  ob.quiet = ob.paused = false;
  invoke('mic_check', { on: true }).catch(e => { $('#obMicState').textContent = `Couldn't open the microphone: ${e}`; });
  later('quiet', 10000, () => { ob.quiet = true; pop($('#obMicPickRow')); obMicRender(); });
  later('pause', 60000, () => { obListen(false); ob.paused = true; obMicRender(); });
}
listen('mic-level', ({ payload }) => {
  if (obStep !== 1 || !ob.listening || ob.checked) return;
  orb.level(payload.meter);
  if (payload.meter > 0.25) ob.heard += 50; // 20 windows a second
  if (ob.heard < 800) return;
  ob.checked = true;
  obListen(false);
  orb.set('check');
  invoke('play_sound', { name: 'start' }).catch(() => {});
  obMicRender();
  later('adv', 1100, () => obStep === 1 && obShow(2));
});
$('#obMicPick').addEventListener('change', e => {
  S.input_device = e.target.value || null;
  syncBound();
  renderMicNote();
  save().then(() => obStep === 1 && ob.mic === 'granted' && !ob.checked && obListen(true));
});

const MIC_LINE = {
  asking: 'Answer the macOS prompt to carry on.',
  denied: "Microphone access is off for Echotype. Switch it on in System Settings and this page carries on by itself.",
  restricted: "This Mac's administrator controls microphone access, so Echotype can't turn it on.",
};
function obMicRender() {
  const st = $('#obMicState');
  st.textContent = ob.mic !== 'granted' ? MIC_LINE[ob.mic] ?? ''
    : ob.checked ? 'Got it. That’s your voice.'
    : ob.paused ? 'Stopped listening after a minute of quiet.'
    : ob.quiet ? 'Nothing yet. Is this the right microphone?'
    : 'Say something. The orb is listening.';
  st.dataset.tone = ob.mic === 'denied' || ob.mic === 'restricted' ? 'warn' : ob.checked ? 'ok' : '';
  if (obStep === 1 && orb.kind !== 'ball') orb.set(ob.checked ? 'check' : 'orb');
  obFoot();
}

// ----- 2: languages -----
// Two menus over `languages` (Auto-detect is the empty list; a third picked in Settings stays) and,
// for a language stt.rs can type in English letters, the script choice with a line in it.
function obLangsRender() {
  const [a = 'auto', b = ''] = S.languages, opt = ([v, l]) => h('option', { value: v }, l);
  $('#obLang1').replaceChildren(...LANGS.map(opt));
  $('#obLang2').replaceChildren(opt(['', 'None']), ...LANGS.filter(([v]) => v !== 'auto' && v !== a).map(opt));
  $('#obLang1').value = a;
  $('#obLang2').value = b;
  $('#obLang2').disabled = a === 'auto';
  const c = scriptLang();
  pop($('#obScriptRow'), !!c); // rises in when a language with English letters is picked
  if (c) $('#obScriptSample').textContent = SCRIPT_SAMPLE[c][S.script === 'latin' ? 0 : 1];
}
for (const el of [$('#obLang1'), $('#obLang2')]) el.addEventListener('change', () => {
  const a = $('#obLang1').value;
  S.languages = a === 'auto' ? [] : [...new Set([a, $('#obLang2').value, ...S.languages.slice(2)].filter(Boolean))].slice(0, LANG_MAX);
  save();
  changed('languages');
});

// ----- 3: try it -----
function renderObKeys() {
  $('#obSay').replaceChildren(`${verb()} `, h('span', { className: 'keys' }, talkCaps()), ' and say: ', h('q', {}, SAY));
  $('#obKeys').replaceChildren(...talkCaps().map(k => { k.classList.toggle('wide', k.textContent.length > 2); return k; }));
  $('#obTip').replaceChildren('Tip: tap ', h('span', { className: 'keys' }, talkCaps()), ' twice to keep talking hands-free.');
}
// The model line counts the download; the orb wears a ring until a model is ready.
function obTryRender() {
  const ready = obReady(), m = obDownloading();
  orb.progress(ready ? null : progress[m?.id] ?? 0);
  const st = $('#obTryState'), wait = 'One moment… getting your speech model ready.';
  if (!ready && !ob.tried && !BUSY.includes(dictation.state)) st.textContent = wait;
  else if (ready && st.textContent === wait) st.textContent = '';
}
listen('hotkey-state', ({ payload: down }) => {
  $('#obKeys').classList.toggle('down', !!down);
  if (down && obStep === 3) { ob.pressed = true; clearTimeout(obT.click); }
});

function obDictation(p) {
  if ($('#onboard').hidden) return;
  if (obStep === 3) obPractice(p);
  else return;
  if (p.state !== 'recording') orb.level(0);
  obFoot();
}
const obHint = m => /^(Hold|Press) /.test(m || '') ? 'Hold the keys while you talk, then let go.'
  : /^Model downloading/.test(m || '') ? 'One moment… the speech model is still downloading.'
  : m === "Didn't catch that" ? "Didn't catch that. Hold the keys, talk, then let go." : m || '';
function obPractice(p) {
  const say = t => ($('#obTryState').textContent = t);
  $('#obTryState').dataset.tone = '';
  if (p.state === 'recording') {
    ob.recAt = performance.now();
    ob.offerClick = ob.offerClick && !ob.pressed;
    orb.progress(null);
    orb.set('orb');
    if (!ob.watched && (S.onboarded || !isMac)) { // once: the real pill is down there (a first run on macOS has none yet)
      ob.watched = true;
      const w = $('#obWatch');
      w.hidden = false;
      w.classList.add('pulse');
    }
    say(p.locked || S.mode === 'toggle' ? 'Listening. Press the keys again to finish.' : 'Listening… let go when you’re done.');
  } else if (p.state === 'transcribing' || p.state === 'cleaning') {
    if (ob.recAt) ob.secs = (performance.now() - ob.recAt) / 1000;
    ob.recAt = 0;
    orb.set('think');
    say('Writing it down…');
  } else if (p.state === 'done' && p.text) {
    ob.tried = true;
    obReveal(p.text);
    orb.set('check');
    later('orb', 1600, () => obStep === 3 && !BUSY.includes(dictation.state) && orb.set('orb'));
    const n = p.text.split(/\s+/).filter(Boolean).length;
    say(`That’s it. ${n} ${n === 1 ? 'word' : 'words'}${ob.secs >= 0.3 ? ` in ${ob.secs.toFixed(1)} s` : ''}.`);
    $('#obTryState').dataset.tone = 'ok';
    $('#obWatch').hidden = true;
    pop($('#obTip'), isMac && S.mode !== 'toggle');
    $('#obNext').focus();
  } else if (p.state === 'hint' || p.state === 'error') {
    orb.set('orb');
    say(p.state === 'hint' ? obHint(p.message) : p.error || 'Something went wrong. Try again.');
    if (p.state === 'hint') obTryRender();
  }
}
// Word by word: 30 ms apart, the whole line within 600 ms. Reduced motion: all at once.
function obReveal(text) {
  const words = text.split(/\s+/).filter(Boolean), step = Math.min(30, 600 / words.length);
  $('#obResult').replaceChildren(...words.flatMap((w, i) => [i ? ' ' : '', h('span', { className: 'w', style: `--d:${Math.round(i * step)}ms` }, w)]));
}

// ----- 4: Accessibility -----
// Once it's on, the step says so and moves on by itself; no trial paste in another app.
function obAxStatus(on) {
  const was = ob.ax;
  ob.ax = on;
  if (!on && Date.now() - ob.axSince > 20000) pop($('#obStale')); // "on" in the list, off for macOS
  if (was === on) return;
  if (on && was === false) {
    invoke('focus_main').catch(() => {});
    invoke('play_sound', { name: 'lock' }).catch(() => {});
    orb.set('check');
    later('orb', 1400, () => obStep === 4 && !BUSY.includes(dictation.state) && orb.set('orb'));
    later('adv', 1600, () => obStep === 4 && obShow(5));
  }
  obAxRender();
}
function obAxRender() {
  $('#obAxAsk').hidden = !!ob.ax;
  pop($('#obAxTry'), !!ob.ax);
  $('#obAxState').textContent = ob.asked ? 'Switch on Echotype in the list. This page carries on by itself.' : 'Without it, your words wait on the clipboard for you to paste.';
  obFoot();
}
$('#obReset').addEventListener('click', async () => {
  $('#obStale').hidden = true;
  ob.axSince = Date.now();
  ob.asked = true;
  try { await invoke('reset_accessibility'); $('#obAxState').textContent = 'Asked again. Switch on Echotype in the list.'; }
  catch (e) { $('#obAxState').textContent = String(e); }
});

// ----- 5: make it yours -----
// One question at a time (.ob-q). A tile saves as it's picked and stays on screen, so every
// choice can be tried; Continue moves on. The pill's questions are macOS-only (data-only).
let obQ = 0;
const obQs = () => $$('.ob-q').filter(q => (q.dataset.q !== 'wispr' || !!wispr) && (!q.dataset.only || (isMac && (q.dataset.only === 'mac' || S.show_in !== 'menubar'))) && (q.dataset.q !== 'speed' || obSpeedModels().length > 1)); // one model: nothing to choose
function obPrefsRender() {
  $('#obName').value = S.display_name || '';
  obHello();
  if (wispr) $('#obWisprList').replaceChildren(...wisprLines(wispr).map(([k, v]) => h('li', {}, h('b', {}, `${k}: `), v)));
  renderObSpeed();
  obCleanRender();
  // Back from the recap lands on the last question.
  obQShow(document.documentElement.classList.contains('ob-back') ? obQs().length - 1 : 0, true);
  // The name is typed straight onto the card, so the caret is there waiting (labelled by the heading).
  if (obQ === 0) setTimeout(() => obStep === 5 && obQ === 0 && $('#obName').focus({ preventScroll: true }));
}
// Under the name, the greeting Home will give, fading in once there's a name (and out, as it was).
function obHello() {
  if (firstName()) $('#obHello').textContent = hello();
  $('#obHello').classList.toggle('on', !!firstName());
}
$('#obName').addEventListener('input', e => { S.display_name = e.target.value; saveSoon(); obHello(); renderGreeting(); });

// The step's own enter (a staggered rise, ob-back reversing it), the count, the orb, the focus.
function obQShow(i, first = false) {
  const qs = obQs(), q = qs[i];
  clearTimeout(obT.q);
  if (!first) document.documentElement.classList.toggle('ob-back', i < obQ);
  obQ = i;
  for (const x of $$('.ob-q')) { x.hidden = x !== q; x.classList.remove('enter'); }
  // The orb, colour and ball questions centre a large orb: it glides there, and back to its column after.
  const step = q.closest('.ob-step'), wide = 'wide' in q.dataset;
  step.classList.toggle('ob-wide', wide);
  orb.slot(wide ? q.querySelector('.ob-centre') : step.querySelector(':scope > .ob-orb'));
  [...q.children].forEach((el, k) => el.style.setProperty('--i', k));
  void q.offsetWidth;
  q.classList.add('enter');
  $('#obCount').textContent = `${i + 1} of ${qs.length}`;
  obTilesRender();
  obOrbPreview();
  obFoot();
  if (!first) q.querySelector('.ob-tile[tabindex="0"], input').focus({ preventScroll: true });
}

const obVal = (t, key) => ({ true: true, false: false })[t.dataset.v] ?? (key === 'ball_size' ? Number(t.dataset.v) : t.dataset.v);
function obTilesRender() {
  for (const g of $$('.ob-q .ob-tiles')) {
    const key = g.dataset.key, q = g.closest('.ob-q').dataset.q, v = t => t.dataset.v;
    const on = t => key === 'ball_size' ? Math.abs(S.ball_size - obVal(t, key)) < 0.1
      : key ? S[key] === obVal(t, key)
      : q === 'wispr' ? v(t) === ob.wispr
      : q === 'clean' ? (v(t) === 'yes' ? obCleanOn() : !obCleanOn() && !S.cleanup.enabled)
      : v(t) === (S.sounds ? S.sound_pack : 'off'); // sound
    const ts = [...g.children];
    for (const t of ts) t.setAttribute('aria-checked', on(t));
    // Roving focus: Tab lands on the chosen tile (or the first) and the arrows move from there.
    const cur = ts.find(t => t.getAttribute('aria-checked') === 'true') ?? ts[0];
    for (const t of ts) t.tabIndex = t === cur ? 0 : -1;
  }
}
function obPick(g, t) {
  const key = g.dataset.key, q = g.closest('.ob-q').dataset.q, v = obVal(t, key);
  if (key) { S[key] = v; save(); syncBound(); changed(key); }
  else if (q === 'wispr') obWisprPick(v);
  else if (q === 'clean') obCleanPick(v === 'yes');
  else { // sound: tapping one plays it, again if it's already the one
    S.sounds = v !== 'off';
    if (S.sounds) { S.sound_pack = v; invoke('preview_sounds', { pack: v }).catch(() => {}); }
    save(); syncBound(); changed('sounds');
  }
  if (['orb_listen', 'orb_color', 'ball_size'].includes(key)) showPill(); // the real pill acts it out too
  obTilesRender();
  obOrbPreview();
}
for (const g of $$('.ob-q .ob-tiles')) {
  g.addEventListener('click', e => { const t = e.target.closest('.ob-tile'); if (t) obPick(g, t); });
  // Arrows move between tiles (up and down by a row); Enter or Space picks (a button's own click).
  g.addEventListener('keydown', e => {
    const cols = getComputedStyle(g).gridTemplateColumns.split(' ').length;
    const d = { ArrowRight: 1, ArrowLeft: -1, ArrowDown: cols, ArrowUp: -cols }[e.key];
    if (!d) return;
    e.preventDefault();
    const ts = [...g.children], n = ts[(ts.indexOf(document.activeElement) + d + ts.length) % ts.length];
    for (const t of ts) t.tabIndex = t === n ? 0 : -1;
    n.focus();
  });
  if (!('preview' in g.dataset)) continue;
  // The orb, colour and ball questions: the card's orb shows the tile under the pointer or the
  // keyboard, and goes back to the current choice when they leave without picking.
  const show = t => t?.classList.contains('ob-tile') && obOrbPreview(g.dataset.key, obVal(t, g.dataset.key));
  g.addEventListener('pointerover', e => show(e.target.closest('.ob-tile')));
  g.addEventListener('focusin', e => show(e.target));
  g.addEventListener('pointerleave', () => obOrbPreview());
  g.addEventListener('focusout', e => g.contains(e.relatedTarget) || obOrbPreview());
}

// The card's orb is the user's: their style in their colour on the pill's dark disc and, on the
// ball question, the resting ball at the pill's real size. `key`/`v`: a tile being looked at.
function obOrbPreview(key, v) {
  if (!orb || obStep !== 5 || $('#onboard').hidden) return;
  const pill = isMac && S.show_in !== 'menubar', o = { orb_listen: S.orb_listen, orb_color: S.orb_color, ball_size: S.ball_size };
  if (key) o[key] = v;
  const ball = pill && obQs()[obQ]?.dataset.q === 'ball';
  orb.style(pill ? { listen: o.orb_listen, think: S.orb_think, detail: S.orb_detail, color: o.orb_color, pill: ball ? { size: o.ball_size, zoom: S.pill_size } : null } : null);
  if (!BUSY.includes(dictation.state)) orb.set(ball ? 'ball' : 'orb');
}

// The built-in cleanup model, when it's installed and fits. Otherwise "Yes" means setting up a
// local server (or, by the link, a key) once onboarding finishes.
function obBuiltIn() {
  const c = sys.cleaners?.find(x => x.automatic);
  return isMac && !!sys.echotype_model_installed && !!c && c.ram_mb + 2048 <= sys.available_ram_gb * 1024;
}
const obCleanOn = () => (obBuiltIn() ? S.cleanup.enabled && S.cleanup.provider === 'echotype' : ob.cleanHow !== 'off');
function obCleanPick(yes) {
  if (!obBuiltIn()) { ob.cleanHow = !yes ? 'off' : ob.cleanHow === 'off' ? 'local' : ob.cleanHow; return obCleanRender(); }
  if (yes) { S.cleanup.enabled = true; setProvider('cleanup', 'echotype'); }
  else { S.cleanup.enabled = false; save(); }
  syncBound();
  setCleanupInert();
  renderRaw();
}
function obCleanRender() {
  const built = obBuiltIn(), key = ob.cleanHow === 'key';
  $('#obCleanYes').textContent = built ? 'With the cleanup model on this Mac'
    : key ? 'With your own API key, set up when you finish' : 'With a local server like Ollama, set up when you finish';
  $('#obCleanHow').hidden = built;
  $('#obCleanHow').textContent = key ? 'Use a local server instead' : 'Use my own API key instead';
}
$('#obCleanHow').addEventListener('click', () => { ob.cleanHow = ob.cleanHow === 'key' ? 'local' : 'key'; obCleanRender(); obTilesRender(); });

// ----- 6: you're set -----
function obDoneRender() {
  $('#obT5').textContent = firstName() ? `You're set, ${firstName()}.` : "You're set.";
  const toggle = S.mode === 'toggle', [pasteKey] = DEFAULT_LAST();
  const item = (keys, text) => h('li', {}, h('span', { className: 'keys' }, keys), h('span', {}, text));
  fill($('#obRecap'), // the hands-free line is `false` in toggle mode, which step 5 offers
    item(talkCaps(), toggle ? 'Press to talk, again to type' : 'Hold to talk, let go to type'),
    !toggle && item([...talkCaps(), h('span', { className: 'times' }, '×2')], 'Keep talking hands-free'),
    item([h('kbd', {}, 'esc')], 'Cancel while you talk'),
    (S.paste_last_hotkey || pasteKey) && item(keycaps(S.paste_last_hotkey || pasteKey), 'Paste the last one again'),
  );
  $('#obAutostartRow').hidden = !sys.can_autostart;
  refreshStatus();
}

async function obFinish() {
  const how = ob.cleanHow; // a local server or a key, picked in step 5 when there's no built-in model
  if (how !== 'off') { setupCleanup(how); return finishOnboarding('cleanup'); } // a key or a server to set up
  const relaunch = obRelaunch();
  // The orb folds back into the ball and drops out of the card, towards the real pill.
  $('#obNext').disabled = true;
  if (filming()) {} // the film takes it home (finishOnboarding)
  else if (obStill) { $('.ob-card').classList.add('bye'); await wait(150); }
  else {
    orb.set('ball');
    await wait(300);
    const c = $('.ob-card').getBoundingClientRect();
    orb.point(c.left + c.width / 2, innerHeight + 40);
    $('.ob-card').classList.add('bye');
    await wait(450);
  }
  await finishOnboarding('home', !relaunch);
  if (relaunch) invoke('relaunch').catch(() => {});
}

// `away`: the window steps aside so the pill at the bottom of the screen is the next thing seen.
async function finishOnboarding(page, away = false) {
  const filmed = filming();
  // New installs get the new shortcuts and (when allowed) launch at login; upgraders are only offered them.
  const [p, c] = DEFAULT_LAST();
  S.paste_last_hotkey ||= p;
  S.copy_last_hotkey ||= c;
  S.autostart = !!sys.can_autostart && $('#obAutostart').checked;
  if (!S.dismissed.includes('upgrade_offer')) S.dismissed.push('upgrade_offer');
  S.onboarded = true;
  syncBound();
  clearInterval(obPoll);
  Object.values(obT).forEach(clearTimeout);
  obListen(false);
  await save();
  const pill = isMac && S.show_in !== 'menubar' && S.pill_idle;
  if (filmed) await obFilmHome(); // the dots fall to the pill; it takes over as they land (a page that needs setting up opens after)
  if (away && pill && !filmed && window.__TAURI__.window) {
    await window.__TAURI__.window.getCurrentWindow().hide().catch(() => {});
    setTimeout(() => window.__TAURI__.event.emit?.('pill-hello'), 350); // overlay.js: the ball pulses once
  }
  orb?.stop();
  showOnboarding(false);
  $('.ob-card').classList.remove('bye', 'intro');
  renderHotkey();
  go(page);
  obDropTiny();
  if (filmed) await obFilmEnd(page !== 'home');
}

// ----- the speech model -----
// The model onboarding waits for: Automatic's pick for this Mac, or the manual pick (advanced).
const obWanted = () => (S.advanced ? models.find(m => m.id === S.stt.local_model) : models.find(m => m.recommended));

// The speed slider: the speech models from quickest to most accurate. Nothing is disabled; each
// stop says whether this Mac can run it.
const OB_SPEED = ['large-v3-turbo-q5_0'];
const obSpeedModels = () => OB_SPEED.map(id => models.find(m => m.id === id)).filter(Boolean);
const gb = mb => `${(mb / 1024).toFixed(1).replace(/\.0$/, '')} GB`;
function obSpeedFit(m) {
  const total = sys.total_ram_gb * 1024, free = sys.available_ram_gb * 1024;
  if (m.fits) return ['ok', `Runs comfortably on this Mac (${gb(free)} free).`];
  if (m.ram_mb + 2048 <= total) return ['warn', `Fits this Mac's ${Math.round(sys.total_ram_gb)} GB, but close some apps first: ${gb(free)} is free now.`];
  return ['bad', `This Mac has ${Math.round(sys.total_ram_gb)} GB of memory and this needs about ${gb(m.ram_mb)}: it may slow everything down or not load.`];
}
function renderObSpeed(at) {
  const list = obSpeedModels();
  if (!list.length) return;
  const slider = $('#obSpeed');
  slider.max = list.length - 1;
  if (at == null) at = Math.max(0, list.findIndex(m => m.id === obWanted()?.id));
  slider.value = at;
  const m = list[at];
  slider.setAttribute('aria-valuetext', m.label);
  $('#obSpeedName').textContent = m.label;
  $('#obSpeedRec').hidden = !m.recommended;
  $('#obSpeedNote').textContent = m.note;
  $('#obSpeedMem').textContent = `${fmtMB(m.size_mb)} download · about ${gb(m.ram_mb)} of memory while you dictate${m.downloaded ? ' · already on this Mac' : ''}`;
  let [tone, fit] = obSpeedFit(m);
  if (m.english_only && S.languages.some(l => l !== 'en')) [tone, fit] = ['warn', `${fit} English only: it can't type your other languages.`];
  $('#obSpeedFit').dataset.tone = tone;
  $('#obSpeedFit').textContent = fit;
}
// Picking a stop: Automatic's own pick keeps Automatic; any other is a manual pick (advanced),
// with Automatic's cleaner choices copied over so only the speech model changes. A download of a
// model no longer chosen stops (tiny.en, the practice model, keeps going).
function obSpeedPick(i) {
  const m = obSpeedModels()[i], auto = models.find(x => x.recommended);
  if (!m) return;
  if (m.id === auto?.id) S.advanced = false;
  else {
    if (!S.advanced) {
      S.cleaner_memory = sys.total_ram_gb * 1024 <= 8192 ? 'save' : 'ready';
      const c = sys.cleaners?.find(x => x.automatic);
      if (c) S.cleanup.model = S.cleanup.provider === 'echotype' ? c.id : S.cleanup.model;
    }
    S.advanced = true;
    S.stt.local_model = m.id;
  }
  save();
  for (const x of models) if (progress[x.id] != null && x.id !== m.id && x.id !== 'tiny.en') invoke('cancel_download', { id: x.id }).catch(() => {});
  obQueue = obQueue.filter(id => id === 'tiny.en' || id === m.id);
  obStartModel();
  changed('advanced');
  renderObSpeed(i);
  renderObModel();
}
$('#obSpeed').addEventListener('input', e => renderObSpeed(+e.target.value)); // preview while sliding
$('#obSpeed').addEventListener('change', e => obSpeedPick(+e.target.value));
const obReady = () => S.stt.provider !== 'local' || !!sttModel()?.downloaded;
const obDownloading = () => models.find(m => progress[m.id] != null);
let obQueue = [], obFetching = false;
// One download at a time, in order: tiny.en is useful only if it lands first.
async function obFetch(...ids) {
  obQueue.push(...ids.filter(id => !obQueue.includes(id)));
  if (obFetching) return;
  obFetching = true;
  while (obQueue.length) {
    const id = obQueue.shift(), m = models.find(x => x.id === id);
    if (m && !m.downloaded && progress[id] == null) await download(id);
  }
  obFetching = false;
}
function obStartModel() {
  if (S.stt.provider !== 'local') return;
  const want = obWanted();
  if (S.advanced) {
    const pick = want?.fits || want?.downloaded ? want : models.find(m => m.recommended);
    if (pick && S.stt.local_model !== pick.id) { S.stt.local_model = pick.id; save(); }
    if (pick && !pick.downloaded) obFetch(pick.id);
    return;
  }
  if (!want || want.downloaded) return;
  const tiny = models.find(m => m.id === 'tiny.en');
  if (tiny && !models.some(m => m.downloaded)) { local('ob-tiny', '1'); obFetch(tiny.id, want.id); }
  else obFetch(want.id);
}
function obModelLanded() {
  if (obStep === 5 && !$('#onboard').hidden && document.activeElement !== $('#obSpeed')) renderObSpeed(); // "already on this Mac", not mid-drag
  obDropTiny();
}
// tiny.en was only for the first practice run: once the real model is here and onboarding is
// done, it goes, unless the user picked it themselves.
function obDropTiny() {
  const want = models.find(m => m.recommended), tiny = models.find(m => m.id === 'tiny.en');
  if (!local('ob-tiny') || !S.onboarded || !want?.downloaded || !tiny?.downloaded) return;
  if (S.stt.local_model === 'tiny.en') return local('ob-tiny', null);
  if (BUSY.includes(dictation.state)) return later('tiny', 5000, obDropTiny);
  local('ob-tiny', null);
  invoke('delete_model', { id: 'tiny.en' }).catch(() => {}).then(refreshModels);
}

// Under the card: how the download is going, with the API option as a link.
function renderObModel() {
  if ($('#onboard').hidden) return;
  const el = $('#obModel'), want = obWanted(), dl = obDownloading();
  const pct = m => `${Math.round((progress[m.id] || 0) * 100)}%`;
  const api = h('button', { className: 'link', type: 'button', onclick: () => finishOnboarding('speech') }, 'Use an API instead');
  if (S.stt.provider !== 'local' || want?.downloaded) fill(el, null);
  else if (!want) fill(el, 'No speech model fits in the memory free right now. ', api);
  else if (modelErr[want.id]) fill(el, `Couldn't download ${want.label}: ${modelErr[want.id]} `, h('button', { className: 'link', type: 'button', onclick: () => obFetch(want.id) }, 'Try again'));
  else if (obReady() && dl) fill(el, `Ready to practise. Getting the full model · `, h('span', { className: 'pct' }, pct(dl)));
  else fill(el, 'Getting your speech model ready · ', h('span', { className: 'pct' }, dl ? pct(dl) : '0%'), ' ', api);
  if (obStep === 3) obTryRender();
}

// ---------- clipboard ----------

// ---------- events ----------
meetingsPage.init({ $, h, invoke, listen, copyText });
listen('dictation-state', ({ payload }) => {
  dictation = payload;
  renderHero();
  if ((payload.state === 'done' || payload.state === 'error') && payload.text) { // error + text = transcribed but not pasted
    recent.unshift({ ts: Date.now(), text: payload.text });
    renderRecent();
  }
  if (payload.state === 'done' || payload.state === 'error') { refreshCloud(); refreshLastApp(); } // words used went up; the app it went to
  if (rerunPending && (payload.state === 'done' || payload.state === 'error')) { rerunPending = false; renderHistory(); }
  if (payload.state !== 'recording') muted = false;
  obDictation(payload);
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
  if (orb && dictation.state === 'recording') orb.level(lvl);
});

listen('model-download', ({ payload: p }) => {
  if (p.error === 'Download cancelled') { delete progress[p.id]; renderModels(); renderObModel(); return; } // the slider moved on
  if (p.error) { delete progress[p.id]; modelErr[p.id] = p.error; renderModels(); renderObModel(); return; }
  if (p.done) {
    delete progress[p.id];
    refreshModels().then(() => { renderObModel(); obModelLanded(); });
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
listen('navigate', ({ payload }) => {
  if (!$('#onboard').hidden) return;
  // "No sound from your microphone" on the pill: Settings, on the microphone picker.
  if (payload !== 'mic') return go(payload);
  go('settings');
  $('#mic').scrollIntoView({ block: 'center' });
  $('#mic').focus();
});

listen('tauri://drag-enter', () => $('#drop').classList.add('over'));
listen('tauri://drag-leave', () => $('#drop').classList.remove('over'));
listen('tauri://drag-drop', ({ payload }) => {
  $('#drop').classList.remove('over');
  if (!payload?.paths?.length) return;
  if ($('#onboard').hidden) go('home');
  transcribe(payload.paths);
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
// No Echotype Cloud in this build: no sign-in, plan or provider anywhere (presets already leave it out).
if (!sys.cloud) $('#setupCloud').remove();
// macOS gets the translucent sidebar (windowEffects in tauri.conf.json); elsewhere it stays opaque (D19).
document.documentElement.dataset.os = isMac ? 'mac' : 'other';
if (!isMac) for (const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT); w.nextNode();) w.currentNode.nodeValue = pc(w.currentNode.nodeValue);
if (isMac && !preview) document.documentElement.dataset.vibrant = '';
S.dictionary ??= [];
S.replacements ??= [];
S.learned ??= [];
S.ignored_words ??= [];
S.transforms ??= [];
S.dismissed ??= [];
base = clone(S);
recent = await invoke('get_history', { limit: 3 }).catch(() => []);

// The title bar over the pane frosts once the page's content has scrolled up under it (.pane-drag).
$('#main').addEventListener('scroll', () => {
  const top = $('main > section:not([hidden])')?.firstElementChild?.getBoundingClientRect().top ?? Infinity;
  document.documentElement.classList.toggle('pane-scrolled', top < $('.pane-drag').offsetHeight);
}, { passive: true });

// Large windows: the whole window a step larger (the webview's page zoom, like the pill's), so
// it doesn't sit tiny on a big screen. From the window's own width, which zoom doesn't change.
const zoomFor = w => (w >= 2000 ? 1.3 : w >= 1600 ? 1.2 : w >= 1300 ? 1.1 : 1);
let zoomed = 1;
function fitZoom() {
  if (filming()) return; // the film's stage is the whole display, at 1x
  const z = zoomFor(window.outerWidth);
  if (z === zoomed) return;
  zoomed = z;
  window.__TAURI__.webview?.getCurrentWebview?.().setZoom(z).catch(() => {});
}
addEventListener('resize', fitZoom);
fitZoom();

// Home's hello: by time of day, with the name from Stats when there is one, and one line from
// a small set, picked when the window opens. Settings → Advanced can turn it off (`greetings`).
const PROMPTS = ["What's on the speak list today?", "Let's get going.", 'Ready when you are.', 'What are we writing today?', 'Talk it out, Echotype types it.', 'Say the first sentence.'];
const prompt = PROMPTS[Math.floor(Math.random() * PROMPTS.length)];
const firstName = () => (S.display_name || '').trim().split(/\s+/)[0];
// "Good morning, Aditya." (onboarding previews it as the name is typed).
function hello() {
  const h = new Date().getHours(), name = firstName();
  return `${h >= 5 && h < 12 ? 'Good morning' : h >= 12 && h < 17 ? 'Good afternoon' : h >= 17 && h < 22 ? 'Good evening' : 'Working late'}${name ? `, ${name}` : ''}.`;
}
function renderGreeting() {
  const el = $('#greeting'), on = S.greetings !== false;
  el.hidden = !on;
  if (on) el.textContent = `${hello()} ${prompt}`;
}

function renderAll() {
  renderGreeting();
  renderHotkey();
  renderPill();
  renderRaw();
  renderUpgrade();
  renderStartup();
  renderWatch();
  renderMicNote();
  renderTalk();
  renderStt();
  renderCleanup();
  renderCloud();
  renderLangNote();
  renderLanguages();
  renderDict();
  renderTransforms();
  renderAdvanced();
  renderNotice();
  $('#clipDaysRow').hidden = !S.history_enabled; // no history, no audio kept
}

bindAll();
syncBound();
renderAll();
renderRecent();
refreshCloud();
refreshNotices();
renderMics();
refreshPerms();
refreshStatus();
try { (await import('./stats.js')).init({ invoke, listen, h, go, settings: () => S, keys: () => `${verb()} ${hotkeyText(S.hotkey)}` }); }
catch (e) { console.error('Stats did not load', e); } // the rest of the window still works
const start = location.hash.slice(1);
go($(`main > section[data-page="${start}"]`) ? start : 'home');
if (!S.onboarded) startOnboarding(+new URLSearchParams(location.search).get('step') || S.onboard_step || 1);
else obDropTiny(); // a practice model left from a relaunch mid-download
