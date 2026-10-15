// Browser preview only: fakes the Rust core (same commands and events) so the UI can be
// opened in a normal browser for design QA. Never loaded inside the Tauri app.
const q = new URLSearchParams(location.search);
const listeners = {};
const emit = (event, payload) => (listeners[event] || []).forEach(cb => cb({ event, payload }));
const wait = ms => new Promise(r => setTimeout(r, ms));
const now = Date.now();

const settings = {
  hotkey: 'Alt+Space', mode: 'hold', input_device: null, language: 'auto', translate_to_english: false,
  local_api: { enabled: false, port: 7811 },
  stt: { provider: 'local', local_model: q.has('onboarding') ? '' : 'large-v3-turbo-q5_0', model: '', base_url: '' },
  cleanup: { enabled: !q.has('nocleanup'), provider: 'ollama', model: 'gemma3:4b', base_url: '', style: 'default', custom_instructions: '' },
  dictionary: ['Echotype', 'Tauri', 'Hinglish', 'Supabase'],
  replacements: [{ from: 'my email', to: 'aditya@example.com' }],
  restore_clipboard: true, history_enabled: true,
  usage_stats: !q.has('statsoff'), display_name: q.get('name') || '',
  onboarded: !q.has('onboarding'), onboard_step: 0,
  talk_key: q.get('talk') || 'accelerator',
  paste_last_hotkey: q.has('upgrade') ? '' : 'CommandOrControl+Control+V',
  copy_last_hotkey: q.has('upgrade') ? '' : 'CommandOrControl+Control+C',
  autostart: !q.has('upgrade'), sounds: true, sound_pack: 'classic', pill_fade: true, orb_listen: 'listening', orb_think: 'working', orb_detail: 1, orb_color: '#eef1ff', pill_size: 1.5, ball_size: 1,
  advanced: q.has('advanced'), cleaner_memory: 'ready', cleaner_idle_unload: false, cleaner_idle_minutes: 10, cleaner_context: 1024, greetings: true, detect_meetings: true, meeting_end: 'ask', cloud_moved: q.has('moved'),
  look: q.get('look') === 'v2' ? 'v2' : 'classic',
  pill_idle: !q.has('pilloff'), pill_hidden_until: q.has('pillhidden') ? now + 42 * 60000 : 0, pill_in_recordings: false,
  pill_positions: {}, pill_screen: null, pill_follow_cursor: true, show_in: q.has('menubar') ? 'menubar' : 'pill',
  dismissed: q.has('upgrade') || q.has('onboarding') ? [] : ['upgrade_offer'],
};
if (q.has('bt')) settings.input_device = 'AirPods Pro';
const presets = [
  { id: 'cloud', label: 'Echotype Cloud', stt: true, cleanup: true, base_url: 'https://cloud.echotype.app/v1', default_stt_model: 'whisper-large-v3-turbo', default_cleanup_model: 'openai/gpt-oss-20b', needs_key: true },
  { id: 'openai', label: 'OpenAI', stt: true, cleanup: true, base_url: 'https://api.openai.com/v1', default_stt_model: 'gpt-4o-mini-transcribe', default_cleanup_model: 'gpt-4.1-mini', needs_key: true },
  { id: 'groq', label: 'Groq', stt: true, cleanup: true, base_url: 'https://api.groq.com/openai/v1', default_stt_model: 'whisper-large-v3-turbo', default_cleanup_model: 'llama-3.1-8b-instant', needs_key: true },
  { id: 'openrouter', label: 'OpenRouter', stt: false, cleanup: true, base_url: 'https://openrouter.ai/api/v1', default_stt_model: '', default_cleanup_model: 'meta-llama/llama-3.1-8b-instruct', needs_key: true },
  { id: 'ollama', label: 'Ollama', stt: false, cleanup: true, base_url: 'http://localhost:11434/v1', default_stt_model: '', default_cleanup_model: 'gemma3:4b', needs_key: false },
  { id: 'lmstudio', label: 'LM Studio', stt: false, cleanup: true, base_url: 'http://localhost:1234/v1', default_stt_model: '', default_cleanup_model: 'qwen3-4b', needs_key: false },
  { id: 'echotype', label: 'Echotype model (local)', stt: false, cleanup: true, base_url: 'http://127.0.0.1:8899/v1', default_stt_model: '', default_cleanup_model: 'default_model', needs_key: false },
  { id: 'custom', label: 'Custom', stt: true, cleanup: true, base_url: '', default_stt_model: '', default_cleanup_model: '', needs_key: false },
];
// The same catalog, sizes and notes as stt.rs MODELS.
const models = [
  { id: 'large-v3-turbo-q5_0', label: 'Large v3 Turbo (Q5)', size_mb: 547, ram_mb: 761, downloaded: !q.has('onboarding') || q.get('model') === 'ready', note: 'Best default: near-large accuracy, fast, 99 languages. Cannot translate.' },
  { id: 'large-v3-turbo', label: 'Large v3 Turbo', size_mb: 1624, ram_mb: 1840, downloaded: false, note: 'Full-precision turbo. Slightly more accurate, twice the size. Cannot translate.' },
  { id: 'medium', label: 'Medium', size_mb: 1533, ram_mb: 1869, downloaded: false, note: 'Multilingual and translates to English (Hinglish → English).' },
  { id: 'small', label: 'Small', size_mb: 488, ram_mb: 717, downloaded: false, note: 'Multilingual, translates; lighter and less accurate than medium.' },
  { id: 'base.en', label: 'Base (English)', size_mb: 148, ram_mb: 316, downloaded: !q.has('onboarding'), note: 'English only. For low-memory machines.' },
  { id: 'tiny.en', label: 'Tiny (English)', size_mb: 78, ram_mb: 224, downloaded: q.get('model') === 'tiny', note: 'English only. Fastest, least accurate.' },
];
// ?ram=8 previews an 8 GB Mac (Automatic's light setup and its warnings); ?cloud shows Echotype Cloud.
const low = q.get('ram') === '8';
const sys = { total_ram_gb: low ? 8 : 24, available_ram_gb: low ? 3.2 : 5.1, os: 'macos', arch: 'aarch64', can_autostart: q.get('appdir') !== '0', low_ram: low, cloud: q.has('cloud'),
  echotype_model_installed: true, echotype_model_mb: 940,
  cleaners: [{ id: 'echotype-clean', label: 'Echotype 1.5B', size_mb: 940, ram_mb: 1113, installed: true, automatic: true, warning: null }] };
