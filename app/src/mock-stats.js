// Browser preview only: get_stats / clear_stats over a seeded sample year, the same shape stats.rs returns.
// ?stats=new starts empty (a new user); ?statsoff turns counting off; ?name=Aditya sets the display name.
const iso = d => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
const addDays = (d, n) => { const x = new Date(d); x.setDate(x.getDate() + n); return x; };

function sampleDays() {
  let seed = 7;
  const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
  const days = new Map(), today = new Date();
  for (let i = 300; i >= 0; i--) {
    const d = addDays(today, -i), weekend = d.getDay() === 0 || d.getDay() === 6;
    if (i > 6 && rand() < (weekend ? 0.7 : 0.22)) continue; // the last week is a streak
    const words = Math.round((weekend ? 150 : 500) + rand() * (weekend ? 600 : 2600));
    const wpm_ms = Math.round(words * 0.7 * 60000 / (118 + rand() * 30));
    days.set(iso(d), { words, count: 4 + Math.round(words / 90), wpm_words: Math.round(words * 0.7), wpm_ms, best_wpm: 140 + rand() * 38 });
  }
  return days;
}

export function statsCommands(q) {
  let days = q.get('stats') === 'new' ? new Map() : sampleDays();
  let last = days.size ? { ts: Date.now() - 4 * 60000, words: 38, wpm: 142 } : null;
  const view = (weeks = 53) => {
    const now = new Date(), today = iso(now);
    const monday = addDays(now, -((now.getDay() + 6) % 7));
    const cell = (d, level = 0) => { const x = days.get(iso(d)); return { date: iso(d), words: x?.words ?? 0, best_wpm: x?.best_wpm ?? null, level }; };
    const span = from => [...days].filter(([k]) => k >= iso(from) && k <= today).map(([, v]) => v);
    const add = (list, f) => list.reduce((a, v) => a + v[f], 0);
    const rate = list => (add(list, 'wpm_ms') ? add(list, 'wpm_words') * 60000 / add(list, 'wpm_ms') : null);
    const start = addDays(monday, -7 * (weeks - 1)), all = [...days.values()];
    const active = span(start).map(v => v.words).filter(Boolean);
    const level = w => (!w ? 0 : Math.min(4, Math.max(1, Math.ceil(4 * active.filter(a => a <= w).length / active.length))));
    const grid = [];
    for (let d = start; iso(d) <= today; d = addDays(d, 1)) grid.push(cell(d, level(days.get(iso(d))?.words ?? 0)));
    const keys = [...days.keys()].sort();
    let streak = 0, longest = 0, run = 0, prev = null, total = 0;
    const milestones = {};
    for (const k of keys) {
      run = prev && iso(addDays(new Date(prev + 'T12:00'), 1)) === k ? run + 1 : 1;
      longest = Math.max(longest, run); prev = k; total += days.get(k).words;
      for (const [id, hit] of [['words_1000', total >= 1000], ['words_10000', total >= 10000], ['words_100000', total >= 100000], ['streak_7', run >= 7], ['streak_30', run >= 30]]) if (hit) milestones[id] ??= k;
    }
    for (let d = days.has(today) ? now : addDays(now, -1); days.has(iso(d)); d = addDays(d, -1)) streak++;
    const best = (f, val) => [...days].reduce((b, [k, v]) => (val(v) > (b?.value ?? 0) ? { date: k, value: val(v) } : b), null);
    return {
      today, words_today: days.get(today)?.words ?? 0, words_week: add(span(monday), 'words'), words_total: add(all, 'words'), count_total: add(all, 'count'),
      last, avg_wpm: rate(all), avg_wpm_week: rate(span(monday)), best_wpm: best('wpm', v => v.best_wpm), best_day: best('words', v => v.words),
      streak, longest_streak: longest, week: Array.from({ length: 7 }, (_, i) => cell(addDays(monday, i))), grid, milestones,
    };
  };
  return {
    get_stats: ({ range }) => view(range),
    clear_stats: () => { days = new Map(); last = null; return null; },
  };
}
