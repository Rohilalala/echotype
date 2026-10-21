// The Stats page and the Home strip, drawn from get_stats (stats.rs). Counts only, kept on this computer.
const $ = (s, el = document) => el.querySelector(s);
let api, view = null, err = null, loaded = false, seq = 0;
let bodyOn; // usage_stats when #statsBody was last drawn

const num = x => Math.round(x).toLocaleString();
const words = x => `${num(x)} ${Math.round(x) === 1 ? 'word' : 'words'}`;
const days = x => `${num(x)} ${x === 1 ? 'day' : 'days'}`;
const date = s => { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); };
const month = d => d.toLocaleDateString([], { month: 'short' });
// 14 Oct, with the year when it is not this year (the grid reaches back 12 months).
const dayMonth = s => { const d = date(s); return `${d.getDate()} ${month(d)}${d.getFullYear() === new Date().getFullYear() ? '' : ` ${d.getFullYear()}`}`; };
const dayLabel = s => `${date(s).toLocaleDateString([], { weekday: 'short' })} ${dayMonth(s)}`; // Tue 14 Oct
const daysBetween = (a, b) => Math.round((date(b) - date(a)) / 864e5);

const MILESTONES = [
  ['words_1000', 'First 1,000 words', 1000],
  ['words_10000', '10,000 words', 10000],
  ['words_100000', '100,000 words', 100000],
  ['streak_7', '7-day streak'],
  ['streak_30', '30-day streak'],
];

export function init(deps) {
  api = deps;
  api.listen('stats-updated', refresh);
  // Settings saves (the name, every keystroke) redraw only what reads settings, so a focused grid survives.
  api.listen('settings-changed', renderSettings);
  $('[data-bind="usage_stats"]').addEventListener('change', renderSettings);
  $('[data-bind="display_name"]').addEventListener('input', renderTitle);
  window.addEventListener('focus', refresh); // "today" moves on at midnight while the window is away
  $('#statsOn').addEventListener('click', () => {
    const sw = $('[data-bind="usage_stats"]');
    sw.checked = true;
    sw.dispatchEvent(new Event('change')); // the bound switch saves it
    sw.focus(); // the notice holding this button goes away
  });
  const confirm = (on, focus = $('#clearStats')) => {
    $('#clearStats').hidden = on;
    $('#clearStatsConfirm').hidden = !on;
    focus?.focus();
  };
  document.addEventListener('echotype:page', e => { confirm(false, null); if (e.detail === 'stats') refresh(); });
  $('#clearStats').addEventListener('click', () => confirm(true, $('#clearStatsCancel')));
  $('#clearStatsCancel').addEventListener('click', () => confirm(false));
  $('#clearStatsConfirm').addEventListener('keydown', e => { if (e.key === 'Escape') confirm(false); });
  $('#shareCopy').addEventListener('click', e => copyCard(e.currentTarget));
  $('#shareSave').addEventListener('click', e => saveCard(e.currentTarget));
  $('#clearStatsYes').addEventListener('click', async () => {
    try { await api.invoke('clear_stats'); }
    catch (e) { confirm(false); err = `Couldn't clear stats: ${e}`; return renderPage(); }
    confirm(false, $('#statsTitle')); // Clear stats is disabled once there is nothing to clear
    refresh();
  });
  refresh();
}

async function refresh() {
  const n = ++seq;
  let next, e2 = null;
  try { next = await api.invoke('get_stats', { range: 53 }); }
  catch (e) { e2 = `Couldn't read stats: ${e}`; }
  if (n !== seq) return; // a newer refresh started meanwhile
  if (next) view = next;
  err = e2;
  loaded = true;
  renderTitle();
  renderStrip();
  renderPage();
}

function renderSettings() {
  renderTitle();
  renderStrip();
  if (bodyOn !== api.settings().usage_stats) renderPage();
}

function renderTitle() {
  const name = (api.settings().display_name || '').trim();
  $('#statsTitle').textContent = name ? `${name}’s stats` : 'Your stats';
}

// ---------- Home ----------
function renderStrip() {
  const el = $('#statsStrip'), { h } = api;
  el.hidden = !view || !api.settings().usage_stats || !view.count_total;
  if (el.hidden) return;
  const v = view, part = (value, label) => h('span', {}, h('b', {}, value), h('small', {}, label));
  const parts = [
    part(num(v.words_today), v.words_today === 1 ? 'word today' : 'words today'),
    part(num(v.words_week), 'this week'),
    v.last?.wpm && part(`${num(v.last.wpm)} wpm`, 'last dictation'),
    v.streak > 1 && part(days(v.streak), 'streak'),
  ].filter(Boolean);
  el.replaceChildren(...parts);
  el.title = 'Open Stats';
  el.onclick = () => api.go('stats');
}

