// Browser preview only: fakes the Rust core (same commands and events) so the UI can be
// opened in a normal browser for design QA. Never loaded inside the Tauri app.
const q = new URLSearchParams(location.search);
const listeners = {};
const emit = (event, payload) => (listeners[event] || []).forEach(cb => cb({ event, payload }));
const wait = ms => new Promise(r => setTimeout(r, ms));
const now = Date.now();

const settings = {
  hotkey: 'Alt+Space', mode: 'hold', input_device: null, languages: [], translate_to_english: false,
  local_api: { enabled: false, port: 7811 },
  stt: { provider: 'local', local_model: q.has('onboarding') ? '' : 'large-v3-turbo-q5_0', model: '', base_url: '' },
  cleanup: { enabled: !q.has('nocleanup'), provider: 'ollama', model: 'gemma3:4b', base_url: '', style: 'default', custom_instructions: '',
    match_app: q.has('apps'), app_styles: q.has('apps') ? [{ id: 'com.apple.Terminal', name: 'Terminal', style: '' }, { id: 'com.google.Chrome', name: 'Google Chrome', style: 'casual' }] : [] },
  dictionary: ['Echotype', 'Tauri', 'Hinglish', 'Supabase'],
  replacements: [{ from: 'my email', to: 'aditya@example.com' }],
  restore_clipboard: true, history_enabled: true, voice_commands: true,
  usage_stats: !q.has('statsoff'), share_telemetry: q.has('telemetry'), display_name: q.get('name') || '',
  onboarded: !q.has('onboarding'), onboard_step: 0,
  talk_key: q.get('talk') || 'accelerator',
  paste_last_hotkey: q.has('upgrade') ? '' : 'CommandOrControl+Control+V',
  copy_last_hotkey: q.has('upgrade') ? '' : 'CommandOrControl+Control+C',
  command_hotkey: 'Alt+Shift+Space',
  autostart: !q.has('upgrade'), sounds: true, sound_pack: 'classic', pill_fade: true, orb_listen: 'listening', orb_think: 'working', orb_detail: 1, orb_color: '#eef1ff', pill_size: 1.5, ball_size: 1,
  advanced: q.has('advanced'), cleaner_memory: 'ready', cleaner_idle_unload: false, cleaner_idle_minutes: 10, cleaner_context: 1024, greetings: true, detect_meetings: true, prefer_builtin_mic: true, pause_music: false, meeting_auto_stop: true, meeting_audio_days: 30,
  call_start: { meet: 'ask', teams: 'ask', zoom: 'ask', slack: 'ask', facetime: 'ask', webex: 'ask', discord: 'ask', other: 'ask' }, cloud_moved: q.has('moved'),
  look: 'system',
  pill_idle: !q.has('pilloff'), pill_hidden_until: q.has('pillhidden') ? now + 42 * 60000 : 0, pill_in_recordings: false, live_text: true,
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
const cancelled = new Set(); // cancel_download: download_model stops at its next step
const models = [
  { id: 'large-v3-turbo-q5_0', label: 'Large v3 Turbo (Q5)', size_mb: 547, ram_mb: 761, downloaded: !q.has('onboarding') || q.get('model') === 'ready', note: 'Best default: near-large accuracy, fast, 99 languages. Cannot translate.' },
  { id: 'tiny.en', label: 'Tiny (English)', size_mb: 78, ram_mb: 224, downloaded: q.get('model') === 'tiny', note: 'English only. Fastest, least accurate.' },
];
// ?ram=8 previews an 8 GB Mac (Automatic's light setup and its warnings); ?cloud shows Echotype Cloud.
const low = q.get('ram') === '8';
const sys = { total_ram_gb: low ? 8 : 24, available_ram_gb: low ? 3.2 : 5.1, os: 'macos', arch: 'aarch64', can_autostart: q.get('appdir') !== '0', low_ram: low, cloud: q.has('cloud'),
  echotype_model_installed: true, echotype_model_mb: 401,
  cleaners: [{ id: 'echotype-tiny', label: 'Echotype Tiny', note: '0.5B. Light and quick; best for English.', downloadable: q.has('dl-cleaners'), size_mb: 401, ram_mb: 552, installed: true, automatic: true, recommended: true, warning: null },
    { id: 'echotype-medium', label: 'Echotype Medium', note: '1.5B. Catches more; uses about twice the memory.', downloadable: q.has('dl-cleaners'), size_mb: 940, ram_mb: 1099, installed: false, automatic: false, recommended: false, warning: low ? 'Uses about 1.1 GB while dictating. On an 8 GB Mac this can slow other apps.' : null }] };
for (const m of models) Object.assign(m, { english_only: m.id.endsWith('.en'), translates: false, fits: m.ram_mb + 2048 <= sys.available_ram_gb * 1024 });
const rec = models.find(m => m.id === 'large-v3-turbo-q5_0');
rec.recommended = true;
// Automatic: the recommendation once downloaded, else the downloaded model closest in RAM (stt.rs auto_model).
const automatic = () => {
  const have = models.filter(m => m.downloaded).sort((a, b) => (a.ram_mb > rec.ram_mb) - (b.ram_mb > rec.ram_mb) || Math.abs(a.ram_mb - rec.ram_mb) - Math.abs(b.ram_mb - rec.ram_mb));
  for (const m of models) m.automatic = m === (rec.downloaded ? rec : have[0] ?? rec);
};
for (const m of models) m.warning = low && m.ram_mb > rec.ram_mb ? `Uses about ${(m.ram_mb / 1024).toFixed(1)} GB while dictating. On an 8 GB Mac this can slow other apps.` : null;

const ago = min => Math.floor((now - min * 60000) / 1000);
let history = [
  { ts: ago(1), raw: "don't forget to not include the tax in the final invoice", text: "don't forget to not include the tax in the final invoice", cleaned: "Don't forget to include the tax in the final invoice.", stt_ms: 520, cleanup_ms: 480, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(2), raw: 'ping arnav and midan about the zozo day thing', text: 'Ping Arnav and Midan about the Zozo Day thing', stt_ms: 420, cleanup_ms: 380, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(3), raw: 'lets meet tomorrow', text: 'lets meet tomorrow', stt_ms: 300, cleanup_ms: 0, stt_provider: 'local', cleanup_provider: null, no_cleanup: true },
  { ts: ago(4), raw: 'haan toh um kal ki meeting mein we decided ki launch ko push karenge thursday tak', text: "In yesterday's meeting we decided to push the launch to Thursday.", stt_ms: 690, cleanup_ms: 540, stt_provider: 'local:large-v3-turbo-q5_0', cleanup_provider: 'ollama', original: 'Kal ki meeting mein we decided ki launch ko push karenge Thursday tak.' },
  { ts: ago(38), raw: 'can you send me the uh the figma link no wait the loom link from the demo', text: 'Can you send me the Loom link from the demo?', stt_ms: 410, cleanup_ms: 610, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(95), raw: 'groceries eggs milk sourdough and two avocados', text: 'Groceries:\n- Eggs\n- Milk\n- Sourdough\n- Two avocados', stt_ms: 380, cleanup_ms: 720, stt_provider: 'local', cleanup_provider: 'ollama' },
  // noclip: recorded before Echotype kept audio, so it has no Re-run (only the newest 20 do).
  { ts: ago(60 * 26), raw: 'ship it after the tauri build passes on ci', text: 'ship it after the tauri build passes on ci', stt_ms: 350, cleanup_ms: 0, stt_provider: 'local', cleanup_provider: null, no_cleanup: true, noclip: true },
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
if (q.has('builtin')) Object.assign(settings.cleanup, { enabled: true, provider: 'echotype', model: 'echotype-medium' });
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
      if (cancelled.delete(id)) { emit('model-download', { id, downloaded: 0, total, done: true, error: 'Download cancelled' }); throw 'Download cancelled'; }
      emit('model-download', { id, downloaded: (total * i) / steps, total, done: false });
    }
    m.downloaded = true;
    emit('model-download', { id, downloaded: total, total, done: true });
    return null;
  },
  cancel_download: ({ id }) => { cancelled.add(id); return null; },
  delete_model: ({ id }) => { models.find(x => x.id === id).downloaded = false; return null; },
  list_input_devices: () => [
    { name: 'MacBook Pro Microphone', bluetooth: false, default: true, builtin: true },
    { name: 'AirPods Pro', bluetooth: true, default: false, builtin: false },
    { name: 'Shure MV7', bluetooth: false, default: false, builtin: false },
  ],
  // ?talk=fn&globe=0|1&im=0|1&relaunch=1 shows each talk-key state.
  shortcut_status: () => {
    const talk = settings.talk_key !== 'accelerator', globe = q.get('globe') !== '0', im = q.get('im') !== '0', relaunch = q.has('relaunch');
    const active = talk && (settings.talk_key !== 'fn' || globe) && im && !relaunch;
    return { talk_key_active: active, talk_key_error: talk && !active ? 'not active' : null, input_monitoring: im, globe_does_nothing: globe,
      needs_relaunch: talk && im && relaunch, unregistered_shortcuts: q.has('shortcutfail') && settings.hotkey === 'Alt+Space' ? ['hotkey'] : [], key_label: '⌥ Space' };
  },
  relaunch: () => null,
  // ?apps turns on "Match the app you're typing in", with a last dictation into Slack.
  installed_apps: () => [{ id: 'com.apple.mail', name: 'Mail', style: 'formal' }, { id: 'com.tinyspeck.slackmacgap', name: 'Slack', style: 'casual' }, { id: 'com.apple.TextEdit', name: 'TextEdit', style: '' }],
  app_notices: () => ({ current: '0.1.0', blocked: q.has('retired'), min_version: q.has('retired') ? '0.2.0' : '0.1.0', notices: q.has('nonotice') ? [] : [{ id: 'models-1', title: 'Echotype models are here', body: 'Download Echotype Tiny or Medium for better cleanup on this Mac.', action_label: 'Open Models', action: 'page:speech' }] }),
  check_update: () => ({ current: '0.1.0', version: q.has('update') ? '0.2.0' : null, notes: null }),
  install_update: () => { throw 'Browser preview: updates install in the app.'; },
  last_app: () => (q.has('apps') ? { id: 'com.tinyspeck.slackmacgap', name: 'Slack', style: 'casual' } : null),
  get_history: ({ limit }) => (settings.history_enabled ? history.slice(0, limit) : []),
  wispr_import_preview: () => (q.has('nowispr') ? null : {
    words: ['Aditya Rohilla', 'Wispr Flow', 'ZopMop', 'Anshi', 'xoxoday'], replacements: [{ from: 'my email address', to: 'me@example.com' }],
    languages: ['en'], app_styles: [{ id: 'com.tinyspeck.slackmacgap', name: 'Slack', style: 'casual' }, { id: 'net.whatsapp.WhatsApp', name: 'WhatsApp', style: 'casual' }],
    talk_key: 'left_control', sounds: true, pause_music: false, autostart: true, look: 'light', display_name: 'Aditya',
  }),
  wispr_import: () => {
    settings.display_name ||= 'Aditya';
    return ['5 dictionary words', '1 snippet', 'your languages', 'styles for Slack, WhatsApp', 'your talk key (Left ⌃)'];
  },
  update_history: ({ ts, text }) => {
    const e = history.find(x => x.ts === ts);
    if (!e) throw 'That entry is no longer in history.';
    e.text = text;
    return null;
  },
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
    for (let i = 0; i < 60; i++) {
      await wait(50);
      emit('audio-level', { meter: Math.abs(Math.sin(i / 4)) * 0.6 + Math.random() * 0.2, muted: false });
      if (i >= 30 && i % 10 === 0) partialOf('Let’s move the standup to 10:30 tomorrow.', (i - 20) / 40);
    }
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
  pill_copy_cleaned: () => null,
  // Re-run last or History entry `ts` (lib.rs rerun_options / rerun): ?norerun = no clip kept.
  rerun_options: ({ ts }) => {
    const e = ts == null ? history[0] : history.find(x => x.ts === ts);
    if (q.has('norerun') || !e || e.noclip || history.indexOf(e) >= 20) return [];
    return [{ id: 'lang:auto', label: 'Auto-detect language' }, { id: 'model:small', label: 'With Small' }, e.cleanup_provider && { id: 'raw', label: 'Without cleanup' }].filter(Boolean);
  },
  rerun: async ({ id, ts }) => {
    const e = ts == null ? history[0] : history.find(x => x.ts === ts);
    if (!e) throw 'There is no recording to re-run';
    emit('dictation-state', { state: 'transcribing' }); await wait(500);
    const text = id === 'raw' ? e.raw : e.raw[0].toUpperCase() + e.raw.slice(1) + '.';
    Object.assign(e, { original: e.original ?? e.text, text, stt_ms: 610, cleanup_ms: id === 'raw' ? 0 : 450, stt_provider: id.startsWith('model:') ? `local:${id.slice(6)}` : e.stt_provider, cleanup_provider: id === 'raw' ? null : e.cleanup_provider, no_cleanup: id === 'raw', cleaned: undefined });
    emit('dictation-state', { state: 'done', text, copied: true, card: true, warning: 'Re-run copied, ⌘V to paste' });
    return null;
  },
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
  quit_app: () => null,
  cloud_start_login: async ({ email }) => {
    await wait(500);
    if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) throw 'Enter a valid email address.';
    return null;
  },
  cloud_sign_in_with: async ({ provider }) => {
    await wait(2500);
    if (q.has('signinfail')) throw 'Sign-in did not finish: The user cancelled';
    const email = provider === 'apple' ? 'x7k2p9@privaterelay.appleid.com' : 'you@gmail.com';
    cloudAccount = { ...sampleAccount('free'), email };
    return { email, plan: 'free' };
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
  summary: q.has('nosummary') ? null : '## Summary\nThe beta went out last week and the crash rate dropped. Testers liked onboarding but found the permissions screen confusing.\n## Decisions\n- Ship next month if the new permissions screen is ready.\n## Action items\n- Speaker 2: redesign the permissions screen and share a draft by Thursday.\n- Priya: update the release notes and send them to marketing tomorrow.',
};
const liveMeeting = {
  id: '2026-10-15-091500-design-sync', title: 'Design sync', started_ms: now - 754000, duration_s: 754, state: 'recording',
  speakers: { you: 'You', s1: 'Speaker 1', s2: 'Speaker 2' }, segments: sampleSegments.slice(0, 5), notices: [], summary: null, name_hints: { s1: 'Priya' },
};
let meetingList = mq === 'empty' ? [] : mq === 'live' ? [liveMeeting, doneMeeting] : [doneMeeting,
  { id: '2026-09-12-140000-hiring-loop', title: 'Hiring loop debrief', started_ms: now - 34 * 86400000, duration_s: 1520, state: 'done', speakers: { you: 'You', s1: 'Speaker 1' }, summary: null,
    segments: [{ start: 4, end: 9, speaker: 's1', text: 'The candidate was strong on system design but the onboarding exercise ran long.' }, { start: 12, end: 15, speaker: 'you', text: 'Let’s ship the offer by Friday.' }],
    notices: ['The audio was deleted 30 days after the meeting to save space (Settings → Meetings). The transcript stays, but speakers can’t be re-checked without the audio.'] },
  { id: '2026-10-10-100000-meeting', title: 'Meeting', started_ms: now - 5 * 86400000, duration_s: 312, state: 'interrupted', speakers: {}, segments: [], notices: [], summary: null }];
