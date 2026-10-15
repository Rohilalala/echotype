//! Local usage stats: words and speaking speed per local day. Counts only, never text, and
//! nothing leaves this computer. One small JSON file of daily totals in the app data dir.

use crate::settings::{self, HistoryEntry};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Words per minute needs at least this much speech and this many words; shorter clips give silly numbers.
const WPM_MIN_MS: u64 = 3_000;
const WPM_MIN_WORDS: u64 = 5;
/// Days with activity kept one by one (2+ years for a daily user, ~70 KB). Older ones fold into `archive`.
const KEEP_DAYS: usize = 800;
/// Grid width when the page asks for nothing else: a year, GitHub style.
pub const DEFAULT_WEEKS: u32 = 53;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Dictation,
    File,
    Api,
}

/// One local day's totals.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Day {
    pub words: u64,
    /// Dictations, voice notes and API calls.
    pub count: u32,
    /// Words and speaking time of the dictations long enough to count for WPM.
    pub wpm_words: u64,
    pub wpm_ms: u64,
    /// 0 when no dictation that day qualified.
    pub best_wpm: f64,
}

/// Totals of the days folded out of `days`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Archive {
    pub words: u64,
    pub count: u64,
    pub wpm_words: u64,
    pub wpm_ms: u64,
    pub best_wpm: Option<DayValue>,
    pub best_day: Option<DayValue>,
    pub longest_streak: u32,
    /// The newest folded day and the run of days ending on it, so streaks continue across the fold.
    pub last_day: Option<NaiveDate>,
    pub tail_run: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct DayValue {
    pub date: NaiveDate,
    pub value: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Last {
    pub ts: u64,
    pub words: u64,
    pub wpm: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Store {
    /// Only days with activity.
    pub days: BTreeMap<NaiveDate, Day>,
    pub archive: Archive,
    pub last: Option<Last>,
    /// Milestone id → the day it was first reached.
    pub milestones: BTreeMap<String, NaiveDate>,
    /// History was imported once; never again, also after Clear stats.
    pub backfilled: bool,
}

pub fn words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

/// The words a result adds, or None when it adds nothing: stats are off, or there were no words.
pub fn to_count(s: &settings::Settings, text: &str) -> Option<u64> {
    Some(words(text)).filter(|w| s.usage_stats && *w > 0)
}

pub fn wpm(source: Source, words: u64, speaking_ms: u64) -> Option<f64> {
    (source == Source::Dictation && speaking_ms >= WPM_MIN_MS && words >= WPM_MIN_WORDS).then(|| words as f64 * 60_000.0 / speaking_ms as f64)
}

pub fn day_of<Tz: TimeZone>(ts_ms: u64, tz: &Tz) -> NaiveDate {
    DateTime::from_timestamp_millis(ts_ms as i64).unwrap_or_default().with_timezone(tz).date_naive()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

const WORD_MILESTONES: [(&str, u64); 3] = [("words_1000", 1_000), ("words_10000", 10_000), ("words_100000", 100_000)];
const STREAK_MILESTONES: [(&str, u32); 2] = [("streak_7", 7), ("streak_30", 30)];

/// `run` days in a row ending on `prev`; is `d` the next one?
fn step(prev: Option<NaiveDate>, run: u32, d: NaiveDate) -> u32 {
    if prev.and_then(|p| p.succ_opt()) == Some(d) { run + 1 } else { 1 }
}

impl Store {
    pub fn record(&mut self, day: NaiveDate, ts: u64, source: Source, words: u64, speaking_ms: u64) {
        let wpm = wpm(source, words, speaking_ms);
        let d = self.days.entry(day).or_default();
        d.words += words;
        d.count += 1;
        if let Some(w) = wpm {
            d.wpm_words += words;
            d.wpm_ms += speaking_ms;
            d.best_wpm = d.best_wpm.max(w);
        }
        if source == Source::Dictation {
            self.last = Some(Last { ts, words, wpm });
        }
        self.mark_milestones();
        self.fold();
    }

    /// Word counts of history entries, no WPM (history has no recording length).
    pub fn backfill<Tz: TimeZone>(&mut self, entries: &[HistoryEntry], tz: &Tz) {
        for e in entries.iter().filter(|e| e.ts > 0 && words(&e.text) > 0) {
            let d = self.days.entry(day_of(e.ts, tz)).or_default();
            d.words += words(&e.text);
            d.count += 1;
        }
        self.backfilled = true;
        self.mark_milestones();
        self.fold();
    }

    /// Stamps each milestone with the first day it was reached. Runs after every change, so a
    /// milestone is stamped long before its day could be folded into the archive.
    fn mark_milestones(&mut self) {
        let (mut total, mut run, mut prev) = (self.archive.words, self.archive.tail_run, self.archive.last_day);
        for (d, day) in &self.days {
            total += day.words;
            run = step(prev, run, *d);
            prev = Some(*d);
            let hits = WORD_MILESTONES.iter().filter(|m| total >= m.1).map(|m| m.0).chain(STREAK_MILESTONES.iter().filter(|m| run >= m.1).map(|m| m.0));
            for id in hits {
                self.milestones.entry(id.into()).or_insert(*d);
            }
        }
    }

    /// Keeps `days` at KEEP_DAYS by folding the oldest into `archive`.
    fn fold(&mut self) {
        while self.days.len() > KEEP_DAYS {
            let Some((date, d)) = self.days.pop_first() else { break };
            let a = &mut self.archive;
            // A day older than the archive's newest (the clock was set back) only adds to the totals.
            if a.last_day.is_none_or(|l| date > l) {
                a.tail_run = step(a.last_day, a.tail_run, date);
                a.last_day = Some(date);
                a.longest_streak = a.longest_streak.max(a.tail_run);
            }
            a.words += d.words;
            a.count += d.count as u64;
            a.wpm_words += d.wpm_words;
            a.wpm_ms += d.wpm_ms;
            if d.best_wpm > 0.0 && a.best_wpm.is_none_or(|b| d.best_wpm > b.value) {
                a.best_wpm = Some(DayValue { date, value: d.best_wpm });
            }
            if a.best_day.is_none_or(|b| d.words as f64 > b.value) {
                a.best_day = Some(DayValue { date, value: d.words as f64 });
            }
        }
    }

    pub fn view(&self, today: NaiveDate, weeks: u32) -> View {
        let week_start = today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
        let cell = |date: NaiveDate, level: u8| {
            let d = self.days.get(&date);
            Cell { date, words: d.map_or(0, |d| d.words), best_wpm: d.map(|d| d.best_wpm).filter(|w| *w > 0.0), level }
        };
        let rate = |words: u64, ms: u64| (ms > 0).then(|| words as f64 * 60_000.0 / ms as f64);
        let sum = |from: NaiveDate| self.days.range(from..=today).fold((0, 0, 0), |(w, ww, ms), (_, d)| (w + d.words, ww + d.wpm_words, ms + d.wpm_ms));
        let (words_week, wpm_words_week, wpm_ms_week) = sum(week_start);

        let grid_start = week_start - chrono::Days::new(7 * (weeks.clamp(1, 110) as u64 - 1));
        let in_grid: Vec<u64> = self.days.range(grid_start..=today).map(|(_, d)| d.words).filter(|w| *w > 0).collect();
        let grid = grid_start.iter_days().take_while(|d| *d <= today).map(|d| cell(d, level(self.days.get(&d).map_or(0, |x| x.words), &in_grid))).collect();

        let best_wpm = self
            .days
            .iter()
            .filter(|(_, d)| d.best_wpm > 0.0)
            .map(|(date, d)| DayValue { date: *date, value: d.best_wpm })
            .chain(self.archive.best_wpm)
            .max_by(|a, b| a.value.total_cmp(&b.value));
        // Ties go to the most recent day.
        let best_day = self.days.iter().map(|(date, d)| DayValue { date: *date, value: d.words as f64 }).chain(self.archive.best_day).max_by(|a, b| a.value.total_cmp(&b.value).then(a.date.cmp(&b.date)));
        let (wpm_words, wpm_ms) = self.days.values().fold((self.archive.wpm_words, self.archive.wpm_ms), |(w, ms), d| (w + d.wpm_words, ms + d.wpm_ms));
        View {
            today,
            words_today: self.days.get(&today).map_or(0, |d| d.words),
            words_week,
            words_total: self.archive.words + self.days.values().map(|d| d.words).sum::<u64>(),
            count_total: self.archive.count + self.days.values().map(|d| d.count as u64).sum::<u64>(),
            last: self.last.clone(),
            avg_wpm: rate(wpm_words, wpm_ms),
            avg_wpm_week: rate(wpm_words_week, wpm_ms_week),
            best_wpm,
            best_day,
            streak: self.current_streak(today),
            longest_streak: self.longest_streak(),
            week: week_start.iter_days().take(7).map(|d| cell(d, 0)).collect(),
            grid,
            milestones: self.milestones.clone(),
        }
    }
}

impl Store {
    /// Consecutive days with activity ending today, or ending yesterday while today has none yet.
    fn current_streak(&self, today: NaiveDate) -> u32 {
        let mut d = if self.days.contains_key(&today) { Some(today) } else { today.pred_opt() };
        let mut n = 0;
        while let Some(x) = d.filter(|x| self.days.contains_key(x)) {
            n += 1;
            d = x.pred_opt();
        }
        if n > 0 && d.is_some() && d == self.archive.last_day {
            n += self.archive.tail_run;
        }
        n
    }

    fn longest_streak(&self) -> u32 {
        let (mut best, mut run, mut prev) = (self.archive.longest_streak, self.archive.tail_run, self.archive.last_day);
        for d in self.days.keys() {
            run = step(prev, run, *d);
            best = best.max(run);
            prev = Some(*d);
        }
        best
    }
}

/// Grid shade 0-4: 0 is no words, then the quartile of the day among the grid's active days,
/// so the darkest cells are always the user's own busiest quarter.
fn level(words: u64, active: &[u64]) -> u8 {
    if words == 0 || active.is_empty() {
        return 0;
    }
    let at_or_below = active.iter().filter(|w| **w <= words).count();
    (at_or_below * 4).div_ceil(active.len()).clamp(1, 4) as u8
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Cell {
    pub date: NaiveDate,
    pub words: u64,
    pub best_wpm: Option<f64>,
    pub level: u8,
}

/// What get_stats returns: everything the Stats page and the Home strip show.
#[derive(Serialize, Clone, Debug)]
pub struct View {
    pub today: NaiveDate,
    pub words_today: u64,
    pub words_week: u64,
    pub words_total: u64,
    pub count_total: u64,
    pub last: Option<Last>,
    pub avg_wpm: Option<f64>,
    pub avg_wpm_week: Option<f64>,
    pub best_wpm: Option<DayValue>,
    pub best_day: Option<DayValue>,
    pub streak: u32,
    pub longest_streak: u32,
    /// Monday to Sunday of this week.
    pub week: Vec<Cell>,
    /// From a Monday `weeks` weeks back through today, a column per week.
    pub grid: Vec<Cell>,
    pub milestones: BTreeMap<String, NaiveDate>,
}

// ---------- the file ----------

// ponytail: one process-wide lock, like history; only the app process writes stats.
static LOCK: Mutex<()> = Mutex::new(());

fn path() -> PathBuf {
    settings::data_dir().join("stats.json")
}

/// A missing file is a fresh store that still imports history. A damaged one is moved aside to
/// stats.json.bak, never written over, and counting starts again without importing history (it
/// may have been cleared on purpose). Err only when it can't be read or moved: then nothing is written.
fn load(path: &Path) -> Result<Store, String> {
    let bad = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Ok(s) => match serde_json::from_str(&s) {
            Ok(store) => return Ok(store),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    };
    std::fs::rename(path, path.with_extension("json.bak")).map_err(|e| format!("stats.json is unreadable ({bad}) and could not be moved aside: {e}"))?;
    eprintln!("echotype: stats.json was unreadable ({bad}); kept it as stats.json.bak");
    let fresh = Store { backfilled: true, ..Default::default() };
    save(path, &fresh)?;
    Ok(fresh)
}

fn save(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    settings::write_atomic(path, &serde_json::to_string(store).map_err(|e| e.to_string())?)
}

fn update(path: &Path, history: impl FnOnce() -> Vec<HistoryEntry>, f: impl FnOnce(&mut Store)) -> Result<(), String> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load(path)?;
    if !store.backfilled {
        store.backfill(&history(), &Local);
    }
    f(&mut store);
    save(path, &store)
}

fn all_history() -> Vec<HistoryEntry> {
    settings::get_history(usize::MAX)
}

/// Counts one result. The caller checks the usage_stats setting, and calls this before the
/// result goes into history so the one-time backfill can't count it twice.
pub fn record(source: Source, words: u64, speaking_ms: u64) -> Result<(), String> {
    let ts = now_ms();
    update(&path(), all_history, |s| s.record(day_of(ts, &Local), ts, source, words, speaking_ms))
}

/// Imports history the first time stats run. True when it did.
pub fn backfill_once() -> Result<bool, String> {
    let p = path();
    if load(&p)?.backfilled {
        return Ok(false);
    }
    update(&p, all_history, |_| {}).map(|()| true)
}

pub fn view(weeks: u32) -> View {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    load(&path()).unwrap_or_default().view(day_of(now_ms(), &Local), weeks)
}

/// Starts over from zero, without importing history again.
pub fn clear() -> Result<(), String> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save(&path(), &Store { backfilled: true, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDateTime};

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    /// Unix ms of a wall-clock time in a fixed offset (seconds east of UTC).
    fn at(wall: &str, east: i32) -> u64 {
        let t = NaiveDateTime::parse_from_str(wall, "%Y-%m-%d %H:%M").unwrap();
        FixedOffset::east_opt(east).unwrap().from_local_datetime(&t).unwrap().timestamp_millis() as u64
    }

    fn rec(s: &mut Store, day: &str, words: u64, ms: u64) {
        s.record(d(day), 0, Source::Dictation, words, ms);
    }

    #[test]
    fn wpm_needs_three_seconds_and_five_words_and_only_counts_dictation() {
        assert_eq!(wpm(Source::Dictation, 50, 20_000), Some(150.0));
        assert_eq!(wpm(Source::Dictation, 5, 3_000), Some(100.0));
        assert_eq!(wpm(Source::Dictation, 4, 10_000), None, "under 5 words");
        assert_eq!(wpm(Source::Dictation, 30, 2_999), None, "under 3 s");
        assert_eq!(wpm(Source::File, 300, 60_000), None);
        assert_eq!(wpm(Source::Api, 300, 60_000), None);
        assert_eq!(words("  Hello,\nworld  it's  me "), 4);
    }

    #[test]
    fn voice_notes_count_words_but_not_speed() {
        let mut s = Store::default();
        s.record(d("2026-10-14"), 1, Source::Dictation, 40, 20_000);
        s.record(d("2026-10-14"), 2, Source::File, 500, 0);
        s.record(d("2026-10-14"), 3, Source::Api, 60, 0);
        let v = s.view(d("2026-10-14"), 53);
        assert_eq!((v.words_today, v.count_total), (600, 3));
        assert_eq!(v.avg_wpm, Some(120.0));
        assert_eq!(v.last, Some(Last { ts: 1, words: 40, wpm: Some(120.0) }), "files and API calls are not 'your last dictation'");
        // A short dictation becomes the last one, without a speed.
        s.record(d("2026-10-14"), 4, Source::Dictation, 3, 1_000);
        assert_eq!(s.view(d("2026-10-14"), 53).last.unwrap().wpm, None);
    }

    #[test]
    fn days_follow_the_local_clock_across_midnight_and_time_zones() {
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let utc = FixedOffset::east_opt(0).unwrap();
        let pst = FixedOffset::west_opt(8 * 3600).unwrap();
        let before = at("2026-10-14 23:59", 19800);
        let after = at("2026-10-15 00:01", 19800);
        assert_eq!(day_of(before, &ist), d("2026-10-14"));
        assert_eq!(day_of(after, &ist), d("2026-10-15"));
        // The same instant is still the 14th in UTC and California.
        assert_eq!(day_of(after, &utc), d("2026-10-14"));
        assert_eq!(day_of(after, &pst), d("2026-10-14"));
        let mut s = Store::default();
        for ts in [before, after] {
            s.record(day_of(ts, &ist), ts, Source::Dictation, 10, 5_000);
        }
        assert_eq!(s.days.keys().copied().collect::<Vec<_>>(), [d("2026-10-14"), d("2026-10-15")]);
    }

    #[test]
    fn this_week_runs_monday_to_sunday() {
        let mut s = Store::default();
        rec(&mut s, "2026-10-11", 100, 0); // Sunday: last week
        rec(&mut s, "2026-10-12", 20, 0); // Monday
        rec(&mut s, "2026-10-18", 3, 0); // Sunday
        let sun = s.view(d("2026-10-18"), 53);
        assert_eq!(sun.words_week, 23);
        assert_eq!(sun.week.first().unwrap().date, d("2026-10-12"));
        assert_eq!(sun.week.last().unwrap().date, d("2026-10-18"));
        assert_eq!(sun.week.iter().map(|c| c.words).collect::<Vec<_>>(), [20, 0, 0, 0, 0, 0, 3]);
        // Monday the 12th: the 11th belongs to the week before.
        assert_eq!(s.view(d("2026-10-12"), 53).words_week, 20);
        // The week's average speed uses this week's dictations only.
        let mut s = Store::default();
        rec(&mut s, "2026-10-11", 60, 60_000); // 60 wpm last week
        rec(&mut s, "2026-10-13", 120, 60_000); // 120 wpm this week
        let v = s.view(d("2026-10-14"), 53);
        assert_eq!((v.avg_wpm_week, v.avg_wpm), (Some(120.0), Some(90.0)));
    }

    #[test]
    fn streaks_count_consecutive_days_and_today_does_not_break_them_yet() {
        let mut s = Store::default();
        for day in ["2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04", "2026-10-10", "2026-10-11", "2026-10-12", "2026-10-13"] {
            rec(&mut s, day, 10, 0);
        }
        let v = s.view(d("2026-10-14"), 53);
        assert_eq!((v.streak, v.longest_streak), (4, 4), "today not dictated yet keeps yesterday's streak");
        assert_eq!(s.view(d("2026-10-13"), 53).streak, 4);
        assert_eq!(s.view(d("2026-10-15"), 53).streak, 0, "a missed day ends it");
        rec(&mut s, "2026-10-14", 10, 0);
        let v = s.view(d("2026-10-14"), 53);
        assert_eq!((v.streak, v.longest_streak), (5, 5));
        assert_eq!(Store::default().view(d("2026-10-14"), 53).streak, 0);
        // Across a month end.
        let mut s = Store::default();
        for day in ["2026-09-29", "2026-09-30", "2026-10-01"] {
            rec(&mut s, day, 1, 0);
        }
        assert_eq!(s.view(d("2026-10-01"), 53).streak, 3);
    }

    #[test]
    fn best_wpm_per_day_and_overall() {
        let mut s = Store::default();
        rec(&mut s, "2026-10-13", 50, 30_000); // 100
        rec(&mut s, "2026-10-13", 40, 15_000); // 160
        rec(&mut s, "2026-10-13", 6, 1_000); // too short to count
        rec(&mut s, "2026-10-14", 70, 30_000); // 140
        let v = s.view(d("2026-10-14"), 53);
        let cell = |date| v.grid.iter().find(|c| c.date == d(date)).unwrap();
        assert_eq!(cell("2026-10-13").best_wpm, Some(160.0));
        assert_eq!(cell("2026-10-14").best_wpm, Some(140.0));
        assert_eq!(cell("2026-10-12").best_wpm, None);
        assert_eq!(v.best_wpm, Some(DayValue { date: d("2026-10-13"), value: 160.0 }));
        assert_eq!(v.best_day, Some(DayValue { date: d("2026-10-13"), value: 96.0 }));
        // Average is words over minutes across qualifying dictations: 160 words in 75 s.
        assert_eq!(v.avg_wpm, Some(160.0 * 60_000.0 / 75_000.0));
    }

    #[test]
    fn grid_levels_are_quartiles_of_the_active_days() {
        let active = [10, 20, 30, 40, 50, 60, 70, 80];
        assert_eq!(level(0, &active), 0);
        assert_eq!([10, 20, 30, 40, 50, 60, 70, 80].map(|w| level(w, &active)), [1, 1, 2, 2, 3, 3, 4, 4]);
        assert_eq!(level(5, &active), 1, "below every active day is still level 1");
        assert_eq!(level(900, &[900]), 4, "one active day is the busiest one");
        assert_eq!(level(7, &[7, 7, 7]), 4, "equal days share the top shade");
        let mut s = Store::default();
        rec(&mut s, "2026-10-13", 10, 0);
        rec(&mut s, "2026-10-14", 1000, 0);
        let v = s.view(d("2026-10-14"), 53);
        assert_eq!(v.grid.len(), 52 * 7 + 3, "53 columns ending on Wednesday");
        assert_eq!(v.grid[0].date.weekday(), chrono::Weekday::Mon);
        assert_eq!(v.grid.last().unwrap().date, d("2026-10-14"));
        let levels: Vec<u8> = v.grid.iter().rev().take(3).map(|c| c.level).collect();
        assert_eq!(levels, [4, 2, 0]);
    }

    #[test]
    fn milestones_are_stamped_on_the_day_they_were_reached() {
        let mut s = Store::default();
        rec(&mut s, "2026-10-01", 600, 0);
        rec(&mut s, "2026-10-02", 600, 0);
        for i in 3..=8 {
            rec(&mut s, &format!("2026-10-0{i}"), 1, 0);
        }
        assert_eq!(s.milestones.get("words_1000"), Some(&d("2026-10-02")));
        assert_eq!(s.milestones.get("streak_7"), Some(&d("2026-10-07")));
        assert!(!s.milestones.contains_key("words_10000"));
        rec(&mut s, "2026-10-20", 9_000, 0);
        assert_eq!(s.milestones.get("words_10000"), Some(&d("2026-10-20")));
        assert_eq!(s.milestones.get("streak_7"), Some(&d("2026-10-07")), "kept once reached");
    }

    fn entry(ts: u64, text: &str) -> HistoryEntry {
        HistoryEntry { ts, raw: "secret raw words".into(), text: text.into(), stt_ms: 1, cleanup_ms: 0, stt_provider: "local".into(), cleanup_provider: None, no_cleanup: false, cleaned: None }
    }

    #[test]
    fn backfill_counts_history_words_by_day_without_speed() {
        let ist = FixedOffset::east_opt(19800).unwrap();
        let mut s = Store::default();
        s.backfill(&[entry(at("2026-10-13 09:00", 19800), "one two three"), entry(at("2026-10-14 00:30", 19800), "four five"), entry(0, "no time"), entry(at("2026-10-12 09:00", 19800), "  ")], &ist);
        assert!(s.backfilled);
        assert_eq!(s.days.get(&d("2026-10-13")).map(|x| (x.words, x.count)), Some((3, 1)));
        assert_eq!(s.days.get(&d("2026-10-14")).map(|x| (x.words, x.wpm_ms)), Some((2, 0)));
        assert_eq!(s.days.len(), 2, "entries without a timestamp or words are skipped");
        assert_eq!(s.last, None);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("echotype-stats-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("stats.json")
    }

    #[test]
    fn the_file_never_holds_text_and_backfills_once() {
        let p = scratch("file");
        let history = || vec![entry(1_760_000_000_000, "private words here")];
        update(&p, history, |s| s.record(d("2026-10-14"), 5, Source::Dictation, 12, 6_000)).unwrap();
        update(&p, || panic!("history is read once"), |s| s.record(d("2026-10-14"), 6, Source::Dictation, 1, 0)).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("private") && !raw.contains("secret"), "{raw}");
        let s = load(&p).unwrap();
        assert!(s.backfilled);
        assert_eq!(s.view(d("2026-10-14"), 1).words_total, 3 + 12 + 1);
    }

    #[test]
    fn clearing_starts_from_zero_and_does_not_import_history_again() {
        let p = scratch("clear");
        update(&p, || vec![entry(1_760_000_000_000, "a b c")], |s| s.record(d("2026-10-14"), 5, Source::Dictation, 12, 6_000)).unwrap();
        save(&p, &Store { backfilled: true, ..Default::default() }).unwrap(); // what clear() writes
        update(&p, || panic!("no second import"), |s| s.record(d("2026-10-15"), 7, Source::Dictation, 2, 0)).unwrap();
        let v = load(&p).unwrap().view(d("2026-10-15"), 53);
        assert_eq!((v.words_total, v.count_total, v.longest_streak), (2, 1, 1));
        assert!(v.milestones.is_empty());
    }

    #[test]
    fn the_off_switch_stops_recording() {
        let mut s = settings::Settings::default();
        assert!(s.usage_stats, "on for new installs");
        let old: settings::Settings = serde_json::from_str(r#"{"hotkey":"Alt+Space","history_enabled":false}"#).unwrap();
        assert!(old.usage_stats, "upgraders keep counting");
        assert_eq!(to_count(&s, "three little words"), Some(3));
        assert_eq!(to_count(&s, "  \n "), None, "nothing said, nothing counted");
        s.usage_stats = false;
        assert_eq!(to_count(&s, "three little words"), None);
    }

    #[test]
    fn a_damaged_file_is_kept_aside_not_written_over() {
        let p = scratch("damaged");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{ not json").unwrap();
        update(&p, || panic!("a damaged file must not re-import history"), |s| s.record(d("2026-10-14"), 1, Source::Dictation, 4, 0)).unwrap();
        assert_eq!(std::fs::read_to_string(p.with_extension("json.bak")).unwrap(), "{ not json");
        assert_eq!(load(&p).unwrap().view(d("2026-10-14"), 1).words_total, 4);
    }

    #[test]
    fn a_day_older_than_the_archive_only_adds_to_the_totals() {
        let mut s = Store::default();
        for day in d("2024-01-01").iter_days().take(KEEP_DAYS + 5) {
            s.record(day, 0, Source::Dictation, 1, 0);
        }
        let (last, run) = (s.archive.last_day, s.archive.tail_run);
        s.record(d("2023-06-01"), 0, Source::Dictation, 7, 0); // clock set back
        assert_eq!((s.archive.last_day, s.archive.tail_run), (last, run));
        assert_eq!(s.view(d("2024-01-01") + chrono::Days::new(KEEP_DAYS as u64 + 4), 53).words_total, KEEP_DAYS as u64 + 5 + 7);
    }

    #[test]
    fn years_of_use_stay_small() {
        let mut s = Store::default();
        let start = d("2020-01-01");
        for (i, day) in start.iter_days().take(3_000).enumerate() {
            s.record(day, i as u64, Source::Dictation, 200 + (i as u64 % 50), 60_000);
        }
        assert_eq!(s.days.len(), KEEP_DAYS);
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.len() < 120_000, "{} bytes", json.len());
        let v = s.view(start + chrono::Days::new(2_999), 53);
        assert_eq!(v.words_total, (0..3_000u64).map(|i| 200 + i % 50).sum::<u64>(), "folded days still count");
        assert_eq!(v.count_total, 3_000);
        assert_eq!((v.streak, v.longest_streak), (3_000, 3_000), "streaks carry across the fold");
        assert_eq!(v.best_day.unwrap().value, 249.0);
    }
}
