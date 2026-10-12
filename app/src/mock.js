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
  stt: { provider: 'local', local_model: q.has('onboarding') ? '' : 'large-v3-turbo', model: '', base_url: '' },
  cleanup: { enabled: true, provider: 'ollama', model: 'gemma3:4b', base_url: '', style: 'default', custom_instructions: '' },
  dictionary: ['Echotype', 'Tauri', 'Hinglish', 'Supabase'],
  replacements: [{ from: 'my email', to: 'aditya@example.com' }],
  restore_clipboard: true, history_enabled: true,
  onboarded: !q.has('onboarding'),
};
const presets = [
  { id: 'cloud', label: 'Echotype Cloud', stt: true, cleanup: true, base_url: 'https://echotype.zopmop.com/v1', default_stt_model: 'whisper-large-v3-turbo', default_cleanup_model: 'openai/gpt-oss-20b', needs_key: true },
  { id: 'openai', label: 'OpenAI', stt: true, cleanup: true, base_url: 'https://api.openai.com/v1', default_stt_model: 'gpt-4o-mini-transcribe', default_cleanup_model: 'gpt-4.1-mini', needs_key: true },
  { id: 'groq', label: 'Groq', stt: true, cleanup: true, base_url: 'https://api.groq.com/openai/v1', default_stt_model: 'whisper-large-v3-turbo', default_cleanup_model: 'llama-3.1-8b-instant', needs_key: true },
  { id: 'openrouter', label: 'OpenRouter', stt: false, cleanup: true, base_url: 'https://openrouter.ai/api/v1', default_stt_model: '', default_cleanup_model: 'meta-llama/llama-3.1-8b-instruct', needs_key: true },
  { id: 'ollama', label: 'Ollama', stt: false, cleanup: true, base_url: 'http://localhost:11434/v1', default_stt_model: '', default_cleanup_model: 'gemma3:4b', needs_key: false },
  { id: 'lmstudio', label: 'LM Studio', stt: false, cleanup: true, base_url: 'http://localhost:1234/v1', default_stt_model: '', default_cleanup_model: 'qwen3-4b', needs_key: false },
  { id: 'custom', label: 'Custom', stt: true, cleanup: true, base_url: '', default_stt_model: '', default_cleanup_model: '', needs_key: false },
];
const models = [
  { id: 'tiny.en', label: 'Tiny', size_mb: 75, ram_mb: 273, english_only: true, translates: false, downloaded: false, note: 'Fastest. Fine for short English notes.' },
  { id: 'base.en', label: 'Base', size_mb: 142, ram_mb: 388, english_only: true, translates: false, downloaded: true, note: 'Small and quick, English only.' },
  { id: 'small', label: 'Small', size_mb: 466, ram_mb: 852, english_only: false, translates: true, downloaded: false, note: 'Multilingual. A good floor for Hindi and Hinglish.' },
  { id: 'medium', label: 'Medium', size_mb: 1533, ram_mb: 2100, english_only: false, translates: true, downloaded: false, note: 'Best for Hinglish to English. Slower than Turbo.' },
  { id: 'large-v3-turbo', label: 'Large v3 Turbo', size_mb: 1624, ram_mb: 2300, english_only: false, translates: false, downloaded: !q.has('onboarding'), note: 'Near large-v3 accuracy at a fraction of the time. Cannot translate.' },
  { id: 'large-v3', label: 'Large v3', size_mb: 3095, ram_mb: 3900, english_only: false, translates: true, downloaded: false, note: 'Most accurate, and the slowest.' },
];
const sys = { total_ram_gb: 24, available_ram_gb: 5.1, os: 'macos', arch: 'aarch64' };
for (const m of models) m.fits = m.ram_mb + 2048 <= sys.available_ram_gb * 1024;
models.find(m => m.id === 'large-v3-turbo').recommended = true;

const ago = min => Math.floor((now - min * 60000) / 1000);
let history = [
  { ts: ago(4), raw: 'haan toh um kal ki meeting mein we decided ki launch ko push karenge thursday tak', text: "In yesterday's meeting we decided to push the launch to Thursday.", stt_ms: 690, cleanup_ms: 540, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(38), raw: 'can you send me the uh the figma link no wait the loom link from the demo', text: 'Can you send me the Loom link from the demo?', stt_ms: 410, cleanup_ms: 610, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(95), raw: 'groceries eggs milk sourdough and two avocados', text: 'Groceries:\n- Eggs\n- Milk\n- Sourdough\n- Two avocados', stt_ms: 380, cleanup_ms: 720, stt_provider: 'local', cleanup_provider: 'ollama' },
  { ts: ago(60 * 26), raw: 'ship it after the tauri build passes on ci', text: 'Ship it after the Tauri build passes on CI.', stt_ms: 350, cleanup_ms: null, stt_provider: 'local', cleanup_provider: null },
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
let busy = false;

const commands = {
  get_settings: () => structuredClone(settings),
  save_settings: ({ settings: s }) => {
    if (s.hotkey === 'CommandOrControl+Space') throw 'macOS uses this shortcut for Spotlight.';
    Object.assign(settings, structuredClone(s));
    return null;
  },
  get_presets: () => presets,
  set_secret: ({ name }) => { secrets.add(name); return null; },
  has_secret: ({ name }) => secrets.has(name),
  delete_secret: ({ name }) => { secrets.delete(name); return null; },
  list_models: () => structuredClone(models),
  download_model: async ({ id }) => {
    const m = models.find(x => x.id === id), total = m.size_mb * 1048576;
    for (let i = 1; i <= 40; i++) { await wait(70); emit('model-download', { id, downloaded: (total * i) / 40, total, done: false }); }
    m.downloaded = true;
    emit('model-download', { id, downloaded: total, total, done: true });
    return null;
  },
  delete_model: ({ id }) => { models.find(x => x.id === id).downloaded = false; return null; },
  list_input_devices: () => ['MacBook Pro Microphone', 'AirPods Pro', 'Shure MV7'],
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
    for (let i = 0; i < 60; i++) { await wait(50); emit('audio-level', { level: Math.abs(Math.sin(i / 4)) * 0.6 + Math.random() * 0.2 }); }
    emit('dictation-state', { state: 'transcribing' }); await wait(700);
    emit('dictation-state', { state: 'cleaning' }); await wait(600);
    emit('dictation-state', { state: 'done', text: 'Let’s move the standup to 10:30 tomorrow.' });
    await wait(400);
    emit('dictation-state', { state: 'idle' });
    busy = false;
    return null;
  },
  check_permissions: () => ({ microphone: 'granted', accessibility: !q.has('noaccess') }),
  open_permission_settings: () => null,
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

window.__TAURI__ = {
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

// overlay.html?state=recording|transcribing|cleaning|done|error shows that state.
if (q.has('state')) {
  setTimeout(() => {
    const state = q.get('state');
    emit('dictation-state', { state, text: 'Pasted', error: state === 'error' ? 'Microphone is not available' : undefined });
    if (state === 'recording') {
      let i = 0;
      setInterval(() => emit('audio-level', { level: Math.abs(Math.sin(i++ / 3)) * 0.5 + Math.random() * 0.25 }), 50);
    }
  }, 50);
}