// ---------- Stats page ----------
function renderPage() {
  const { h } = api, S = api.settings();
  $('#statsOff').hidden = S.usage_stats;
  $('#clearStats').disabled = !view?.count_total;
  $('#statsShare').hidden = !!err || !view?.count_total;
  const body = $('#statsBody'), gridFocused = document.activeElement?.classList.contains('heat');
  bodyOn = S.usage_stats;
  if (err) return body.replaceChildren(h('p', { className: 'empty', role: 'alert' }, err, h('button', { className: 'btn', type: 'button', onclick: refresh }, 'Try again')));
  if (!loaded || !view) return body.replaceChildren(h('p', { className: 'empty' }, 'Loading stats…'));
  const v = view, has = v.count_total > 0;
  body.replaceChildren(...[
    !has && h('p', { className: 'stats-empty' }, S.usage_stats
      ? `Nothing counted yet. ${api.keys()} in any text field and speak; your words and speed show up here.`
      : 'Nothing counted yet, and counting is off.'),
    has && speed(v),
    has && wordsBlock(v),
    grid(v),
    has && facts(v),
    milestones(v),
  ].filter(Boolean));
  if (gridFocused) $('.heat', body).focus(); // a dictation landed while reading the grid
}

const fig = (label, value, unit, sub, big) => api.h('div', { className: 'fig' + (big ? ' big' : '') },
  api.h('span', { className: 'fig-label' }, label),
  api.h('span', { className: 'fig-value' }, value, unit && api.h('small', {}, unit)),
  api.h('span', { className: 'fig-sub' }, sub));

function speed(v) {
  const { h } = api, last = v.last;
  const lastSub = !last ? 'No dictation yet' : last.wpm ? `Last dictation, ${words(last.words)}` : 'Last dictation was too short to time';
  return h('div', {},
    h('h2', { className: 'group-title' }, 'Speed'),
    h('div', { className: 'group figs' },
      fig('Right now', last?.wpm ? num(last.wpm) : '–', last?.wpm && 'wpm', lastSub, true),
      fig('Average', v.avg_wpm ? num(v.avg_wpm) : '–', v.avg_wpm && 'wpm', v.avg_wpm_week ? `${num(v.avg_wpm_week)} wpm this week` : 'None timed this week'),
      fig('Best', v.best_wpm ? num(v.best_wpm.value) : '–', v.best_wpm && 'wpm', v.best_wpm ? dayLabel(v.best_wpm.date) : 'Not timed yet')),
    h('p', { className: 'stats-note' }, 'Words per minute of dictations 3 seconds or longer, timed without the pauses before and after. Voice notes count words only.'));
}

function wordsBlock(v) {
  const { h } = api, top = Math.max(...v.week.map(c => c.words), 1);
  const week = h('ol', { className: 'week', 'aria-label': 'Words each day this week' }, v.week.map(c => {
    const future = c.date > v.today;
    return h('li', { className: 'week-day' + (c.date === v.today ? ' today' : ''), title: future ? '' : `${words(c.words)}${c.best_wpm ? ` · ${num(c.best_wpm)} wpm best` : ''} · ${dayLabel(c.date)}` },
      h('span', { className: 'week-bar', 'aria-hidden': 'true' }, !future && h('i', { style: `height: ${c.words ? Math.max(6, (c.words / top) * 100) : 0}%` })),
      h('b', {}, future ? '' : num(c.words)),
      h('span', {}, date(c.date).toLocaleDateString([], { weekday: 'short' })));
  }));
  return h('div', {},
    h('h2', { className: 'group-title' }, 'Words'),
    h('div', { className: 'group' },
      h('div', { className: 'figs' },
        fig('Today', num(v.words_today), '', v.words_today ? '' : 'None yet today'),
        fig('This week', num(v.words_week), '', 'Monday to Sunday'),
        fig('Total', num(v.words_total), '', `From ${num(v.count_total)} ${v.count_total === 1 ? 'recording' : 'recordings'}`)),
      week));
}

