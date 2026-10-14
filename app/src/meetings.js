// Meetings page: start/stop meeting notes, the list of meetings, and one meeting's live
// transcript, speakers (renamable), summary, copy/export and delete. main.js passes its helpers.
let $, h, invoke, copyText;
let meetings = [], status = { live: null, computer_audio: true, speakers: true, screen_recording: true, download_mb: 0 };
let open = null; // the meeting being viewed
let editing = null; // speaker id being renamed
let confirmDelete = false, startError = '', timer, starting = false, draftTitle = '';
let exportsOf = null; // { id, md, txt }, fetched with the meeting so Copy runs inside the click

const clock = s => {
  s = Math.max(0, Math.floor(s));
  const mm = String(Math.floor(s / 60) % 60).padStart(2, '0'), ss = String(s % 60).padStart(2, '0');
  return s >= 3600 ? `${Math.floor(s / 3600)}:${mm}:${ss}` : `${mm}:${ss}`;
};
const when = ms => new Date(ms).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const STATE = { recording: 'Recording', processing: 'Writing up', interrupted: 'Interrupted' };
const live = () => status.live?.recording ? status.live : null;
const elapsed = () => (Date.now() - live().started_ms) / 1000;

// You stay neutral; other speakers cycle through the accent and status tokens (a dot, not the text).
const SPEAKER_TOKENS = ['--accent', '--warn', '--ok', '--bad', '--fg-3'];
const colour = id => id === 'you' ? 'var(--fg)' : `var(${SPEAKER_TOKENS[((parseInt(id.slice(1), 10) || 1) - 1) % SPEAKER_TOKENS.length]})`;
const dot = id => h('i', { className: 'spk-dot', style: `background:${colour(id)}` });

export function init(helpers) {
  ({ $, h, invoke, copyText } = helpers);
  helpers.listen('meeting', async ({ payload }) => {
    await refresh();
    if (open?.id === payload.id) await load(payload.id).catch(() => {});
    if (editing === null) render(); // a live update must not wipe a rename being typed
  });
}

export async function show() {
  await refresh();
  if (open) await load(open.id).catch(() => { open = null; });
  render();
}

async function load(id) {
  open = await invoke('meeting_get', { id });
  exportsOf = null;
  if (open.state !== 'recording' && open.segments.length) {
    const [md, txt] = await Promise.all([invoke('meeting_export', { id, format: 'md' }), invoke('meeting_export', { id, format: 'txt' })]);
    exportsOf = { id, md, txt };
  }
}

async function refresh() {
  [meetings, status] = await Promise.all([invoke('meeting_list').catch(() => []), invoke('meeting_status').catch(() => status)]);
}

function render() {
  clearInterval(timer);
  if (live()) timer = setInterval(tick, 1000);
  const root = $('#meetings');
  root.replaceChildren(...(open ? viewMeeting(open) : viewList()).filter(k => k != null && k !== false && k !== ''));
}

function tick() {
  if (!live()) return clearInterval(timer);
  for (const el of document.querySelectorAll('[data-elapsed]')) el.textContent = clock(elapsed());
}

