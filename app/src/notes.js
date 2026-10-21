// Notes page: a scratchpad. The list of notes (title from the first line, last edit) and one
// note's editor, autosaved to notes.json (notes.rs) with earlier versions to go back to.
// Dictating into the note works like any app: while the editor has focus, Rust sends the text
// here as `note-insert` instead of copying it (lib.rs deliver), and Command Mode and transforms
// read the selection this page reports (notes_target), not a ⌘C. main.js passes its helpers.
let $, h, invoke, copyText;
let notes = [], open = null; // open: the note in the editor; { id: null } until its first save
let editor = null, query = '', error = '', confirmDelete = false;
let saveTimer, chain = Promise.resolve(), sent, searchBox = null;
const SAVE_MS = 600;

const when = ms => new Date(ms).toLocaleString([], { month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
// As notes.rs title(): the first line with words.
const title = t => (t.split('\n').map(l => l.trim().replace(/^#+/, '').trim()).find(Boolean) || 'New note').slice(0, 80);
const words = t => (t.match(/\S+/g) || []).length;
const plural = (n, w) => `${n} ${w}${n === 1 ? '' : 's'}`;

export function init(helpers) {
  ({ $, h, invoke, copyText } = helpers);
  helpers.listen('note-insert', ({ payload }) => insert(payload));
  helpers.listen('note-new', () => newNote());
  document.addEventListener('echotype:page', e => { if (e.detail !== 'notes') { flush(); target(); } });
  document.addEventListener('selectionchange', target);
  addEventListener('focus', target);
  addEventListener('blur', target);
}

export async function show() {
  await chain; // a save just sent lands in the list
  try { notes = await invoke('notes_list'); error = ''; } catch (e) { error = String(e); }
  render();
}

// Tells Rust whether the editor has focus and what is selected in it (lib.rs note_target).
function target() {
  const on = !!editor && document.activeElement === editor && document.hasFocus() && !$('#notes').hidden;
  const sel = on ? editor.value.slice(editor.selectionStart, editor.selectionEnd) : null;
  if (sel === sent) return;
  sent = sel;
  invoke('notes_target', { selection: sel }).catch(() => {});
}

function render() {
  // The editor is kept across renders; put focus and the caret back where they were.
  const had = editor && document.activeElement === editor, a = editor?.selectionStart, b = editor?.selectionEnd;
  const typing = searchBox && document.activeElement === searchBox;
  $('#notes').replaceChildren(...(open ? viewNote(open) : viewList()).filter(k => k != null && k !== false && k !== ''));
  if (had) { editor.focus(); editor.setSelectionRange(a, b); }
  if (typing && !open) searchBox.focus();
}

// ---------- list ----------
function viewList() {
  const q = query.trim().toLowerCase().split(/\s+/).filter(Boolean); // as notes.rs search(): every word
  const shown = notes.filter(n => n.text.trim() && q.every(w => n.text.toLowerCase().includes(w)));
  searchBox ??= h('input', { type: 'search', className: 'note-search', placeholder: 'Search notes', 'aria-label': 'Search notes', spellcheck: false, oninput: () => { query = searchBox.value; render(); } });
  return [
    h('div', { className: 'page-head' }, h('h1', {}, 'Notes'), h('button', { className: 'btn primary', type: 'button', onclick: () => newNote() }, 'New note')),
    h('p', { className: 'lede' }, 'A scratchpad for thinking out loud. Dictate into a note as into any app, and select text to use Command Mode or a transform on it. New note by voice in the menu bar menu opens a note and starts listening.'),
    error && h('p', { className: 'note warn', role: 'alert' }, error),
    notes.length > 0 && searchBox,
    h('div', { className: 'list' }, shown.length ? shown.map(n => h('button', { className: 'row entry meeting-row', type: 'button', onclick: () => openNote(n) },
      h('span', { className: 'text' }, title(n.text)), h('span', { className: 'when' }, when(n.updated_ms))))
      : h('p', { className: 'empty' }, notes.length ? 'No notes match.' : 'Your notes show up here.')),
  ];
}

function openNote(n) {
  flush();
  open = n; confirmDelete = false; error = '';
  editor = h('textarea', { className: 'note-editor text', value: n.text, placeholder: 'Type or dictate', 'aria-label': 'Note' });
  editor.addEventListener('input', changed);
  editor.addEventListener('blur', () => flush());
  for (const ev of ['focus', 'blur', 'select', 'keyup', 'mouseup']) editor.addEventListener(ev, target);
  render();
  $('#main').scrollTop = 0;
}

// A new note, not saved until it has words (an empty one never reaches the list).
function newNote() {
  openNote({ id: null, text: '', updated_ms: 0, versions: [] });
  editor.focus();
  target();
}

function back() {
  flush();
  open = editor = null; confirmDelete = false;
  target();
  render(); // at once, then again with the saved list
  show();
}

// ---------- one note ----------
function viewNote(n) {
  const btn = (label, fn) => { const b = h('button', { className: 'btn', type: 'button' }, label); b.onclick = () => fn(b); return b; };
  // Newest first; picking one makes it the note again, and the text it replaces becomes a version.
  const versions = h('select', { 'aria-label': 'Earlier versions', disabled: !n.versions.length, onchange: e => {
    const v = n.versions[e.target.value];
    e.target.value = '';
    if (v) restore(v.text);
  } }, h('option', { value: '' }, n.versions.length ? 'Earlier versions…' : 'No earlier versions'),
    n.versions.map((v, i) => [i, v]).reverse().map(([i, v]) => h('option', { value: i }, `${when(v.ms)} · ${plural(words(v.text), 'word')}`)));
  return [
    h('button', { className: 'btn quiet back', type: 'button', onclick: back }, '‹ Notes'),
    h('div', { className: 'page-head' }, h('h1', { className: 'text', id: 'noteTitle' }, title(editor.value)),
      confirmDelete ? h('span', { className: 'confirm' }, 'Delete this note and its earlier versions?',
        h('button', { className: 'btn quiet', type: 'button', onclick: () => { confirmDelete = false; render(); } }, 'Cancel'),
        h('button', { className: 'btn danger', type: 'button', onclick: () => remove(n) }, 'Delete'))
        : h('button', { className: 'btn quiet danger', type: 'button', onclick: () => { confirmDelete = true; render(); } }, 'Delete')),
    h('div', { id: 'noteMeta' }, meta()),
    editor,
    h('div', { className: 'export' },
      btn('Copy', b => copyText(editor.value, b)),
      btn('Save as .md', async () => {
        await flush();
        if (open?.id == null) return;
        try { await invoke('notes_export', { id: open.id }); error = ''; } catch (e) { error = String(e); }
        renderMeta();
      }),
      versions),
  ];
}

function meta() {
  const dirty = editor.value !== open.text;
  return [
    h('p', { className: 'meeting-meta' }, dirty ? 'Saving…' : open.id == null ? 'Empty note' : `Edited ${when(open.updated_ms)}`, ' · ', plural(words(editor.value), 'word')),
    error && h('p', { className: 'note warn', role: 'alert' }, error),
  ].filter(Boolean);
}

// Only the status line while typing: a full render would rebuild the versions menu every keystroke.
function renderMeta() {
  if (open && editor) $('#noteMeta')?.replaceChildren(...meta());
}

function changed() {
  $('#noteTitle').textContent = title(editor.value);
  renderMeta();
  clearTimeout(saveTimer);
  saveTimer = setTimeout(flush, SAVE_MS);
}

// Saves the editor now (debounced autosave, leaving the note or the page). Saves run one after
// another, so a new note's first save hands its id to the next.
function flush() {
  clearTimeout(saveTimer);
  return open && editor ? save(open, editor.value, false) : chain;
}

function save(n, text, keep) {
  chain = chain.then(async () => {
    if (n.id == null ? !text.trim() : text === n.text && !keep) return;
    try {
      const got = await invoke('notes_save', { id: n.id, text, keep });
      const fresh = n.id == null, had = n.versions.length;
      Object.assign(n, got);
      if (n === open && n.versions.length !== had) render(); // a new version for the menu
      if (fresh) notes.unshift(n);
      notes.sort((x, y) => y.updated_ms - x.updated_ms);
      error = '';
    } catch (e) { error = String(e); }
    if (n === open) renderMeta();
  });
  return chain;
}

async function restore(text) {
  await flush();
  editor.value = text; // ponytail: ⌘Z can't step back over a restore; the text it replaced is a version
  $('#noteTitle').textContent = title(text);
  save(open, text, true);
}

async function remove(n) {
  clearTimeout(saveTimer);
  await chain;
  try {
    if (n.id != null) await invoke('notes_delete', { id: n.id });
    notes = notes.filter(x => x !== n);
    open = editor = null; confirmDelete = false; error = '';
    target();
  } catch (e) { error = String(e); }
  render();
}

// A dictation or Command Mode result for the focused note: in at the cursor, over the selection,
// as a paste would be, with a space after a word. insertText keeps it on the editor's ⌘Z.
function insert(text) {
  if (!editor || !open) return;
  const a = editor.selectionStart, b = editor.selectionEnd, before = editor.value.slice(0, a);
  const t = (a === b && before && !/\s$/.test(before) && !/^[\s.,!?;:)]/.test(text) ? ' ' : '') + text;
  editor.focus();
  editor.setSelectionRange(a, b);
  if (!document.execCommand('insertText', false, t)) { editor.setRangeText(t, a, b, 'end'); changed(); }
}