// The year grid: a column per week, Monday on top; shades are quartiles of the user's own active days.
function grid(v) {
  const { h } = api, cells = v.grid, weeks = Math.ceil(cells.length / 7);
  const sum = cells.reduce((a, c) => a + c.words, 0);
  const tip = h('div', { className: 'heat-tip', hidden: true, 'aria-hidden': 'true' });
  const live = h('span', { className: 'sr-only', 'aria-live': 'polite' });
  const box = h('div', { className: 'heat', tabIndex: 0, role: 'group', 'aria-label': `Words per day for the last ${weeks} weeks. Arrow keys move between days.` });
  box.style.gridTemplateColumns = `auto repeat(${weeks}, minmax(0, 1fr))`;
  let prev = -1;
  for (let w = 0; w < weeks; w++) {
    const d = date(cells[w * 7].date), m = d.getMonth();
    // A label where a month's first Monday starts a column, with room for it before the next one.
    if (m !== prev && w <= weeks - 3 && (w > 0 || d.getDate() <= 8)) box.append(h('span', { className: 'heat-month', style: `grid-area: 1 / ${w + 2} / 2 / span 3` }, month(d)));
    prev = m;
  }
  ['Mon', 'Wed', 'Fri'].forEach((_, i) => {
    const monday = date(cells[0].date); monday.setDate(monday.getDate() + i * 2);
    box.append(h('span', { className: 'heat-wd', style: `grid-area: ${i * 2 + 2} / 1` }, monday.toLocaleDateString([], { weekday: 'short' })));
  });
  cells.forEach((c, i) => box.append(h('i', { className: 'heat-cell', 'data-l': c.level, 'data-i': i, style: `grid-area: ${(i % 7) + 2} / ${Math.floor(i / 7) + 2}` })));

  const text = c => (c.words ? `${words(c.words)}${c.best_wpm ? ` · ${num(c.best_wpm)} wpm best` : ''}` : 'No words') + ` · ${dayLabel(c.date)}`;
  let active = cells.length - 1;
  const show = (i, announce) => {
    active = Math.max(0, Math.min(cells.length - 1, i));
    box.querySelector('.heat-cell.on')?.classList.remove('on');
    const cell = box.querySelector(`[data-i="${active}"]`), card = box.parentElement;
    cell.classList.add('on');
    tip.textContent = text(cells[active]);
    tip.hidden = false;
    const r = cell.getBoundingClientRect(), cr = card.getBoundingClientRect();
    tip.style.left = `${Math.min(Math.max(r.left + r.width / 2 - cr.left - tip.offsetWidth / 2, 6), cr.width - tip.offsetWidth - 6)}px`;
    tip.style.top = `${r.top - cr.top - tip.offsetHeight - 6}px`;
    if (announce) live.textContent = tip.textContent;
  };
  const hide = () => { tip.hidden = true; box.querySelector('.heat-cell.on')?.classList.remove('on'); };
  box.addEventListener('mouseover', e => { const c = e.target.closest('.heat-cell'); if (c) show(+c.dataset.i); });
  box.addEventListener('mouseleave', () => (document.activeElement === box ? show(active) : hide()));
  box.addEventListener('focus', () => show(active, true));
  box.addEventListener('blur', hide);
  box.addEventListener('keydown', e => {
    const step = { ArrowLeft: -7, ArrowRight: 7, ArrowUp: -1, ArrowDown: 1 }[e.key];
    if (step) show(active + step, true);
    else if (e.key === 'Home') show(0, true);
    else if (e.key === 'End') show(cells.length - 1, true);
    else return;
    e.preventDefault();
  });

  const legend = h('span', { className: 'legend', 'aria-hidden': 'true' }, 'Less', [0, 1, 2, 3, 4].map(l => h('i', { className: 'heat-cell', 'data-l': l })), 'More');
  return h('div', {},
    h('h2', { className: 'group-title' }, 'Last 12 months'),
    h('div', { className: 'group heat-card' },
      h('div', { className: 'heat-head' }, h('span', {}, `${words(sum)} in the last year`), legend),
      box, tip, live));
}

function facts(v) {
  const { h } = api;
  const fact = (label, value, sub) => h('div', {}, h('dt', {}, label), h('dd', {}, value, sub && h('span', {}, sub)));
  return h('dl', { className: 'facts' },
    v.best_day && fact('Most active day', dayLabel(v.best_day.date), words(v.best_day.value)),
    fact('Current streak', days(v.streak), v.streak ? (v.words_today ? 'Including today' : 'Through yesterday') : 'A day with any dictation starts one'),
    fact('Longest streak', days(v.longest_streak), 'Days in a row with a dictation'));
}

function milestones(v) {
  const { h } = api, got = v.milestones || {};
  const marks = MILESTONES.filter(([id]) => got[id]).map(([id, label]) => h('li', { className: 'mark' }, label, h('time', { dateTime: got[id] }, dayMonth(got[id]))));
  if (v.best_wpm && daysBetween(v.best_wpm.date, v.today) < 7) {
    marks.push(h('li', { className: 'mark' }, `New best speed, ${num(v.best_wpm.value)} wpm`, h('time', { dateTime: v.best_wpm.date }, dayMonth(v.best_wpm.date))));
  }
  const next = MILESTONES.find(([id, , at]) => at && !got[id]);
  return h('div', {},
    h('h2', { className: 'group-title' }, 'Milestones'),
    marks.length ? h('ul', { className: 'marks' }, marks) : null,
    next && h('p', { className: 'stats-note' }, `Next: ${next[1].replace('First ', '')}, ${num(next[2] - v.words_total)} to go.`));
}