for (const m of models) Object.assign(m, { english_only: m.id.endsWith('.en'), translates: ['medium', 'small'].includes(m.id), fits: m.ram_mb + 2048 <= sys.available_ram_gb * 1024 });
const rec = models.find(m => m.id === (low ? 'small' : 'large-v3-turbo-q5_0'));
rec.recommended = true;
// Automatic: the recommendation once downloaded, else the downloaded model closest in RAM (stt.rs auto_model).
const automatic = () => {
  const have = models.filter(m => m.downloaded).sort((a, b) => (a.ram_mb > rec.ram_mb) - (b.ram_mb > rec.ram_mb) || Math.abs(a.ram_mb - rec.ram_mb) - Math.abs(b.ram_mb - rec.ram_mb));
  for (const m of models) m.automatic = m === (rec.downloaded ? rec : have[0] ?? rec);
};
for (const m of models) m.warning = low && m.ram_mb > rec.ram_mb ? `Uses about ${(m.ram_mb / 1024).toFixed(1)} GB while dictating. On an 8 GB Mac this can slow other apps.` : null;

const ago = min => Math.floor((now - min * 60000) / 1000);
let history = [
  { ts: ago(4), raw: 'haan toh um kal ki meeting mein we decided ki launch ko push karenge thursday tak', text: "In yesterday's meeting we decided to push the launch to Thursday.", stt_ms: 690, cleanup_ms: 540, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(38), raw: 'can you send me the uh the figma link no wait the loom link from the demo', text: 'Can you send me the Loom link from the demo?', stt_ms: 410, cleanup_ms: 610, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(95), raw: 'groceries eggs milk sourdough and two avocados', text: 'Groceries:\n- Eggs\n- Milk\n- Sourdough\n- Two avocados', stt_ms: 380, cleanup_ms: 720, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(60 * 26), raw: 'ship it after the tauri build passes on ci', text: 'ship it after the tauri build passes on ci', stt_ms: 350, cleanup_ms: 0, stt_provider: 'local', cleanup_provider: null, no_cleanup: true },
];
const secrets = new Set(['llm:groq']);