// ---------- list ----------
function startCard() {
  if (live()) {
    return h('div', { className: 'group' },
      h('div', { className: 'row' },
        h('span', { className: 'row-text' },
          h('span', { className: 'row-title rec-line' }, h('i', { className: 'rec-dot' }), 'Recording · ', h('span', { 'data-elapsed': '' }, clock(elapsed()))),
          h('span', { className: 'row-desc' }, 'Transcribing as you talk. Stop when the meeting ends.')),
        h('span', { className: 'actions' },
          h('button', { className: 'btn', type: 'button', onclick: () => openMeeting(live().id) }, 'View transcript'),
          h('button', { className: 'btn primary', type: 'button', onclick: stop }, 'Stop'))));
  }
  const title = h('input', { type: 'text', id: 'meetingTitle', value: draftTitle, placeholder: 'Title (optional)', 'aria-label': 'Meeting title', spellcheck: false, oninput: () => (draftTitle = title.value) });
  title.addEventListener('keydown', e => { if (e.key === 'Enter') start(title.value); });
  return h('div', { className: 'group' },
    h('div', { className: 'row' }, title, h('button', { className: 'btn primary', type: 'button', onclick: () => start(title.value) }, 'Start meeting notes')),
    h('div', { className: 'row' }, h('span', { className: 'row-text' },
      h('span', { className: 'row-desc consent' }, 'Let everyone know this meeting is being transcribed.'),
      status.download_mb > 0 && h('span', { className: 'row-desc' }, `The first meeting downloads ${status.download_mb} MB of speaker models.`),
      startError && h('span', { className: 'row-desc error', role: 'alert' }, startError))),
    !status.computer_audio && h('div', { className: 'row' },
      h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, 'Microphone only on this OS'),
        h('span', { className: 'row-desc' }, 'Echotype transcribes your microphone. Capturing the other people in the call from the computer’s audio is not available here yet.'))),
    !status.screen_recording && h('div', { className: 'row' },
      h('span', { className: 'row-text' }, h('span', { className: 'row-title' }, 'Computer audio is off'),
        h('span', { className: 'row-desc' }, 'Without Screen Recording permission only your microphone is transcribed. Echotype records sound only, never your screen.')),
      h('button', { className: 'btn', type: 'button', onclick: () => invoke('open_permission_settings', { kind: 'screen_recording' }) }, 'Allow')));
}

function viewList() {
  const rows = meetings.map(m => h('button', { className: 'row entry meeting-row', type: 'button', onclick: () => openMeeting(m.id) },
    h('span', { className: 'text' }, m.title),
    STATE[m.state] && h('span', { className: 'tag' }, STATE[m.state]),
    h('span', { className: 'when' }, `${when(m.started_ms)} · ${clock(m.duration_s)}`)));
  return [
    h('h1', {}, 'Meetings'),
    h('p', { className: 'lede' }, !status.computer_audio ? 'Notes for calls and meetings, transcribed from your microphone.'
      : status.speakers ? 'Notes for calls and meetings. Your microphone is You; everyone else comes from the computer’s audio and is told apart on this Mac.'
      : 'Notes for calls and meetings. Your microphone is You; everyone else comes from the computer’s audio, labelled Others.'),
    startCard(),
    h('h2', { className: 'group-title' }, 'Past meetings'),
    h('div', { className: 'list' }, rows.length ? rows : h('p', { className: 'empty' }, 'Your meetings show up here.')),
  ];
}

async function start(title) {
  if (starting) return; // a double click must not start (and fail) twice
  starting = true; startError = '';
  try { const m = await invoke('meeting_start', { title: title || null }); await refresh(); open = m; exportsOf = null; draftTitle = ''; }
  catch (e) { startError = String(e); }
  starting = false;
  render();
}

async function stop() {
  try { await invoke('meeting_stop'); } catch (e) { startError = String(e); }
  await refresh();
  if (open) await load(open.id).catch(() => {}); // now "processing": no Stop, no Delete
  render();
}

async function openMeeting(id) {
  editing = null; confirmDelete = false; startError = '';
  try { await load(id); } catch (e) { startError = String(e); open = null; await refresh(); }
  render();
  $('#main').scrollTop = 0;
}

// ---------- one meeting ----------
const name = (m, id) => m.speakers[id] || id;

function speakerChip(m, id) {
  if (editing === id) {
    const input = h('input', { type: 'text', value: name(m, id), 'aria-label': `Rename ${name(m, id)}`, maxLength: 60, spellcheck: false });
    const done = async save => {
      if (editing !== id) return;
      editing = null;
      if (save && input.value.trim() && input.value.trim() !== name(m, id)) {
        try { await invoke('meeting_rename_speaker', { id: m.id, speaker: id, name: input.value }); await load(m.id); } // exports carry the new name
        catch (e) { startError = String(e); }
      }
      render();
    };
    input.addEventListener('keydown', e => { if (e.key === 'Enter') done(true); if (e.key === 'Escape') done(false); });
    input.addEventListener('blur', () => done(true));
    setTimeout(() => { input.focus(); input.select(); });
    return h('span', { className: 'chip spk editing' }, dot(id), input);
  }
  return h('button', { className: 'chip spk', type: 'button', title: 'Rename', onclick: () => { editing = id; render(); } }, dot(id), name(m, id));
}

