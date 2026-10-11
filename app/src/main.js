// Echotype settings window. Talks to the Rust core through Tauri commands and events.
if (!window.__TAURI__) await import('./mock.js'); // browser preview for design QA

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];

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
const progress = {}, modelErr = {};
let recent = [], dropResults = [], apiStatus = null, tokenShown = false;

// ---------- saving ----------
let saveTimer, savedTimer;
async function save(quiet) {
  clearTimeout(saveTimer);
  const settings = { ...S, replacements: S.replacements.filter(r => r.from.trim()) };
  try {
    await invoke('save_settings', { settings });
    flash();
    return null;
  } catch (e) {
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
  if (p === 'translate_to_english') renderLangNote();
  if (p === 'cleanup.enabled') setCleanupInert();
  if (p.startsWith('local_api')) refreshApi();
  if (p === 'history_enabled') renderHistory();
}

// ---------- navigation ----------
function go(page) {
  for (const s of $$('main > section')) s.hidden = s.dataset.page !== page;
  for (const b of $$('nav button')) b.toggleAttribute('aria-current', false), b.dataset.page === page && b.setAttribute('aria-current', 'page');
  $('#main').scrollTop = 0;
  if (page === 'history') renderHistory();
  if (page === 'settings') { refreshPerms(); refreshApi(); }
  history.replaceState(null, '', '#' + page);
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
  if (!mods.length && !/^F\d+$/.test(key)) return { mods, error: 'Add a modifier such as ⌥, ⌃ or ⌘.' };
  return { mods, accel: [...mods, key].join('+') };
}

function renderHotkey() {
  const btn = $('#hotkeyBtn');
  if (!btn.classList.contains('listening')) btn.replaceChildren(...keycaps(S.hotkey));
  $('#modeDesc').textContent = S.mode === 'toggle'
    ? 'Press once to start, again to stop.'
    : 'Hold the shortcut while you speak, let go to paste.';
  renderHero();
  $('#obTry').replaceChildren(S.mode === 'toggle' ? 'Press' : 'Hold', ...keycaps(S.hotkey), 'and speak');
}

let recording = null;
function stopRecording() {
  if (!recording) return;
  window.removeEventListener('keydown', recording, true);
  window.removeEventListener('keyup', recording, true);
  recording = null;
  $('#hotkeyBtn').classList.remove('listening');
  renderHotkey();
}
$('#hotkeyBtn').addEventListener('click', () => {
  if (recording) return stopRecording();
  const btn = $('#hotkeyBtn'), err = $('#hotkeyError');
  err.hidden = true;
  btn.classList.add('listening');
  btn.textContent = 'Press a shortcut…';
  recording = async e => {
    e.preventDefault(); e.stopPropagation();
    const r = accelFrom(e);
    if (e.type === 'keyup') { if (!r.mods.length) btn.textContent = 'Press a shortcut…'; return; }
    if (e.key === 'Escape' && !r.mods.length) return stopRecording();
    if (!r.accel) {
      if (r.error) { err.textContent = r.error; err.hidden = false; }
      btn.textContent = r.mods.length ? r.mods.map(keyName).join(isMac ? ' ' : '+') + ' …' : 'Press a shortcut…';
      return;
    }
    const prev = S.hotkey;
    S.hotkey = r.accel;
    stopRecording();
    const fail = await save(true);
    if (fail) {
      S.hotkey = prev;
      err.textContent = `${hotkeyText(r.accel)} didn't work: ${fail}`;
      err.hidden = false;
    }
    renderHotkey();
  };
  window.addEventListener('keydown', recording, true);
  window.addEventListener('keyup', recording, true);
});
$('#hotkeyBtn').addEventListener('blur', stopRecording);

// ---------- home ----------
let dictation = { state: 'idle' };
const STATE_LABEL = { idle: 'Ready', recording: 'Listening', transcribing: 'Transcribing', cleaning: 'Cleaning up', done: 'Pasted', error: "Couldn't dictate" };
const METER = [0.5, 0.75, 0.95, 1, 0.8, 0.95, 0.7, 0.5];

// What stops dictation from working right now, with the one action that fixes it.
let perms = null, sttKeySaved = true;
function blocker() {
  if (perms && !perms.accessibility) return { text: "Echotype can't paste yet: Accessibility access is off.", label: 'Allow Accessibility', act: () => invoke('open_permission_settings', { kind: 'accessibility' }) };
  if (perms?.microphone === 'denied') return { text: 'Microphone access is blocked for Echotype.', label: 'Allow microphone', act: () => invoke('open_permission_settings', { kind: 'microphone' }) };
  const p = preset(S.stt.provider);
  if (S.stt.provider === 'local') {
    if (!models.find(m => m.id === S.stt.local_model)?.downloaded) return { text: 'No speech model is downloaded yet.', label: 'Choose a model', act: () => go('speech') };
  } else if (p?.needs_key && !sttKeySaved) return { text: `Add your ${p.label} API key to transcribe.`, label: 'Add key', act: () => go('speech') };
  return null;
}

function renderHero() {
  if (!S) return; // a focus event can land before boot finishes
  const st = dictation.state;
  const b = st === 'idle' || st === 'done' ? blocker() : null;
  document.body.dataset.state = st;
  document.body.toggleAttribute('data-blocked', !!b);
  $('#heroState span').textContent = b ? 'Not ready' : STATE_LABEL[st] ?? 'Ready';
  const line = $('#heroLine'), sub = $('#heroSub');
  const start = h('button', { className: 'btn quiet', type: 'button', onclick: () => invoke('toggle_dictation') }, 'Start with a click');
  if (st === 'recording') {
    line.replaceChildren('Listening', h('span', { className: 'meter', id: 'heroMeter', 'aria-hidden': 'true' }, METER.map(() => h('i'))));
    sub.replaceChildren(S.mode === 'toggle' ? 'Press the shortcut again to stop. Esc cancels.' : 'Let go to paste. Esc cancels.');
  } else if (st === 'transcribing' || st === 'cleaning') {
    line.replaceChildren(st === 'cleaning' ? 'Cleaning up…' : 'Transcribing…');
    sub.replaceChildren('The text goes wherever your cursor is.');
  } else {
    line.replaceChildren(S.mode === 'toggle' ? 'Press' : 'Hold', ...keycaps(S.hotkey), 'to dictate');
    if (b) sub.replaceChildren(h('span', {}, b.text), h('button', { className: 'btn', type: 'button', onclick: b.act }, b.label));
    else if (st === 'error') sub.replaceChildren(h('span', {}, `${dictation.error || 'Dictation stopped.'} Try again, or check Settings.`), start);
    else if (st === 'done' && dictation.warning) sub.replaceChildren(h('span', {}, dictation.warning), start);
    else sub.replaceChildren(h('span', {}, 'Text appears wherever your cursor is.'), start);
  }
}

function renderRecent() {
  const box = $('#recent');
  $('#recentTitle').hidden = box.hidden = !recent.length;
  box.replaceChildren(...recent.slice(0, 3).map(r => {
    const copy = h('button', { className: 'btn', type: 'button' }, 'Copy');
    copy.onclick = () => copyText(r.text, copy);
    return h('div', { className: 'row entry' }, h('span', { className: 'when' }, fmtTime(r.ts, true)), h('span', { className: 'text' }, r.text), copy);
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
  const def = p?.[kind === 'stt' ? 'default_stt_model' : 'default_cleanup_model'];
  if (def) cfg.model = def;
  save();
  if (kind === 'stt') renderStt(); else fillRemote('cleanup');
  renderLangNote();
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
  row.hidden = p?.needs_key === false && p.id !== 'custom';
  if (row.hidden) return;
  const saved = await invoke('has_secret', { name }).catch(() => false);
  if (secretName(kind) !== name) return; // provider changed while we waited
  if (kind === 'stt') { sttKeySaved = saved; renderHero(); }
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
  const local = S.stt.provider === 'local';
  $('#sttLocal').hidden = !local;
  $('#sttRemote').hidden = local;
  if (local) renderModels(); else fillRemote('stt');
}

const fmtMB = mb => mb >= 1000 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;

function modelRow(m, { compact, pick } = {}) {
  const inUse = S.stt.provider === 'local' && S.stt.local_model === m.id;
  const p = progress[m.id];
  let actions;
  if (p != null) {
    actions = [h('span', { className: 'bar', role: 'progressbar', 'aria-label': `Downloading ${m.label}`, 'aria-valuenow': Math.round(p * 100), 'data-progress': m.id }, h('i', { style: `transform:scaleX(${p})` })),
      h('span', { className: 'pct', 'data-pct': m.id }, `${Math.round(p * 100)}%`)];
  } else if (!m.downloaded) {
    actions = [h('button', { className: m.recommended ? 'btn primary' : 'btn', type: 'button', onclick: () => download(m.id) }, 'Download')];
  } else if (inUse && !pick) {
    actions = [h('span', { className: 'in-use' }, svg(CHECK), 'In use')];
  } else {
    const del = !pick && h('button', { className: 'btn quiet danger', type: 'button' }, 'Delete');
    if (del) del.onclick = async () => {
      if (!del.dataset.armed) { del.dataset.armed = 1; del.textContent = 'Confirm delete'; return setTimeout(() => { del.textContent = 'Delete'; delete del.dataset.armed; }, 3000); }
      try { await invoke('delete_model', { id: m.id }); } catch (e) { modelErr[m.id] = String(e); }
      await refreshModels();
    };
    actions = [del, pick ? (inUse ? h('span', { className: 'in-use' }, svg(CHECK), 'Selected') : h('button', { className: 'btn', type: 'button', onclick: () => useModel(m.id) }, 'Use'))
      : h('button', { className: 'btn', type: 'button', onclick: () => useModel(m.id) }, 'Use')];
  }
  return h('div', { className: 'row model' },
    h('span', { className: 'row-text' },
      h('span', { className: 'model-name' }, m.label,
        m.recommended && h('span', { className: 'tag rec' }, 'Recommended'),
        m.english_only && h('span', { className: 'tag' }, 'English only'),
        m.translates && h('span', { className: 'tag' }, 'Translates')),
      h('span', { className: 'model-meta' },
        h('span', {}, fmtMB(m.size_mb)),
        h('span', {}, `${fmtMB(m.ram_mb)} memory`),
        !m.fits && h('span', { className: 'badge warn' }, 'Needs more memory')),
      !compact && m.note && h('span', { className: 'row-desc' }, m.note),
      modelErr[m.id] && h('span', { className: 'row-desc error', role: 'alert' }, modelErr[m.id])),
    h('span', { className: 'actions' }, actions));
}

function renderModels() {
  $('#memline').replaceChildren('Free memory now ', h('b', {}, `${sys.available_ram_gb.toFixed(1)} GB`), ` of ${Math.round(sys.total_ram_gb)} GB. A model fits when it leaves 2 GB for everything else.`);
  $('#modelList').replaceChildren(...models.map(m => modelRow(m)));
  renderObModels();
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
  renderObModels();
  renderLangNote();
  renderHero();
}

// ---------- cleanup ----------
function setCleanupInert() {
  const off = !S.cleanup.enabled;
  $('#cleanupBody').classList.toggle('disabled', off);
  $('#cleanupBody').inert = off; // off means not focusable either, not just dimmed
}
function renderCleanup() {
  $('#cleanupProvider').replaceChildren(...presets.filter(p => p.cleanup).map(p => h('option', { value: p.id, selected: p.id === S.cleanup.provider }, p.label)));
  setCleanupInert();
  fillRemote('cleanup');
}
$('#cleanupProvider').addEventListener('change', e => setProvider('cleanup', e.target.value));

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
  note.replaceChildren(alt
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
function fmtTime(ts, short) {
  const d = new Date(typeof ts === 'number' && ts < 1e12 ? ts * 1000 : ts);
  if (isNaN(d)) return '';
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.toDateString() === new Date().toDateString()) return time;
  const day = d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  return short ? day : `${day}\n${time}`;
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
      ? h('p', { className: 'empty' }, 'No dictations yet. ', S.mode === 'toggle' ? 'Press ' : 'Hold ', hotkeyText(S.hotkey), ' in any text field and the result lands here.')
      : h('p', { className: 'empty' }, 'History is off, so nothing is kept.', h('button', { className: 'btn', type: 'button', onclick: () => go('settings') }, 'Turn on in Settings')));
    return;
  }
  list.replaceChildren(...items.map(it => {
    const copy = h('button', { className: 'btn', type: 'button' }, 'Copy');
    copy.onclick = e => { e.preventDefault(); copyText(it.text, copy); };
    return h('details', {},
      h('summary', {}, h('span', { className: 'when' }, fmtTime(it.ts)), h('span', { className: 'text' }, it.text), copy),
      h('div', { className: 'detail' },
        it.raw !== it.text && h('p', { className: 'raw text' }, h('b', {}, 'Heard: '), it.raw),
        h('span', { className: 'timing' },
          h('span', {}, `Speech ${it.stt_ms} ms${it.stt_provider ? ` (${it.stt_provider})` : ''}`),
          it.cleanup_ms != null && h('span', {}, `Cleanup ${it.cleanup_ms} ms${it.cleanup_provider ? ` (${it.cleanup_provider})` : ''}`))));
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
  let devices = [];
  try { devices = await invoke('list_input_devices'); $('#micError').hidden = true; }
  catch (e) { $('#micError').textContent = `Couldn't list microphones (${e}). Echotype uses the system default.`; $('#micError').hidden = false; }
  $('#mic').replaceChildren(h('option', { value: '' }, 'System default'), ...devices.map(d => h('option', { value: d }, d)));
  $('#mic').value = S.input_device ?? '';
}

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
  $('#perms').replaceChildren(...rows());
  $('#obPerms').replaceChildren(...rows());
  $('.ob-step[data-step="1"] [data-next]').textContent = p.accessibility ? 'Continue' : 'Continue without pasting';
  renderHero();
}
window.addEventListener('focus', refreshPerms); // coming back from System Settings

// ---------- onboarding ----------
let obStep = 1;
function obShow(n) {
  obStep = n;
  $$('.ob-step').forEach(s => (s.hidden = +s.dataset.step !== n));
  $$('#obSteps li').forEach((li, i) => { li.className = i + 1 === n ? 'on' : i + 1 < n ? 'done' : ''; li.toggleAttribute('aria-current', i + 1 === n); });
  if (n === 2) renderObModels();
  $(`.ob-step[data-step="${n}"] h2`).focus();
}
function showOnboarding(on) {
  $('#onboard').hidden = !on;
  $('aside').inert = $('main').inert = on; // keep Tab inside the dialog
}
function renderObModels() {
  if ($('#onboard').hidden) return;
  const rec = models.find(m => m.recommended);
  $('#obModelLede').textContent = rec
    ? `${rec.label} is the best fit for the ${sys.available_ram_gb.toFixed(1)} GB of memory free right now. Everything runs on this Mac.`
    : 'None of the local models fit in the memory free right now. You can use an API instead.';
  const list = models.filter(m => m.fits || m.downloaded).sort((a, b) => !!b.recommended - !!a.recommended);
  $('#obModels').replaceChildren(...list.map(m => modelRow(m, { compact: !m.recommended, pick: true })));
  const chosen = models.find(m => m.id === S.stt.local_model);
  $('#obModelNext').disabled = !(S.stt.provider === 'local' && chosen?.downloaded);
}
$$('[data-next]').forEach(b => b.addEventListener('click', () => {
  if (obStep === 2 && !models.find(m => m.id === S.stt.local_model)?.downloaded) return;
  obShow(obStep + 1);
}));
$$('[data-recheck]').forEach(b => b.addEventListener('click', refreshPerms));
async function finishOnboarding(page) {
  S.onboarded = true;
  await save();
  showOnboarding(false);
  go(page);
}
$('#obSkip').addEventListener('click', () => finishOnboarding('home'));
$('#onboard').addEventListener('keydown', e => { if (e.key === 'Escape') finishOnboarding('home'); });
$('#obApi').addEventListener('click', () => finishOnboarding('speech'));
$('#obDone').addEventListener('click', () => finishOnboarding('home'));
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
  if (payload.state === 'done' && payload.text) {
    recent.unshift({ ts: Date.now(), text: payload.text });
    renderRecent();
  }
  const st = $('#obTryState'), res = $('#obResult');
  st.textContent = payload.state === 'error' ? payload.error || 'Something went wrong. Try again.'
    : payload.state === 'idle' || payload.state === 'done' ? 'Click into any text field first, then say a sentence.'
    : `${STATE_LABEL[payload.state]}…`;
  if (payload.state === 'done' && payload.text) { res.textContent = payload.text; res.hidden = false; }
  $('#obToggle').textContent = payload.state === 'recording' ? 'Stop' : 'Start with a click';
});

listen('audio-level', ({ payload }) => {
  const bars = $$('#heroMeter i');
  const lvl = Math.sqrt(Math.max(0, Math.min(1, payload.level)));
  bars.forEach((b, i) => { b.style.transform = `scaleY(${Math.max(0.15, Math.min(1, lvl * METER[i] * (0.75 + Math.random() * 0.5)))})`; });
});

listen('model-download', ({ payload: p }) => {
  if (p.error) { delete progress[p.id]; modelErr[p.id] = p.error; renderModels(); return; }
  if (p.done) {
    delete progress[p.id];
    refreshModels().then(() => { if (!$('#onboard').hidden && obStep === 2) useModel(p.id); });
    return;
  }
  const frac = p.total ? p.downloaded / p.total : 0;
  const had = progress[p.id] != null;
  progress[p.id] = frac;
  const bars = $$(`[data-progress="${p.id}"]`);
  if (!had || !bars.length) return renderModels();
  bars.forEach(b => { b.firstChild.style.transform = `scaleX(${frac})`; b.setAttribute('aria-valuenow', Math.round(frac * 100)); });
  $$(`[data-pct="${p.id}"]`).forEach(el => (el.textContent = `${Math.round(frac * 100)}%`));
});

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
presets = presets.filter(p => p.id !== 'cloud'); // v1: no cloud
isMac = /mac|darwin/i.test(sys.os);
S.dictionary ??= [];
S.replacements ??= [];
recent = await invoke('get_history', { limit: 3 }).catch(() => []);

bindAll();
syncBound();
renderHotkey();
renderRecent();
renderStt();
renderCleanup();
renderLangNote();
renderDict();
renderMics();
refreshPerms();
const start = location.hash.slice(1);
go($(`main > section[data-page="${start}"]`) ? start : 'home');
if (!S.onboarded) { showOnboarding(true); obShow(1); }