// Echotype Cloud: ?cloud=free or ?cloud=pro starts signed in; any 6 digits except 000000 sign in.
// Sample numbers only (the preview says so in the sidebar).
const nextMonday = () => { const d = new Date(); d.setUTCDate(d.getUTCDate() + ((8 - d.getUTCDay()) % 7 || 7)); d.setUTCHours(0, 0, 0, 0); return d.toISOString(); };
const sampleAccount = plan => plan === 'pro'
  ? { email: 'you@example.com', plan, words_used: 18240, words_limit: 0, resets_at: nextMonday(), audio_seconds_used: 11520, audio_seconds_limit: 144000 }
  : { email: 'you@example.com', plan: 'free', words_used: 1240, words_limit: 2000, resets_at: nextMonday(), audio_seconds_used: 610, audio_seconds_limit: 7200 };
let cloudAccount = ['free', 'pro'].includes(q.get('cloud')) ? sampleAccount(q.get('cloud')) : null;
if (q.has('cloud')) settings.stt.provider = 'cloud';
if (q.has('builtin')) Object.assign(settings.cleanup, { enabled: true, provider: 'echotype', model: 'echotype-clean' });
let busy = false;
// Onboarding: ?mic=unknown|denied|granted|restricted (onboarding starts at unknown), ?ax=1 (starts on).
const perm = { mic: q.get('mic') || (q.has('onboarding') ? 'unknown' : 'granted'), ax: q.has('onboarding') ? q.get('ax') === '1' : !q.has('noaccess') };
let micTimer;

