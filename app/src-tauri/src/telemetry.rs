//! Opt-in anonymous usage stats and crash reports for the beta (Settings → Privacy, off by
//! default). Counts, ids and versions only: never audio, text, dictionary words, app names,
//! file paths, names or emails. While the setting is off nothing is counted, kept or sent.

use crate::settings::{self, Settings};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Unsent days kept while the server can't be reached; older ones are dropped.
const KEEP_DAYS: usize = 5;
/// Crash reports kept on disk at most, so a crash loop can't pile them up.
const MAX_CRASHES: usize = 5;
/// Timing samples kept per day for the medians.
const MAX_SAMPLES: usize = 1_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    NoSpeech,
    CleanupRejected,
    PasteFellBack,
    ModelLoad,
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Models {
    pub speech_provider: String,
    pub speech_model: String,
    pub cleanup_provider: String,
    pub cleanup_model: String,
}

/// One local day, kept on this Mac until it is sent.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
struct Counts {
    dictations: u32,
    words: u64,
    failures: BTreeMap<Failure, u32>,
    speech_ms: Vec<u64>,
    cleanup_ms: Vec<u64>,
    models: Models,
    meetings_started: u32,
    meetings_finished: u32,
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default)]
struct Store {
    install_id: String,
    days: BTreeMap<NaiveDate, Counts>,
    /// The day a batch was last tried, so a server that is down is tried once a day, not hourly.
    last_try: Option<NaiveDate>,
}

/// Exactly what POST /v1/telemetry receives.
#[derive(Serialize, Debug)]
pub struct Batch {
    pub install_id: String,
    pub version: String,
    pub os: &'static str,
    pub arch: &'static str,
    pub days: Vec<Day>,
    pub crashes: Vec<Crash>,
}