// ---------- Share: the headline numbers as a PNG card, drawn here ----------
// Numbers only: no name, no dictated text, no app names. 1200×630 (the size link previews use), at 2×.
const W = 1200, H = 630, PAD = 72;
const FONT = '-apple-system, BlinkMacSystemFont, "SF Pro Display", system-ui, sans-serif';
let icon;
const loadIcon = () => (icon ??= new Promise(res => { const img = new Image(); img.onload = () => res(img); img.onerror = () => res(null); img.src = 'icon.svg'; }));

function drawCard(v, img) {
  const c = document.createElement('canvas');
  c.width = W * 2; c.height = H * 2;
  const g = c.getContext('2d');
  g.scale(2, 2);
  const bg = g.createLinearGradient(0, 0, 0, H); // the app icon's near-black
  bg.addColorStop(0, '#161618'); bg.addColorStop(1, '#050505');
  g.fillStyle = bg; g.fillRect(0, 0, W, H);
  const text = (t, x, y, size, color, weight = 500, align = 'left') => {
    g.font = `${weight} ${size}px ${FONT}`; g.fillStyle = color; g.textAlign = align;
    g.fillText(t, x, y);
    return g.measureText(t).width;
  };
  const GRAY = '#8e8e93', DIM = '#636366';
  if (img) g.drawImage(img, PAD - 8, 60, 72, 72); // its rounded square fills about 80% of the file
  text('Echotype', img ? PAD + 72 : PAD, 107, 30, '#fff', 600);
  text(`Week of ${dayMonth(v.week[0].date)}`, W - PAD, 107, 22, GRAY, 500, 'right');

  text('Words this week', PAD, 204, 24, GRAY);
  text(num(v.words_week), PAD - 4, 318, 120, '#fff', 600);
  // This week by day, as on the Stats page.
  const top = Math.max(...v.week.map(d => d.words), 1), bw = 40, gap = (460 - 7 * bw) / 6, x0 = W - PAD - 460;
  v.week.forEach((d, i) => {
    const x = x0 + i * (bw + gap), future = d.date > v.today, hgt = d.words ? Math.max(8, (d.words / top) * 130) : 4;
    g.fillStyle = future ? 'rgba(255,255,255,0.08)' : d.date === v.today ? '#fff' : 'rgba(255,255,255,0.55)';
    g.beginPath(); g.roundRect(x, 318 - (future ? 4 : hgt), bw, future ? 4 : hgt, 6); g.fill();
    text(date(d.date).toLocaleDateString([], { weekday: 'narrow' }), x + bw / 2, 352, 18, d.date === v.today ? '#fff' : DIM, 500, 'center');
  });

  g.fillStyle = 'rgba(255,255,255,0.12)'; g.fillRect(PAD, 392, W - 2 * PAD, 1);
  const col = (W - 2 * PAD) / 4;
  [['Total words', num(v.words_total), ''],
    ['Average speed', v.avg_wpm ? num(v.avg_wpm) : '–', v.avg_wpm ? 'wpm' : ''],
    ['Best speed', v.best_wpm ? num(v.best_wpm.value) : '–', v.best_wpm ? 'wpm' : ''],
    ['Streak', num(v.streak), v.streak === 1 ? 'day' : 'days'],
  ].forEach(([label, value, unit], i) => {
    const x = PAD + i * col;
    text(label, x, 446, 20, GRAY);
    const w = text(value, x, 506, 48, '#fff', 600);
    if (unit) text(unit, x + w + 8, 506, 22, GRAY);
  });
  text('Dictated with Echotype', PAD, H - 50, 18, DIM);
  return c;
}

// The card for the numbers on screen. A browser that taints a canvas drawing an SVG gets it without the icon.
async function card() {
  const c = drawCard(view, await loadIcon());
  try { c.getContext('2d').getImageData(0, 0, 1, 1); return c; } catch { return drawCard(view, null); }
}

const say = (btn, label) => { const was = btn.dataset.label ??= btn.textContent; btn.textContent = label; setTimeout(() => (btn.textContent = was), 1600); };

async function copyCard(btn) {
  // A promised blob, so the clipboard write starts inside the click as WebKit asks.
  const png = card().then(c => new Promise((res, rej) => c.toBlob(b => (b ? res(b) : rej(new Error('no image'))), 'image/png')));
  try { await navigator.clipboard.write([new ClipboardItem({ 'image/png': png })]); say(btn, 'Copied'); }
  catch (e) { btn.title = String(e?.message || e); say(btn, "Couldn't copy"); }
}

async function saveCard(btn) {
  try {
    const png = (await card()).toDataURL('image/png').split(',')[1];
    await api.invoke('save_stats_card', { png });
    say(btn, 'Saved to Downloads');
  } catch (e) { btn.title = String(e); say(btn, "Couldn't save"); }
}