const commands = {
  get_settings: () => structuredClone(settings),
  save_settings: ({ settings: s }) => {
    if (s.hotkey === 'CommandOrControl+Space') throw 'macOS uses this shortcut for Spotlight.';
    Object.assign(settings, structuredClone(s));
    setTimeout(() => emit('settings-changed', structuredClone(settings)), 0);
    return null;
  },
  get_presets: () => presets.filter(p => sys.cloud || p.id !== 'cloud'),
  set_secret: ({ name }) => { secrets.add(name); return null; },
  has_secret: ({ name }) => secrets.has(name),
  delete_secret: ({ name }) => { secrets.delete(name); return null; },
  list_models: () => { automatic(); return structuredClone(models); },
  // ?dl=slow takes five times as long; ?dl=stuck never finishes (onboarding's "getting ready").
  download_model: async ({ id }) => {
    const m = models.find(x => x.id === id), total = m.size_mb * 1048576;
    const steps = id === 'tiny.en' ? 16 : 40, ms = (id === 'tiny.en' ? 60 : 110) * (q.get('dl') === 'slow' ? 5 : 1);
    for (let i = 1; i <= steps; i++) {
      await wait(ms);
      if (q.get('dl') === 'stuck' && i > steps / 3) return new Promise(() => {});
      emit('model-download', { id, downloaded: (total * i) / steps, total, done: false });
    }
    m.downloaded = true;
    emit('model-download', { id, downloaded: total, total, done: true });
    return null;
  },
  delete_model: ({ id }) => { models.find(x => x.id === id).downloaded = false; return null; },
  list_input_devices: () => [
    { name: 'MacBook Pro Microphone', bluetooth: false, default: true },
    { name: 'AirPods Pro', bluetooth: true, default: false },
    { name: 'Shure MV7', bluetooth: false, default: false },
  ],
  // ?talk=fn&globe=0|1&im=0|1&relaunch=1 shows each talk-key state.
  shortcut_status: () => {
    const talk = settings.talk_key !== 'accelerator', globe = q.get('globe') !== '0', im = q.get('im') !== '0', relaunch = q.has('relaunch');
    const active = talk && (settings.talk_key !== 'fn' || globe) && im && !relaunch;
    return { talk_key_active: active, talk_key_error: talk && !active ? 'not active' : null, input_monitoring: im, globe_does_nothing: globe,
      needs_relaunch: talk && im && relaunch, unregistered_shortcuts: q.has('shortcutfail') && settings.hotkey === 'Alt+Space' ? ['hotkey'] : [], key_label: '⌥ Space' };
  },
  relaunch: () => null,
  get_history: ({ limit }) => (settings.history_enabled ? history.slice(0, limit) : []),
  clear_history: () => { history = []; return null; },
  transcribe_file: async ({ path }) => {
    await wait(1400);
    if (/\.(txt|pdf|png)$/i.test(path)) throw `Not an audio file: ${path.split('/').pop()}`;
    return {
      raw: 'bhai woh client wala deck um kal subah tak bhej dena aur haan pricing slide pe naya number daal dena fifteen ki jagah twelve',
      text: 'Please send the client deck by tomorrow morning, and update the pricing slide to 12 instead of 15.',
      stt_ms: 1180, cleanup_ms: 640,
    };
  },
  get_system_info: () => sys,
  local_api_status: () => ({ enabled: settings.local_api.enabled, port: settings.local_api.port, token: 'et_7f3c9a2e5b1d4f86a0c2e91b' }),
  toggle_dictation: async () => {
    if (busy) return null;
    busy = true;
    emit('dictation-state', { state: 'recording' });
    for (let i = 0; i < 60; i++) { await wait(50); emit('audio-level', { meter: Math.abs(Math.sin(i / 4)) * 0.6 + Math.random() * 0.2, muted: false }); }
    emit('dictation-state', { state: 'transcribing' }); await wait(700);
    emit('dictation-state', { state: 'cleaning' }); await wait(600);
    emit('dictation-state', { state: 'done', text: 'Let’s move the standup to 10:30 tomorrow.' });
    await wait(400);
    emit('dictation-state', { state: 'idle' });
    busy = false;
    return null;
  },
  pill_click: () => (busy ? null : commands.toggle_dictation()),
  pill_cancel: () => null,
  pill_open: () => null,
  pill_menu: () => null,
  pill_drag: () => null,
  pill_reset: () => null,
  pill_copy: () => null,
  pill_dismiss: () => null,
  check_permissions: () => ({ microphone: perm.mic === 'restricted' ? 'denied' : perm.mic, microphone_restricted: perm.mic === 'restricted', accessibility: perm.ax }),
  // A grant made "in System Settings" a moment later (?mic=denied&later, and Accessibility unless ?stale).
  open_permission_settings: ({ kind }) => {
    if (kind === 'microphone' && perm.mic === 'denied' && q.has('later')) setTimeout(() => (perm.mic = 'granted'), 2500);
    if (kind === 'accessibility' && !q.has('stale')) setTimeout(() => (perm.ax = true), 2500);
    return null;
  },
  // The system prompt, answered after a moment: ?answer=deny says no.
  request_microphone: () => {
    if (perm.mic === 'unknown') setTimeout(() => (perm.mic = q.get('answer') === 'deny' ? 'denied' : 'granted'), 900);
    return perm.mic;
  },
  reset_accessibility: () => { setTimeout(() => (perm.ax = true), 2000); return null; },
  // The sound check: a quiet room, then a voice (?silent keeps it quiet).
  mic_check: ({ on }) => {
    clearInterval(micTimer);
    if (!on) return null;
    let i = 0;
    micTimer = setInterval(() => {
      const voice = !q.has('silent') && i++ > 24;
      emit('mic-level', { meter: voice ? 0.45 + 0.35 * Math.abs(Math.sin(i / 3)) : 0.04 * Math.random(), muted: false });
    }, 50);
    return null;
  },
  focus_main: () => 'TextEdit',
  play_sound: () => null,
  open_textedit: () => null,
  quit_app: () => null,
  cloud_start_login: async ({ email }) => {
    await wait(500);
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw 'Enter a valid email address.';
    return null;
  },
  cloud_verify: async ({ email, code }) => {
    await wait(500);
    if (!/^\d{6}$/.test(code) || code === '000000') throw 'That code is wrong or has expired.';
    cloudAccount = { ...sampleAccount('free'), email };
    return { email, plan: 'free' };
  },
  cloud_account: () => { if (!cloudAccount) throw 'signed out'; return structuredClone(cloudAccount); },
  cloud_checkout: () => { if (!cloudAccount) throw 'signed out'; cloudAccount = { ...sampleAccount('pro'), email: cloudAccount.email }; return null; },
  cloud_portal: () => { if (!cloudAccount) throw 'signed out'; return null; },
  cloud_logout: () => { cloudAccount = null; return null; },
  retry_last: async () => {
    emit('dictation-state', { state: 'transcribing' }); await wait(700);
    emit('dictation-state', { state: 'done', text: 'Moving the design review to Friday at 3.', copied: true });
    return null;
  },
};

