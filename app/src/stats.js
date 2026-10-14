// The Stats page and the Home strip, drawn from get_stats (stats.rs). Counts only, kept on this computer.
const $ = (s, el = document) => el.querySelector(s);
let api, view = null, err = null, loaded = false;

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
  api.listen('settings-changed', render);
  document.addEventListener('echotype:page', e => e.detail === 'stats' && refresh());
  $('[data-bind="usage_stats"]').addEventListener('change', render);
  $('[data-bind="display_name"]').addEventListener('input', renderTitle);
  $('#statsOn').addEventListener('click', () => {
    const sw = $('[data-bind="usage_stats"]');
    sw.checked = true;
    sw.dispatchEvent(new Event('change')); // the bound switch saves it
  });
  const confirm = on => { $('#clearStats').hidden = on; $('#clearStatsConfirm').hidden = !on; (on ? $('#clearStatsCancel') : $('#clearStats')).focus(); };
  $('#clearStats').addEventListener('click', () => confirm(true));
  $('#clearStatsCancel').addEventListener('click', () => confirm(false));
  $('#clearStatsYes').addEventListener('click', async () => {
    try { await api.invoke('clear_stats'); } catch (e) { err = `Couldn't clear stats: ${e}`; }
    confirm(false);
    refresh();
  });
  refresh();
}

async function refresh() {
  try { view = await api.invoke('get_stats', { range: 53 }); err = null; }
  catch (e) { err = `Couldn't read stats: ${e}`; }
  loaded = true;
  render();
}

function render() {
  renderTitle();
  renderStrip();
  renderPage();
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
  const body = $('#statsBody');
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