// The summary is Markdown from the model: headings and bullets, shown as text (never parsed as HTML).
function summary(md) {
  const out = [];
  let list = null;
  for (const line of md.split('\n')) {
    const t = line.trim();
    if (!t) { list = null; continue; }
    if (/^#{1,6}\s/.test(t)) { list = null; out.push(h('h3', {}, t.replace(/^#+\s*/, ''))); }
    else if (/^[-*•]\s/.test(t)) { if (!list) out.push(list = h('ul', {})); list.append(h('li', {}, t.replace(/^[-*•]\s*/, '').replace(/\*\*/g, ''))); }
    else { list = null; out.push(h('p', {}, t.replace(/\*\*/g, ''))); }
  }
  return h('div', { className: 'summary text' }, out);
}

function viewMeeting(m) {
  const isLive = live()?.id === m.id;
  const ids = Object.keys(m.speakers).filter(id => m.segments.some(s => s.speaker === id));
  const btn = (label, fn) => { const b = h('button', { className: 'btn', type: 'button' }, label); b.onclick = () => fn(b); return b; };
  const failed = e => { startError = String(e); render(); };
  const saved = b => { b.textContent = 'Saved to Downloads'; setTimeout(() => (b.textContent = b.dataset.label), 1500); };
  const save = fmt => b => { b.dataset.label = b.textContent; invoke('meeting_save', { id: m.id, format: fmt }).then(() => saved(b), failed); };
  const meta = isLive
    ? [h('span', { className: 'rec-line' }, h('i', { className: 'rec-dot' }), 'Recording · ', h('span', { 'data-elapsed': '' }, clock(elapsed())))]
    : [`${when(m.started_ms)} · ${clock(m.duration_s)}`, STATE[m.state] && h('span', { className: 'tag' }, STATE[m.state])];
  return [
    h('button', { className: 'btn quiet back', type: 'button', onclick: () => { open = null; startError = ''; show(); } }, '‹ Meetings'),
    h('div', { className: 'page-head' }, h('h1', { className: 'text' }, m.title),
      isLive ? h('button', { className: 'btn primary', type: 'button', onclick: stop }, 'Stop')
        : confirmDelete ? h('span', { className: 'confirm' }, 'Delete this meeting and its audio?',
          h('button', { className: 'btn quiet', type: 'button', onclick: () => { confirmDelete = false; render(); } }, 'Cancel'),
          h('button', { className: 'btn danger', type: 'button', onclick: async () => {
            try { await invoke('meeting_delete', { id: m.id }); open = null; confirmDelete = false; await show(); }
            catch (e) { startError = String(e); render(); }
          } }, 'Delete'))
        : h('button', { className: 'btn quiet danger', type: 'button', disabled: m.state === 'processing', onclick: () => { confirmDelete = true; render(); } }, 'Delete')),
    h('p', { className: 'meeting-meta' }, meta),
    ...m.notices.map(n => h('p', { className: 'note' }, n)),
    startError && h('p', { className: 'note warn', role: 'alert' }, startError),
    m.state === 'processing' && h('p', { className: 'note working' }, 'Telling speakers apart and writing the summary'),
    m.summary && summary(m.summary),
    exportsOf?.id === m.id && h('div', { className: 'export' },
      btn('Copy Markdown', b => copyText(exportsOf.md, b)),
      btn('Copy text', b => copyText(exportsOf.txt, b)),
      btn('Save .md', save('md')),
      btn('Save .txt', save('txt'))),
    ids.length > 0 && h('div', { className: 'speakers' }, h('span', { className: 'speakers-label' }, 'Speakers'), ids.map(id => speakerChip(m, id))),
    h('div', { className: 'transcript text', 'aria-live': isLive ? 'polite' : null },
      m.segments.length ? m.segments.map(s => h('div', { className: 'line' },
        h('time', {}, clock(s.start)),
        h('span', { className: 'who' }, dot(s.speaker), name(m, s.speaker)),
        h('span', { className: 'said' }, s.text)))
        : h('p', { className: 'empty' }, isLive ? 'Listening. The first lines appear about 25 seconds in.' : 'Nothing was said.')),
  ];
}