Object.assign(commands, (await import('./mock-stats.js')).statsCommands(q));
// Meetings: ?meetings=empty|live|done (default done). Sample data only.
const mq = q.get('meetings') || 'done';
const sampleSegments = [
  { start: 2, end: 7, speaker: 'you', text: 'Morning, everyone. Let’s get started with the launch review for the mobile app.' },
  { start: 8, end: 14, speaker: 's1', text: 'Thanks. The beta went out to our testers last week, and the crash rate dropped a lot.' },
  { start: 15, end: 19, speaker: 'you', text: 'Great news. What did testers say about onboarding?' },
  { start: 20, end: 29, speaker: 's1', text: 'Most liked it, but several found the permissions screen confusing. We should simplify it before launch.' },
  { start: 30, end: 36, speaker: 's2', text: 'I can take that. I’ll redesign the permissions screen and share a draft by Thursday.' },
  { start: 37, end: 43, speaker: 'you', text: 'Perfect. So we ship next month, as long as the new permissions screen is ready.' },
  { start: 44, end: 49, speaker: 's1', text: 'Agreed. I’ll update the release notes and send them to marketing tomorrow.' },
];
const doneMeeting = {
  id: '2026-10-14-083000-launch-review', title: 'Launch review', started_ms: now - 26 * 3600000, duration_s: 2873, state: 'done',
  speakers: { you: 'You', s1: 'Priya', s2: 'Speaker 2' }, segments: sampleSegments, notices: [],
  summary: '## Summary\nThe beta went out last week and the crash rate dropped. Testers liked onboarding but found the permissions screen confusing.\n## Decisions\n- Ship next month if the new permissions screen is ready.\n## Action items\n- Speaker 2: redesign the permissions screen and share a draft by Thursday.\n- Priya: update the release notes and send them to marketing tomorrow.',
};
const liveMeeting = {
  id: '2026-10-15-091500-design-sync', title: 'Design sync', started_ms: now - 754000, duration_s: 754, state: 'recording',
  speakers: { you: 'You', s1: 'Speaker 1', s2: 'Speaker 2' }, segments: sampleSegments.slice(0, 5), notices: [], summary: null,
};
let meetingList = mq === 'empty' ? [] : mq === 'live' ? [liveMeeting, doneMeeting] : [doneMeeting,
  { id: '2026-10-12-140000-hiring-loop', title: 'Hiring loop debrief', started_ms: now - 3 * 86400000, duration_s: 1520, state: 'done', speakers: {}, segments: [], notices: [], summary: null },
  { id: '2026-10-10-100000-meeting', title: 'Meeting', started_ms: now - 5 * 86400000, duration_s: 312, state: 'interrupted', speakers: {}, segments: [], notices: [], summary: null }];
Object.assign(commands, {
  meeting_status: () => ({ live: mq === 'live' ? { id: liveMeeting.id, started_ms: liveMeeting.started_ms, recording: true } : null, computer_audio: !q.has('miconly'), speakers: !q.has('miconly'), screen_recording: !q.has('noscreen'), download_mb: mq === 'empty' ? 36 : 0 }),
  meeting_list: () => meetingList.map(m => ({ ...m, segments: [], summary: null })),
  meeting_get: ({ id }) => structuredClone(meetingList.find(m => m.id === id)),
  meeting_start: () => { throw 'Browser preview: meetings are not recorded here.'; },
  meeting_stop: () => null,
  meeting_rename_speaker: ({ id, speaker, name }) => { const m = meetingList.find(x => x.id === id); m.speakers[speaker] = name.trim(); return structuredClone(m); },
  meeting_delete: ({ id }) => { meetingList = meetingList.filter(m => m.id !== id); return null; },
  meeting_export: () => 'sample export',
  preview_sounds: () => null,
  show_pill_demo: () => null,
  meeting_save: () => '/Users/you/Downloads/launch-review.md',
});

const api = {
  core: {
    invoke: async (cmd, args = {}) => {
      if (!commands[cmd]) throw `Unknown command: ${cmd}`;
      await wait(20);
      return commands[cmd](args);
    },
  },
  event: {
    listen: async (event, cb) => {
      (listeners[event] ??= []).push(cb);
      return () => (listeners[event] = listeners[event].filter(x => x !== cb));
    },
  },
};
window.__TAURI__ = api;
window.__mock = { emit, commands };

// Say so on screen: everything in this preview is sample data.
const sidebar = document.querySelector('.sidebar');
if (sidebar) sidebar.append(Object.assign(document.createElement('p'), { className: 'preview-note', textContent: 'Browser preview, sample data' }));