#[derive(Serialize, Debug)]
pub struct Day {
    pub date: NaiveDate,
    pub dictations: u32,
    pub words: u64,
    pub failures: BTreeMap<Failure, u32>,
    pub speech_ms_median: Option<u64>,
    pub cleanup_ms_median: Option<u64>,
    #[serde(flatten)]
    pub models: Models,
    pub meetings_started: u32,
    pub meetings_finished: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Crash {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub message: String,
    pub backtrace: String,
}

/// Provider and model ids in use. A local model file or a custom server's model name could say
/// anything about the user, so those go as "file" and "custom".
pub fn models(s: &Settings) -> Models {
    let provider = |p: &str| if p == "local" || settings::preset(p).is_some() { p.to_string() } else { "custom".into() };
    let named = |p: &str, m: &str| {
        if provider(p) == "custom" || m.starts_with(['/', '~', '.']) || m.contains('\\') { "custom".into() } else { m.chars().take(64).collect() }
    };
    let speech_model = if s.stt.provider == "local" {
        let id = crate::stt::local_model(s);
        if crate::stt::MODELS.iter().any(|m| m.id == id) { id } else { "file".into() }
    } else {
        named(&s.stt.provider, &s.stt.model)
    };
    let (cleanup_provider, cleanup_model) = match s.cleanup.provider.as_str() {
        _ if !s.cleanup.enabled => ("off".into(), String::new()),
        "echotype" => ("echotype".into(), crate::llm::cleaner(s).id.into()),
        p => (provider(p), named(p, &s.cleanup.model)),
    };
    Models { speech_provider: provider(&s.stt.provider), speech_model, cleanup_provider, cleanup_model }
}

// ---------- counting ----------

// ponytail: one process-wide lock, like stats; only the app process writes this file.
static LOCK: Mutex<()> = Mutex::new(());

fn file(dir: &Path) -> PathBuf {
    dir.join("telemetry.json")
}

fn crash_dir(dir: &Path) -> PathBuf {
    dir.join("crashes")
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn load(p: &Path) -> Store {
    std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(p: &Path, store: &mut Store) {
    while store.days.len() > KEEP_DAYS {
        store.days.pop_first();
    }
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(store) {
        let _ = settings::write_atomic(p, &json);
    }
}

fn add(dir: &Path, s: &Settings, day: NaiveDate, f: impl FnOnce(&mut Counts)) {
    if !s.share_telemetry {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = file(dir);
    let mut store = load(&p);
    f(store.days.entry(day).or_default());
    save(&p, &mut store);
}

fn sample(v: &mut Vec<u64>, ms: u64) {
    if v.len() < MAX_SAMPLES {
        v.push(ms);
    }
}

/// A finished dictation: its word count and how long speech and cleanup took.
pub fn dictation(s: &Settings, words: u64, speech_ms: u64, cleanup_ms: u64) {
    let models = models(s);
    add(&settings::data_dir(), s, today(), |c| {
        c.dictations += 1;
        c.words += words;
        sample(&mut c.speech_ms, speech_ms);
        if cleanup_ms > 0 {
            sample(&mut c.cleanup_ms, cleanup_ms);
        }
        c.models = models;
    });
}

pub fn failure(s: &Settings, kind: Failure) {
    add(&settings::data_dir(), s, today(), |c| *c.failures.entry(kind).or_default() += 1);
}

pub fn meeting(s: &Settings, finished: bool) {
    add(&settings::data_dir(), s, today(), |c| if finished { c.meetings_finished += 1 } else { c.meetings_started += 1 });
}

fn median(v: &[u64]) -> Option<u64> {
    let mut v = v.to_vec();
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

fn report(date: NaiveDate, c: &Counts) -> Day {
    Day {
        date,
        dictations: c.dictations,
        words: c.words,
        failures: c.failures.clone(),
        speech_ms_median: median(&c.speech_ms),
        cleanup_ms_median: median(&c.cleanup_ms),
        models: c.models.clone(),
        meetings_started: c.meetings_started,
        meetings_finished: c.meetings_finished,
    }
}

// ---------- sending ----------

fn new_id() -> String {
    let mut b = [0u8; 16];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn read_crashes(dir: &Path) -> Vec<(PathBuf, Crash)> {
    let Ok(rd) = std::fs::read_dir(crash_dir(dir)) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            match std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()) {
                Some(c) => Some((p, c)),
                None => {
                    let _ = std::fs::remove_file(&p); // unreadable: it can never be sent
                    None
                }
            }
        })
        .collect()
}

/// One pass of the sender. Off: crash reports and unsent counts (with the install id) are
/// deleted and nothing is sent. On: finished days at most once a day, crash reports at launch;
/// both are deleted only once the server has taken them.
fn tick(dir: &Path, on: bool, launch: bool, version: &str, today: NaiveDate, post: impl FnOnce(&Batch) -> bool) {
    let p = file(dir);
    if !on {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::fs::remove_dir_all(crash_dir(dir));
        let _ = std::fs::remove_file(&p);
        return;
    }
    let crashes = if launch { read_crashes(dir) } else { vec![] };
    let batch = {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut store = load(&p);
        if store.install_id.is_empty() {
            store.install_id = new_id();
        }
        let days: Vec<Day> = if store.last_try == Some(today) { vec![] } else { store.days.range(..today).map(|(d, c)| report(*d, c)).collect() };
        if !days.is_empty() {
            store.last_try = Some(today);
        }
        let batch = Batch { install_id: store.install_id.clone(), version: version.into(), os: std::env::consts::OS, arch: std::env::consts::ARCH, days, crashes: crashes.iter().map(|(_, c)| c.clone()).collect() };
        save(&p, &mut store);
        batch
    };
    if batch.days.is_empty() && batch.crashes.is_empty() {
        return;
    }
    // Not under the lock: a slow or absent server must not hold up counting a dictation.
    if !post(&batch) {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = load(&p);
    for d in &batch.days {
        store.days.remove(&d.date);
    }
    save(&p, &mut store);
    for (f, _) in crashes {
        let _ = std::fs::remove_file(f);
    }
}

/// Fails quietly: the server may not be up yet, and the batch is kept for the next day.
fn post(batch: &Batch) -> bool {
    let url = format!("{}/telemetry", settings::cloud_url());
    reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build().and_then(|c| c.post(url).json(batch).send()).is_ok_and(|r| r.status().is_success())
}

/// The sender: a minute after launch (crash reports and yesterday's counts), then hourly checks
/// that send at most one batch a day.
pub fn start(version: String, current: impl Fn() -> Settings + Send + 'static) {
    std::thread::spawn(move || {
        let dir = settings::data_dir();
        let mut launch = true;
        loop {
            std::thread::sleep(Duration::from_secs(if launch { 60 } else { 3_600 }));
            tick(&dir, current().share_telemetry, launch, &version, today(), post);
            launch = false;
        }
    });
}

// ---------- crash reports ----------

static VERSION: OnceLock<String> = OnceLock::new();

/// The fixed start of a panic message, before anything format! could have put in it (an Err's
/// text, a transcript, a number): up to the first ':', quote, backtick or digit.
fn prefix(msg: &str) -> &str {
    let end = msg.find(|c: char| matches!(c, ':' | '"' | '\'' | '`') || c.is_ascii_digit()).unwrap_or(msg.len());
    msg[..end].trim()
}

/// Echotype's own files are crate-relative ("src/lib.rs"); dependencies and std are absolute paths.
fn location(file: &str, line: u32) -> String {
    if file.starts_with("src/") && !file.contains("..") { format!("{file}:{line}") } else { "external".into() }
}

/// Only the symbolized function names of a std backtrace ("   3: echotype_lib::process"):
/// its "at <file>" lines and unsymbolized addresses go.
fn frames(backtrace: &str) -> String {
    let name = |l: &str| {
        let (n, f) = l.trim().split_once(": ")?;
        (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && !f.starts_with("0x") && f != "<unknown>").then(|| f.to_string())
    };
    backtrace.lines().filter_map(name).collect::<Vec<_>>().join("\n")
}

/// Last pass over what is left: any word that looks like an email, a URL or an absolute path goes.
fn redact(text: &str) -> String {
    let word = |w: &str| {
        let t = w.trim_start_matches(['(', '[', '<', '{', '"', '\'', '`']);
        let drive = t.as_bytes().get(1..3) == Some(b":\\".as_slice());
        if t.contains("://") {
            "[url]".to_string()
        } else if t.contains('@') && t.contains('.') {
            "[email]".into()
        } else if t.starts_with(['/', '~', '\\']) || drive || t.contains("/Users/") || t.contains("/private/") || t.contains("/home/") || t.contains(":\\") {
            "[path]".into()
        } else {
            w.to_string()
        }
    };
    text.lines().map(|l| l.split(' ').map(word).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>().join("\n")
}

fn crash(message: &str, file: &str, line: u32, backtrace: &str) -> Crash {
    Crash {
        version: VERSION.get().cloned().unwrap_or_default(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        message: redact(&format!("{} at {}", prefix(message), location(file, line))).chars().take(300).collect(),
        backtrace: redact(&frames(backtrace)).chars().take(8_000).collect(),
    }
}

/// Straight from the settings file: a panic has no app state to ask, and unreadable means no.
fn opted_in() -> bool {
    std::fs::read_to_string(Settings::path()).ok().and_then(|s| serde_json::from_str::<Settings>(&s).ok()).is_some_and(|s| s.share_telemetry)
}

fn write_crash(dir: &Path, on: bool, report: impl FnOnce() -> Crash) {
    let dir = crash_dir(dir);
    if !on || std::fs::read_dir(&dir).map_or(0, |r| r.count()) >= MAX_CRASHES {
        return;
    }
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(format!("{ms}.json")), serde_json::to_string(&report()).unwrap_or_default());
}

/// Opted in, a panic leaves a small report in the data dir for the sender at the next launch.
pub fn install_panic_hook(version: String) {
    let _ = VERSION.set(version);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_crash(&settings::data_dir(), opted_in(), || {
            let payload = info.payload();
            let msg = payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("");
            let (file, line) = info.location().map_or(("", 0), |l| (l.file(), l.line()));
            crash(msg, file, line, &std::backtrace::Backtrace::force_capture().to_string())
        });
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("echotype-telemetry-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn put_crash(dir: &Path, name: &str) {
        std::fs::create_dir_all(crash_dir(dir)).unwrap();
        std::fs::write(crash_dir(dir).join(name), serde_json::to_string(&crash("boom", "src/lib.rs", 1, "frames")).unwrap()).unwrap();
    }

    /// Settings full of things that must never leave: words, names, emails, app names, paths.
    fn private() -> Settings {
        let mut s = Settings { share_telemetry: true, advanced: true, display_name: "Aditya Secretname".into(), dictionary: vec!["Zyxwordy".into()], ..Settings::default() };
        s.replacements = vec![settings::Replacement { from: "my email".into(), to: "someone@example.com".into() }];
        s.stt.local_model = "/Users/someone/models/ggml-private.bin".into();
        s.cleanup.enabled = true;
        s.cleanup.provider = "custom".into();
        s.cleanup.model = "acme-internal-model".into();
        s.cleanup.base_url = "https://llm.acme.internal/v1".into();
        s.cleanup.custom_instructions = "Sign off as Aditya".into();
        s.cleanup.app_styles = vec![settings::AppStyle { id: "com.acme.SecretApp".into(), name: "Secret App".into(), style: "casual".into(), ..Default::default() }];
        s
    }

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn the_payload_holds_counts_and_ids_only() {
        let dir = scratch("payload");
        let s = private();
        add(&dir, &s, d("2026-10-18"), |c| {
            c.dictations = 3;
            c.words = 42;
            c.speech_ms = vec![900, 300, 600];
            c.cleanup_ms = vec![500];
            c.failures.insert(Failure::PasteFellBack, 2);
            c.models = models(&s);
            c.meetings_started = 1;
        });
        put_crash(&dir, "1.json");
        let mut sent = String::new();
        tick(&dir, true, true, "0.1.0", d("2026-10-19"), |b| {
            sent = serde_json::to_string(b).unwrap();
            true
        });
        for secret in ["Secretname", "Zyxwordy", "someone", "example.com", "ggml-private", "/Users", "acme", "Secret App", "SecretApp", "Sign off"] {
            assert!(!sent.contains(secret), "{secret} in {sent}");
        }
        let v: Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(keys(&v), ["arch", "crashes", "days", "install_id", "os", "version"]);
        let day = &v["days"][0];
        assert_eq!(
            keys(day),
            ["cleanup_model", "cleanup_ms_median", "cleanup_provider", "date", "dictations", "failures", "meetings_finished", "meetings_started", "speech_model", "speech_ms_median", "speech_provider", "words"]
        );
        assert_eq!((day["speech_ms_median"].as_u64(), day["failures"]["paste_fell_back"].as_u64()), (Some(600), Some(2)));
        assert_eq!((day["speech_model"].as_str(), day["cleanup_provider"].as_str(), day["cleanup_model"].as_str()), (Some("file"), Some("custom"), Some("custom")));
        assert_eq!(keys(&v["crashes"][0]), ["arch", "backtrace", "message", "os", "version"]);
        assert_eq!(v["install_id"].as_str().unwrap().len(), 32);
    }

    #[test]
    fn off_sends_nothing_counts_nothing_and_deletes_crash_reports() {
        let dir = scratch("off");
        let mut s = private();
        add(&dir, &s, d("2026-10-18"), |c| c.dictations = 1); // counted while it was on
        put_crash(&dir, "1.json");
        s.share_telemetry = false;
        add(&dir, &s, d("2026-10-18"), |_| panic!("nothing is counted while off"));
        tick(&dir, false, true, "0.1.0", d("2026-10-19"), |_| panic!("nothing is sent while off"));
        assert!(!crash_dir(&dir).exists(), "crash reports are deleted, not kept");
        assert!(!file(&dir).exists(), "unsent counts and the install id are deleted");
    }

    #[test]
    fn once_a_day_and_kept_only_until_the_server_takes_them() {
        let dir = scratch("daily");
        let s = Settings { share_telemetry: true, ..Settings::default() };
        for day in ["2026-10-17", "2026-10-18", "2026-10-19"] {
            add(&dir, &s, d(day), |c| c.dictations += 1);
        }
        // The server is down: one try today, nothing deleted.
        let mut tries = 0;
        tick(&dir, true, true, "v", d("2026-10-19"), |b| {
            tries += 1;
            assert_eq!(b.days.iter().map(|x| x.date).collect::<Vec<_>>(), [d("2026-10-17"), d("2026-10-18")], "today is not finished");
            false
        });
        tick(&dir, true, false, "v", d("2026-10-19"), |_| panic!("at most once a day"));
        assert_eq!(tries, 1);
        assert_eq!(load(&file(&dir)).days.len(), 3);
        // The next day it is up: the three finished days go and are forgotten here.
        let id = load(&file(&dir)).install_id;
        tick(&dir, true, false, "v", d("2026-10-20"), |b| {
            assert_eq!((b.days.len(), b.install_id.as_str()), (3, id.as_str()), "one id for the install");
            true
        });
        assert!(load(&file(&dir)).days.is_empty());
        // Crash reports go at launch, also on a day the counts were already sent.
        put_crash(&dir, "2.json");
        tick(&dir, true, true, "v", d("2026-10-20"), |b| b.crashes.len() == 1 && b.days.is_empty());
        assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 0);
    }

    #[test]
    fn a_long_outage_keeps_only_a_few_days() {
        let dir = scratch("outage");
        let s = Settings { share_telemetry: true, ..Settings::default() };
        for day in d("2026-10-01").iter_days().take(20) {
            add(&dir, &s, day, |c| c.dictations += 1);
        }
        let days = load(&file(&dir)).days;
        assert_eq!(days.len(), KEEP_DAYS);
        assert_eq!(days.keys().next(), Some(&d("2026-10-16")), "the newest are kept");
    }

    #[test]
    fn a_crash_report_keeps_only_fixed_wording_location_and_function_names() {
        let secrets = ["secret transcript", "someone@example.com", "https://acme.example/x", "/Users/someone", "someone", "acme", "0x1"];
        let msg = r#"Could not paste "my secret transcript" for someone@example.com via https://acme.example/x from /Users/someone/Desktop/notes.txt"#;
        let bt = "   0: std::backtrace::Backtrace::force_capture\n             at /rustc/abc/library/std/src/backtrace.rs:312:13\n   1: echotype_lib::process::{{closure}}\n             at /Users/someone/echotype/app/src-tauri/src/lib.rs:580:9\n   2: 0x1\n   3: <unknown>";
        let c = crash(msg, "src/lib.rs", 12, bt);
        assert_eq!(c.message, "Could not paste at src/lib.rs:12");
        assert_eq!(c.backtrace, "std::backtrace::Backtrace::force_capture\nechotype_lib::process::{{closure}}");
        let real = frames(&std::backtrace::Backtrace::force_capture().to_string());
        assert!(real.contains("telemetry::tests") && !real.contains(".rs:") && !real.contains('/'), "{real}");
        // An Err's text, a backticked string, a number: the interpolated part never makes it.
        for (m, want) in [
            (r#"called `Result::unwrap()` on an `Err` value: "said my secret transcript""#, "called"),
            ("byte index 3 is not a char boundary; it is inside `my secret transcript`", "byte index"),
            ("explicit panic", "explicit panic"),
        ] {
            assert_eq!(crash(m, "src/x.rs", 1, "").message, format!("{want} at src/x.rs:1"));
        }
        assert_eq!(crash("boom", "/Users/someone/.cargo/registry/src/x/lib.rs", 3, "").message, "boom at external");
        // Whatever might remain is redacted again.
        let r = redact("see someone@example.com https://acme.example/x (/Users/someone/a) /private/var/x C:\\Users\\a ~/notes fine::name");
        assert_eq!(r, "see [email] [url] [path] [path] [path] [path] fine::name");
        let all = serde_json::to_string(&c).unwrap();
        for s in secrets {
            assert!(!all.contains(s), "{s} in {all}");
        }
        assert_eq!(crash(&"x".repeat(900), "src/a.rs", 1, "").message.len(), 300);
    }

    #[test]
    fn no_crash_file_while_off() {
        let dir = scratch("hook");
        write_crash(&dir, false, || panic!("not even built while off"));
        assert!(!crash_dir(&dir).exists());
        write_crash(&dir, true, || crash("boom", "src/lib.rs", 1, ""));
        assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 1);
    }

    /// Against a local server/: ECHOTYPE_CLOUD_URL=http://127.0.0.1:8790/v1 cargo test live_telemetry -- --ignored
    #[test]
    #[ignore = "needs a running server"]
    fn live_telemetry() {
        let dir = scratch("live");
        let s = private();
        add(&dir, &s, d("2026-10-18"), |c| {
            c.dictations = 1;
            c.speech_ms = vec![400];
            c.failures.insert(Failure::NoSpeech, 1);
            c.models = models(&s);
        });
        put_crash(&dir, "1.json");
        tick(&dir, true, true, "0.1.0", d("2026-10-19"), post);
        assert!(load(&file(&dir)).days.is_empty(), "the server took the day");
        assert_eq!(std::fs::read_dir(crash_dir(&dir)).unwrap().count(), 0, "and the crash report");
    }

    #[test]
    fn medians_and_model_labels() {
        assert_eq!((median(&[]), median(&[5]), median(&[9, 1, 5])), (None, Some(5), Some(5)));
        let mut s = Settings { advanced: true, ..Settings::default() };
        s.stt.local_model = "tiny.en".into();
        s.cleanup.enabled = true;
        s.cleanup.provider = "ollama".into();
        s.cleanup.model = "gemma3:1b".into();
        assert_eq!(models(&s), Models { speech_provider: "local".into(), speech_model: "tiny.en".into(), cleanup_provider: "ollama".into(), cleanup_model: "gemma3:1b".into() });
        s.cleanup.enabled = false;
        s.stt.provider = "groq".into();
        s.stt.model = "whisper-large-v3-turbo".into();
        let m = models(&s);
        assert_eq!((m.speech_provider.as_str(), m.speech_model.as_str(), m.cleanup_provider.as_str()), ("groq", "whisper-large-v3-turbo", "off"));
    }
}