Object.assign(commands, {
  meeting_status: () => ({ live: mq === 'live' ? { id: liveMeeting.id, started_ms: liveMeeting.started_ms, recording: true } : null, computer_audio: !q.has('miconly'), speakers: !q.has('miconly'), screen_recording: !q.has('noscreen'), download_mb: mq === 'empty' ? 36 : 0, summaries: q.has('cloud') }),
  meeting_window: () => null,
  meeting_ask: async ({ question }) => {
    if (!q.has('cloud')) throw 'Asking about a meeting is coming with Echotype Pro.';
    await new Promise(r => setTimeout(r, 400));
    return `Sample answer to "${question}" from the preview.`;
  },
  meeting_summarize: async ({ id }) => {
    if (!q.has('cloud')) throw 'Meeting summaries are coming with Echotype Pro.';
    await new Promise(r => setTimeout(r, 600));
    const m = meetingList.find(x => x.id === id);
    m.summary = '## Summary\nSample notes from the preview.\n## Decisions\nNone recorded.\n## Action items\nNone recorded.';
    return structuredClone(m);
  },
  meeting_list: () => meetingList.map(m => ({ ...m, segments: [], summary: null })),
  meeting_get: ({ id }) => structuredClone(meetingList.find(m => m.id === id)),
  // meeting.rs search: every word somewhere, up to 3 lines with the most words, in time order.
  meeting_search: ({ query }) => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    return meetingList.filter(m => words.length && words.every(w => [m.title, m.summary || '', ...m.segments.map(s => s.text)].some(t => t.toLowerCase().includes(w))))
      .map(m => ({ id: m.id, title: m.title, started_ms: m.started_ms, snippets: m.segments.map((s, i) => [words.filter(w => s.text.toLowerCase().includes(w)).length, i])
        .filter(([n]) => n).sort((a, b) => b[0] - a[0] || a[1] - b[1]).slice(0, 3).sort((a, b) => a[1] - b[1]).map(([, i]) => ({ start: m.segments[i].start, text: m.segments[i].text })) }));
  },
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

// What lib.rs sends while recording (dictation-partial): the words so far, cut like last_words
// to their last 24 characters from a word start. `done` (0..1) is how much has been said.
function partialOf(text, done) {
  if (!settings.live_text) return;
  const so = text.split(' ').slice(0, Math.max(1, Math.round(text.split(' ').length * done))).join(' ');
  emit('dictation-partial', so.length <= 24 ? so : '…' + so.slice(-24).replace(/^\S*\s/, ''));
}

// overlay.html?state=<state>&chip=<chip> (and index.html?state=... for the Home hero) renders any
// dictation state through the real overlay/Home code: state = recording|transcribing|cleaning|done|error|hint,
// chip = quiet|too_quiet|no_cleanup, plus message=, error=, action=, copied=1, locked=1, muted=1,
// partial=... with state=recording (the words so far; partial=1 for a stock sentence),
// card=1 (nowhere to paste; text= sets the transcript, nokey=1 drops the paste-last hint),
// offer=Start notes with state=hint&card=1&warning=...&text=... (a detected call's offer, calls.rs),
// cleaned=... with state=done&card=1&nokey=1&warning=... (the guard pasted the words as spoken).
if (q.has('state')) {
  setTimeout(() => {
    const state = q.get('state');
    emit('dictation-state', {
      state, text: q.get('text') || (state === 'done' ? 'Let’s move the standup to 10:30 tomorrow.' : undefined), offer: q.get('offer') || undefined,
      card: q.has('card'), paste_key: q.has('card') && !q.has('nokey') ? '⌘ ⌃ V' : undefined,
      error: state === 'error' ? q.get('error') || 'Microphone access is off' : undefined,
      message: q.get('message') || undefined, chip: q.get('chip') || undefined, action: q.get('action') || undefined,
      warning: q.get('warning') || undefined, copied: q.has('copied'), locked: q.has('locked'), retry: q.has('retry'), cleaned: q.get('cleaned') || undefined,
    });
    if (state === 'recording') {
      let i = 0;
      const muted = q.has('muted'), still = q.has('still');
      const level = () => emit('audio-level', { meter: muted ? 0 : still ? [0.75, 1, 0.9][i++ % 3] : Math.abs(Math.sin(i++ / 3)) * 0.5 + Math.random() * 0.25, muted });
      for (let k = 0; k < 4; k++) level(); // at once, so a screenshot never depends on (throttled) timers
      setInterval(level, 50);
      const words = q.get('partial');
      if (words) partialOf(words === '1' ? 'And so, my fellow Americans, ask not what your country can do for you' : words, 1);
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
  levelTimer = setInterval(() => {
    emit('audio-level', { meter: Math.abs(Math.sin(i++ / 3)) * 0.55 + 0.15, muted: false });
    if (i >= 30 && i % 20 === 0) partialOf('Echotype, meet my Mac.', Math.min(1, i / 80));
  }, 50);
}
addEventListener('keydown', e => { if (e.code === 'Space' && e.altKey && !e.repeat) { e.preventDefault(); press(); } }, true);
addEventListener('keyup', e => { if (held && (e.code === 'Space' || e.key === 'Alt')) { e.preventDefault(); release(); } }, true);
window.__mock.hold = async ms => { press(); await wait(ms); return release(); };
window.__mock.perm = perm;