// HTML5 drag and drop stands in for Tauri's native drag-drop events.
addEventListener('dragover', e => { e.preventDefault(); emit('tauri://drag-enter', {}); });
addEventListener('dragleave', e => { if (!e.relatedTarget) emit('tauri://drag-leave', {}); });
addEventListener('drop', e => {
  e.preventDefault();
  emit('tauri://drag-drop', { paths: [...e.dataTransfer.files].map(f => `/Users/you/Downloads/${f.name}`) });
});

// overlay.html?view=idle|hint and &hover=1: what pill.rs would send (the idle bar, its hint, ✕/✓ on hover).
if (q.has('view') || q.has('hover')) {
  setTimeout(() => emit('pill', { active: q.has('state'), hover: q.has('hover'), expand: q.get('view') === 'hint', hint: 'Hold ⌥ Space or click to dictate' }), 60);
}

// overlay.html?state=<state>&chip=<chip> (and index.html?state=... for the Home hero) renders any
// dictation state through the real overlay/Home code: state = recording|transcribing|cleaning|done|error|hint,
// chip = quiet|too_quiet|no_cleanup, plus message=, error=, action=, copied=1, locked=1, muted=1,
// card=1 (nowhere to paste; text= sets the transcript, nokey=1 drops the paste-last hint),
// offer=Start notes with state=hint&card=1&warning=...&text=... (a detected call's offer, calls.rs).
if (q.has('state')) {
  setTimeout(() => {
    const state = q.get('state');
    emit('dictation-state', {
      state, text: q.get('text') || (state === 'done' ? 'Let’s move the standup to 10:30 tomorrow.' : undefined), offer: q.get('offer') || undefined,
      card: q.has('card'), paste_key: q.has('card') && !q.has('nokey') ? '⌘ ⌃ V' : undefined,
      error: state === 'error' ? q.get('error') || 'Microphone access is off' : undefined,
      message: q.get('message') || undefined, chip: q.get('chip') || undefined, action: q.get('action') || undefined,
      warning: q.get('warning') || undefined, copied: q.has('copied'), locked: q.has('locked'), retry: q.has('retry'),
    });
    if (state === 'recording') {
      let i = 0;
      const muted = q.has('muted'), still = q.has('still');
      const level = () => emit('audio-level', { meter: muted ? 0 : still ? [0.75, 1, 0.9][i++ % 3] : Math.abs(Math.sin(i++ / 3)) * 0.5 + Math.random() * 0.25, muted });
      for (let k = 0; k < 4; k++) level(); // at once, so a screenshot never depends on (throttled) timers
      setInterval(level, 50);
    }
  }, 50);
}

// The dictation shortcut in the preview: hold ⌥ Space (or call __mock.hold(ms)) for what hotkey.rs
// and lib.rs send: hotkey-state with the keys, then a dictation of "Echotype, meet my Mac." A tap is
// a hint. From onboarding's step 3 on it counts as pasted into another app.
let held = 0, live = false;
async function release() {
  const ms = performance.now() - held;
  held = 0;
  emit('hotkey-state', false);
  clearInterval(levelTimer);
  if (!live) return;
  live = false;
  if (ms < 300) { emit('dictation-state', { state: 'hint', message: 'Hold ⌥ Space to dictate' }); return; }
  emit('dictation-state', { state: 'transcribing' }); await wait(650);
  emit('dictation-state', { state: 'done', text: 'Echotype, meet my Mac.', copied: settings.onboard_step < 3 && !settings.onboarded });
  await wait(400);
  emit('dictation-state', { state: 'idle' });
}
let levelTimer;
function press() {
  if (held) return;
  held = performance.now();
  emit('hotkey-state', true);
  const ready = models.some(m => m.downloaded);
  if (!ready) return emit('dictation-state', { state: 'hint', message: 'Model downloading 40%', action: 'model' });
  live = true;
  emit('dictation-state', { state: 'recording' });
  let i = 0;
  levelTimer = setInterval(() => emit('audio-level', { meter: Math.abs(Math.sin(i++ / 3)) * 0.55 + 0.15, muted: false }), 50);
}
addEventListener('keydown', e => { if (e.code === 'Space' && e.altKey && !e.repeat) { e.preventDefault(); press(); } }, true);
addEventListener('keyup', e => { if (held && (e.code === 'Space' || e.key === 'Alt')) { e.preventDefault(); release(); } }, true);
window.__mock.hold = async ms => { press(); await wait(ms); return release(); };
window.__mock.perm = perm;
