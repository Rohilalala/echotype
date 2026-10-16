//! The built-in cleanup model: llama.cpp's llama-server, shipped inside the app as a sidecar
//! (bundle.externalBin, built by scripts/build-llama-server.sh), runs the Echotype GGUF on
//! 127.0.0.1 behind a random key. It starts on the first cleanup (or as soon as the provider is
//! chosen), starts again if it died, and stops when the provider changes away or Echotype exits.
//! A separate process because whisper-rs already links its own ggml and a second copy collides.

use crate::settings::Settings;
use crate::stt;
use serde::Serialize;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Cleaner {
    /// Its stt::RAM_MB row, and what `cleanup.model` holds when it is picked by hand.
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub size_mb: u64,
    pub sha256: &'static str,
    /// One line for the Models page.
    pub note: &'static str,
}

/// The Echotype cleanup models (Qwen2.5 Instruct fine-tunes, Apache-2.0), from the 2026-10-16
/// distillation: on 12,198 real clips they add 0.31 (Tiny) and 0.51 (Medium) points of WER over raw
/// whisper, against 5.23 for the first 1.5B they replace. Automatic gives low-memory Macs Tiny.
pub const CLEANERS: &[Cleaner] = &[
    Cleaner { id: "echotype-tiny", label: "Echotype Tiny", file: "echotype-tiny-q5_k_m.gguf", size_mb: 401, sha256: "5b6faf7e64f41d34569413409bc0628e4c48d2893ad95ace9b51192b6bee2be9", note: "0.5B. Light and quick; best for English." },
    Cleaner { id: "echotype-medium", label: "Echotype Medium", file: "echotype-medium-q4_k_m.gguf", size_mb: 940, sha256: "944fa5a9221b7a1c0e8b8617392df071ef57323a64dc3e99f5a563e02ace6f37", note: "1.5B. Catches more; uses about twice the memory." },
];

/// Where the cleaner files are served from: the echotype-models Cloudflare R2 bucket (uploaded with
/// app/scripts/r2-put.sh, beside Qwen's LICENSE and a NOTICE), `{DOWNLOAD_URL}/{file}`.
// ponytail: the bucket's r2.dev address, which Cloudflare rate-limits; a custom domain for launch.
pub const DOWNLOAD_URL: Option<&str> = Some("https://pub-da62fbe34de644598313554b0a5ccc51.r2.dev");

pub fn find(id: &str) -> Option<&'static Cleaner> {
    CLEANERS.iter().find(|c| c.id == id)
}

/// Downloads cleaner `id`, kept only if its SHA-256 matches.
pub fn download(id: &str, progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    let c = find(id).ok_or_else(|| format!("Unknown cleanup model '{id}'"))?;
    let base = DOWNLOAD_URL.ok_or("Downloads of the Echotype models are coming soon.")?;
    stt::fetch(&format!("{}/{}", base.trim_end_matches('/'), c.file), &model_path(c), c.sha256, progress)
}

/// Deletes cleaner `id`'s file, stopping the server first if it is running on it.
pub fn delete(id: &str) -> Result<(), String> {
    let c = find(id).ok_or_else(|| format!("Unknown cleanup model '{id}'"))?;
    if !installed(c) {
        return Err(format!("{} is not downloaded", c.label));
    }
    let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|s| s.opts.model == model_path(c)) {
        *g = None;
    }
    drop(g);
    std::fs::remove_file(model_path(c)).map_err(|e| e.to_string())
}

/// Tokens of context the cleaner is served with (prompt, transcript and answer together):
/// cleanup::cleanup splits longer dictations to fit. Advanced settings can change it.
pub const CONTEXT: u32 = 1024;
const START_TIMEOUT: Duration = Duration::from_secs(60);

pub fn model_path(c: &Cleaner) -> PathBuf {
    stt::models_dir().join(c.file)
}

fn installed(c: &Cleaner) -> bool {
    model_path(c).is_file()
}

/// The cleaner Automatic uses on a Mac with this much memory: the 1.5B, or on a low-memory Mac
/// the smallest one; the closest installed one until that is installed.
fn auto_cleaner(total_mb: u64, installed: impl Fn(&str) -> bool) -> &'static Cleaner {
    let by_ram = |c: &&Cleaner| stt::ram_of(c.id);
    let want = if stt::low_ram(total_mb) { CLEANERS.iter().min_by_key(by_ram) } else { CLEANERS.iter().max_by_key(by_ram) };
    let want = want.expect("CLEANERS is not empty").id;
    let id = stt::closest(CLEANERS.iter().map(|c| c.id), want, installed).unwrap_or(want);
    CLEANERS.iter().find(|c| c.id == id).expect("an id from CLEANERS")
}

/// The cleaner these settings use: the manual pick (`cleanup.model`) with advanced settings on, else Automatic's.
pub fn cleaner(s: &Settings) -> &'static Cleaner {
    let manual = CLEANERS.iter().find(|c| s.advanced && c.id == s.cleanup.model);
    manual.unwrap_or_else(|| auto_cleaner(stt::ram_mb().0, |id| CLEANERS.iter().any(|c| c.id == id && installed(c))))
}

/// "save" (macOS may page the mapped model out under pressure) or "ready" (locked in RAM):
/// Automatic saves on a low-memory Mac.
pub fn memory(s: &Settings) -> &str {
    if s.advanced {
        &s.cleaner_memory
    } else if stt::low_ram(stt::ram_mb().0) {
        "save"
    } else {
        "ready"
    }
}

/// The context the built-in cleaner is served with: CONTEXT, or the advanced setting.
pub fn context(s: &Settings) -> u32 {
    if s.advanced { s.cleaner_context.clamp(512, 8192) } else { CONTEXT }
}

/// Minutes without a cleanup before the server stops; None = never (Automatic, or the switch off).
fn idle(s: &Settings) -> Option<Duration> {
    (s.advanced && s.cleaner_idle_unload).then(|| Duration::from_secs(60 * u64::from(s.cleaner_idle_minutes.max(5))))
}

#[derive(Serialize)]
pub struct CleanerStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub note: &'static str,
    /// Whether it can be downloaded yet (DOWNLOAD_URL is set).
    pub downloadable: bool,
    pub size_mb: u64,
    pub ram_mb: u64,
    pub installed: bool,
    pub automatic: bool,
    pub warning: Option<String>,
}

/// Every cleaner for the Cleanup page, with the same RAM labels and warnings as the speech models.
pub fn cleaners(total_mb: u64) -> Vec<CleanerStatus> {
    let auto = auto_cleaner(total_mb, |id| CLEANERS.iter().any(|c| c.id == id && installed(c)));
    CLEANERS
        .iter()
        .map(|c| CleanerStatus {
            id: c.id,
            label: c.label,
            note: c.note,
            downloadable: DOWNLOAD_URL.is_some(),
            size_mb: model_path(c).metadata().ok().filter(|m| m.is_file()).map_or(c.size_mb, |m| m.len() / 1_048_576),
            ram_mb: stt::ram_of(c.id),
            installed: installed(c),
            automatic: c.id == auto.id,
            warning: stt::ram_warning(stt::ram_of(c.id), stt::ram_of(auto.id), total_mb),
        })
        .collect()
}

/// (installed, size in MB) of the cleaner these settings use, for the Cleanup page.
pub fn status(s: &Settings) -> (bool, u64) {
    let c = cleaner(s);
    cleaners(stt::ram_mb().0).into_iter().find(|x| x.id == c.id).map_or((false, c.size_mb), |x| (x.installed, x.size_mb))
}

/// Whether these settings use the built-in model, so its server should be running.
pub fn wanted(s: &Settings) -> bool {
    s.cleanup.enabled && s.cleanup.provider == "echotype"
}

/// After a settings change, or at startup: start the server ahead of the first dictation, or free its memory.
pub fn apply(s: &Settings) {
    if !wanted(s) {
        return stop();
    }
    let t = Instant::now();
    match ensure(s) {
        Ok(_) => eprintln!("echotype: cleanup model ready in {} ms", t.elapsed().as_millis()),
        Err(e) => eprintln!("echotype: cleanup model not started: {e}"),
    }
}

struct Server {
    child: Child,
    port: u16,
    key: String,
    /// What it was started with; different settings restart it on the next cleanup.
    opts: Opts,
    last_used: Instant,
}

/// The settings that become llama-server flags.
#[derive(Clone, Debug, PartialEq)]
struct Opts {
    model: PathBuf,
    ctx: u32,
    /// "ready": keep the weights locked in RAM.
    lock: bool,
}

/// mlock failed once in this run, so "ready" loads the weights unmapped instead (--load-mode none).
static MLOCK_FAILED: AtomicBool = AtomicBool::new(false);
/// "Unload when idle", in seconds; 0 = never. Read by the reaper thread.
static IDLE_SECS: AtomicU64 = AtomicU64::new(0);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ponytail: one server, one request at a time (-np 1); cleanup is one short request per dictation.
static SERVER: Mutex<Option<Server>> = Mutex::new(None);

/// Base URL and bearer key of the running server, starting it first if it is not running or was
/// started with other settings (a memory, context or model change applies on the next cleanup).
pub fn ensure(settings: &Settings) -> Result<(String, String), String> {
    let c = cleaner(settings);
    let opts = Opts { model: model_path(c), ctx: context(settings), lock: memory(settings) == "ready" };
    IDLE_SECS.store(idle(settings).map_or(0, |d| d.as_secs()), Ordering::Relaxed);
    let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = g.as_mut()
        && matches!(s.child.try_wait(), Ok(None))
        && s.opts == opts
    {
        s.last_used = Instant::now();
        return Ok((url(s.port), s.key.clone()));
    }
    *g = None; // died, never started or other settings: stop it before starting another
    let bin = sidecar();
    preflight(bin.as_deref(), &opts.model, stt::ram_of(c.id), stt::ram_mb().1)?;
    stt::warn_if_not_internal(&opts.model);
    let log = crate::settings::data_dir().join("llama-server.log");
    let mut s = start(&bin.clone().expect("checked by preflight"), &opts, &log)?;
    if opts.lock && !MLOCK_FAILED.load(Ordering::Relaxed) && mlock_failed(&log) {
        // "Keep ready" never fails a cleanup: load the weights unmapped instead, which macOS
        // can only swap, not drop. Remembered for this run so later starts skip the attempt.
        eprintln!("echotype: could not lock the cleanup model in memory (see {}); loading it without mmap instead", log.display());
        MLOCK_FAILED.store(true, Ordering::Relaxed);
        drop(s);
        s = start(&bin.expect("checked by preflight"), &opts, &log)?;
    }
    let out = (url(s.port), s.key.clone());
    *g = Some(s);
    reap_when_idle();
    Ok(out)
}

/// llama.cpp logs this and carries on unlocked when mlock is refused (a wired-memory limit).
fn mlock_failed(log: &Path) -> bool {
    std::fs::read_to_string(log).is_ok_and(|l| l.contains("failed to mlock"))
}

/// "Unload when idle": one thread for the life of the app stops the server once it has gone
/// unused for IDLE_SECS; the next cleanup starts it again.
fn reap_when_idle() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(30));
            let idle = IDLE_SECS.load(Ordering::Relaxed);
            let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
            if idle > 0 && g.as_ref().is_some_and(|s| s.last_used.elapsed().as_secs() >= idle) {
                *g = None;
                eprintln!("echotype: cleanup model stopped after {} min unused", idle / 60);
            }
        }
    });
}

/// The first cleanup after a quiet spell took ~740 ms against 55-150 ms warm (live, M5): the
/// weights and GPU go cold. Called when a recording starts: if the cleaner has been idle for 20 s
/// or is not running, it answers one short request with the same system prompt (llama-server
/// keeps that prefix cached), so it is warm by the time the speech is transcribed.
pub fn prewarm(s: &Settings) {
    if !s.cleanup.enabled {
        return;
    }
    if s.cleanup.provider != "echotype" {
        return prewarm_local_server(s);
    }
    let cold = SERVER.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|v| v.last_used.elapsed() >= Duration::from_secs(20));
    if cold {
        let s = s.clone();
        std::thread::spawn(move || {
            let _ = crate::cleanup::chat(&s, &crate::cleanup::system_prompt(&s), "Okay.", Duration::from_secs(20));
        });
    }
}

/// Ollama and LM Studio unload an idle model (Ollama after 5 minutes), and the first cleanup after
/// that waited on loading it: 7.1 s for gemma4:e4b against 244 ms warm. When a recording starts
/// after a minute or more without one, a short request loads it while the user talks. API
/// providers are never pinged: that would cost the user money.
fn prewarm_local_server(s: &Settings) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let local = crate::settings::preset(&s.cleanup.provider).is_some_and(|p| !p.needs_key && p.id != "custom");
    if !local || s.cleanup.model.trim().is_empty() {
        return;
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return;
    }
    *last = Some(Instant::now());
    let s = s.clone();
    std::thread::spawn(move || {
        let _ = crate::cleanup::chat(&s, &crate::cleanup::system_prompt(&s), "Okay.", Duration::from_secs(60));
    });
}

/// After a cleanup: counts as use, so "Unload when idle" waits from the end of a long request.
pub fn touch() {
    if let Some(s) = SERVER.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        s.last_used = Instant::now();
    }
}

/// Stops the server; it starts again on the next cleanup. Also called on exit.
pub fn stop() {
    *SERVER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Next to Echotype's own executable: Contents/MacOS/ in the app bundle, target/<profile>/ in a
/// cargo build (tauri-build copies it there), one level up from a test binary in target/<profile>/deps/.
fn sidecar() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors().skip(1).take(2).map(|d| d.join("llama-server")).find(|p| p.is_file())
}

fn preflight(sidecar: Option<&Path>, model: &Path, need_mb: u64, free_mb: u64) -> Result<(), String> {
    if !model.is_file() {
        let file = model.file_name().unwrap_or_default().to_string_lossy();
        return Err(format!("The Echotype model isn't installed. Put {file} in {} or pick another cleanup provider", model.parent().unwrap_or(model).display()));
    }
    if sidecar.is_none() {
        return Err("The Echotype model runner (llama-server) is missing from this copy of Echotype. Reinstall Echotype".into());
    }
    if need_mb > free_mb {
        return Err(format!("Not enough free memory for the Echotype model: it needs about {need_mb} MB and {free_mb} MB is free. Close some apps or pick another cleanup provider"));
    }
    Ok(())
}

fn url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// llama-server flags for llama.cpp b11619. The GGUF carries Qwen's chat template, used as is.
/// Memory: "save" keeps llama.cpp's default (the weights mapped from the file, so macOS can drop
/// them under pressure and read them back); "ready" adds mlock (b11619's spelling of --mlock is
/// --load-mode mmap+mlock), or after a failed mlock loads them unmapped (--no-mmap: --load-mode none).
fn args(o: &Opts, port: u16, mlock_failed: bool) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec!["-m".into(), o.model.clone().into()];
    for s in ["--host", "127.0.0.1", "--port", &port.to_string(), "-c", &o.ctx.to_string(), "-ngl", "99", "-np", "1", "--no-webui", "--offline"] {
        a.push(s.into());
    }
    if o.lock {
        a.extend(["--load-mode".into(), if mlock_failed { "none" } else { "mmap+mlock" }.into()]);
    }
    a
}

/// A free localhost port. ponytail: another process could take it before the server binds; that
/// start then fails and the next cleanup picks a new port.
fn free_port() -> Result<u16, String> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

fn start(bin: &Path, opts: &Opts, log: &Path) -> Result<Server, String> {
    let (port, key) = (free_port()?, crate::api::new_token()?);
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let stderr = std::fs::File::create(log).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
    let child = Command::new(bin)
        .args(args(opts, port, MLOCK_FAILED.load(Ordering::Relaxed)))
        .env("LLAMA_API_KEY", &key) // in the environment, not argv, so `ps` never shows it
        // The pinned llama-server's router-child mode: it exits when its stdin closes, so it can
        // never outlive Echotype, even after a crash or kill -9. We hold the pipe and never write to it.
        .env("LLAMA_SERVER_ROUTER_PORT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("The Echotype model could not start: {e}"))?;
    let mut s = Server { child, port, key, opts: opts.clone(), last_used: Instant::now() };
    wait_ready(&mut s, START_TIMEOUT).map_err(|e| format!("The Echotype model did not start: {e} (see {})", log.display()))?;
    Ok(s) // on error `s` drops above, which kills the process
}

/// Whether process `pid` is the one listening on 127.0.0.1:`port`.
// ponytail: one lsof call at start-up (~50 ms); read the socket table with proc_pidfdinfo if lsof
// is ever missing. macOS only: the built-in model's server ships with the macOS app alone.
#[cfg(target_os = "macos")]
fn listens_on(pid: u32, port: u16) -> bool {
    Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().any(|p| p == pid.to_string()))
}

/// Off macOS there is no built-in model server and no ownership check yet, so never trust one.
#[cfg(not(target_os = "macos"))]
fn listens_on(_: u32, _: u16) -> bool {
    false
}

/// Polls GET /health (503 while the model loads, 200 once it answers) until ready, dead or timed out.
fn wait_ready(s: &mut Server, timeout: Duration) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)).build().map_err(|e| e.to_string())?;
    let health = format!("http://127.0.0.1:{}/health", s.port);
    let t = Instant::now();
    loop {
        if let Ok(Some(status)) = s.child.try_wait() {
            return Err(format!("it exited ({status})"));
        }
        if client.get(&health).send().is_ok_and(|r| r.status().is_success()) {
            // The port was free a moment ago, not reserved: make sure the process answering on it
            // is the one we started before it gets the key and transcripts.
            if !listens_on(s.child.id(), s.port) {
                return Err(format!("another program answered on port {}", s.port));
            }
            return Ok(());
        }
        if t.elapsed() > timeout {
            return Err(format!("it was not ready after {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_args_bind_localhost_on_the_given_port() {
        let o = Opts { model: "/m/echotype-clean-q4_k_m.gguf".into(), ctx: CONTEXT, lock: false };
        let a = args(&o, 4321, false);
        let want = ["-m", "/m/echotype-clean-q4_k_m.gguf", "--host", "127.0.0.1", "--port", "4321", "-c", "1024", "-ngl", "99", "-np", "1", "--no-webui", "--offline"];
        assert_eq!(a, want.map(OsString::from));
        assert!(!a.iter().any(|x| x.to_string_lossy().contains("api-key")), "the key goes in the environment, never argv");
        assert_eq!(url(4321), "http://127.0.0.1:4321/v1");
    }

    #[test]
    fn memory_modes_become_load_flags() {
        let mut o = Opts { model: "/m/x.gguf".into(), ctx: 2048, lock: false };
        let tail = |o: &Opts, failed| args(o, 1, failed).into_iter().skip(14).map(|a| a.into_string().unwrap()).collect::<Vec<_>>();
        assert!(tail(&o, false).is_empty(), "save: llama.cpp's default mmap");
        assert!(args(&o, 1, false).contains(&"2048".into()));
        o.lock = true;
        assert_eq!(tail(&o, false), ["--load-mode", "mmap+mlock"], "ready: --mlock");
        assert_eq!(tail(&o, true), ["--load-mode", "none"], "ready after a failed mlock: --no-mmap");
    }

    #[test]
    fn automatic_cleaner_memory_and_context_follow_the_mac() {
        assert_eq!(auto_cleaner(8192, |_| true).id, "echotype-tiny", "8 GB: the smallest");
        assert_eq!(auto_cleaner(24_576, |_| true).id, "echotype-medium");
        assert_eq!(auto_cleaner(24_576, |id| id == "echotype-tiny").id, "echotype-tiny", "the installed one until Medium is");
        let mut s = Settings { cleaner_memory: "save".into(), cleaner_context: 4096, cleaner_idle_unload: true, cleaner_idle_minutes: 1, ..Default::default() };
        assert_eq!(context(&s), CONTEXT, "advanced off: the one constant");
        assert_eq!(idle(&s), None);
        s.advanced = true;
        assert_eq!((memory(&s), context(&s)), ("save", 4096));
        assert_eq!(idle(&s), Some(Duration::from_secs(300)), "at least 5 minutes, longer than any request");
        s.cleanup.model = "echotype-clean".into(); // the first 1.5B, no longer offered
        assert!(find(cleaner(&s).id).is_some(), "an unknown pick falls back to Automatic's");
        let st = cleaners(8192);
        assert!(st.iter().all(|c| c.ram_mb > 0));
        assert!(st.iter().find(|c| c.id == "echotype-medium").unwrap().warning.is_some(), "Medium on an 8 GB Mac");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_port_is_only_trusted_from_the_process_listening_on_it() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(listens_on(std::process::id(), port), "this test process listens there");
        assert!(!listens_on(1, port), "launchd does not");
        drop(l);
        assert!(!listens_on(std::process::id(), port), "nobody listens any more");
    }

    #[test]
    fn free_port_is_bindable() {
        let p = free_port().unwrap();
        assert!(p > 0);
        std::net::TcpListener::bind(("127.0.0.1", p)).unwrap();
    }

    #[test]
    fn preflight_says_what_is_missing_first() {
        let dir = std::env::temp_dir().join(format!("echotype-llm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (model, bin) = (dir.join(CLEANERS[0].file), dir.join("llama-server"));
        let err = preflight(Some(&bin), &model, 1113, 64_000).unwrap_err();
        assert!(err.starts_with(&format!("The Echotype model isn't installed. Put {} in ", CLEANERS[0].file)), "{err}");
        std::fs::write(&model, b"x").unwrap();
        assert!(preflight(None, &model, 1113, 64_000).unwrap_err().contains("runner (llama-server) is missing"));
        let err = preflight(Some(&bin), &model, 1113, 1000).unwrap_err();
        assert!(err.contains("needs about 1113 MB and 1000 MB is free"), "{err}");
        assert!(preflight(Some(&bin), &model, 1113, 1113).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_enabled_echotype_cleanup_wants_the_server() {
        let mut s = Settings::default();
        s.cleanup.provider = "echotype".into();
        assert!(!wanted(&s), "cleanup off");
        s.cleanup.enabled = true;
        assert!(wanted(&s));
        s.cleanup.provider = "ollama".into();
        assert!(!wanted(&s));
    }

    #[test]
    fn a_refused_mlock_is_read_from_the_server_log() {
        let log = std::env::temp_dir().join(format!("echotype-mlock-{}.log", std::process::id()));
        std::fs::write(&log, "warning: failed to mlock 983040000-byte buffer (after previously locking 0 bytes): Resource temporarily unavailable\n").unwrap();
        assert!(mlock_failed(&log));
        std::fs::write(&log, "load_model: loading model\n").unwrap();
        assert!(!mlock_failed(&log));
        std::fs::remove_file(&log).unwrap();
        assert!(!mlock_failed(&log), "no log, no failure");
    }

    /// The real sidecar: memory modes reach llama-server's command line, a change restarts it on
    /// the next cleanup, and "Unload when idle" stops it. `cargo test -- --ignored --nocapture modes_and_idle`.
    #[test]
    #[ignore]
    fn modes_and_idle_on_the_real_sidecar() {
        if !installed(&CLEANERS[0]) {
            eprintln!("skipped: no {}", model_path(&CLEANERS[0]).display());
            return;
        }
        let argv = || {
            let pid = SERVER.lock().unwrap().as_ref().map(|s| s.child.id()).expect("running");
            let out = Command::new("ps").args(["-o", "args=", "-p", &pid.to_string()]).output().unwrap();
            (pid, String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let mut s = Settings { advanced: true, cleaner_memory: "ready".into(), ..Default::default() };
        s.cleanup.enabled = true;
        s.cleanup.provider = "echotype".into();
        let t = Instant::now();
        ensure(&s).unwrap();
        let (ready_pid, a) = argv();
        eprintln!("ready ({} ms): {a}", t.elapsed().as_millis());
        assert!(a.contains("--load-mode mmap+mlock") && a.contains("-c 1024"), "{a}");
        assert!(crate::cleanup::cleanup(&s, "um so the meeting is at uh five").unwrap().contains("meeting"));
        s.cleaner_memory = "save".into();
        s.cleaner_context = 2048;
        ensure(&s).unwrap();
        let (save_pid, a) = argv();
        eprintln!("save: {a}");
        assert!(save_pid != ready_pid && !a.contains("--load-mode") && a.contains("-c 2048"), "{a}");
        s.cleaner_idle_unload = true;
        ensure(&s).unwrap();
        assert_eq!(argv().0, save_pid, "idle unload is not a server flag: no restart");
        IDLE_SECS.store(1, Ordering::Relaxed); // instead of the 5-minute minimum
        std::thread::sleep(Duration::from_secs(32));
        assert!(SERVER.lock().unwrap().is_none(), "stopped after going unused");
        let t = Instant::now();
        assert!(crate::cleanup::cleanup(&s, "um so the meeting is at uh five").unwrap().contains("meeting"));
        eprintln!("first cleanup after an idle unload: {} ms", t.elapsed().as_millis());
        stop();
    }

    /// The real sidecar and model: `cargo test -- --ignored llm` after scripts/build-llama-server.sh,
    /// with the GGUF in the models dir. Starts the server, cleans one transcript, stops it.
    #[test]
    #[ignore]
    fn real_sidecar_cleans_a_transcript() {
        if !installed(&CLEANERS[0]) {
            eprintln!("skipped: no {}", model_path(&CLEANERS[0]).display());
            return;
        }
        let mut s = Settings::default();
        s.cleanup.enabled = true;
        s.cleanup.provider = "echotype".into();
        s.dictionary = vec!["Arnav".into()];
        let out = crate::cleanup::cleanup(&s, "um so arnav said the meeting is at uh five").unwrap();
        assert!(out.contains("Arnav") && !out.contains("um ") && !out.contains("uh"), "{out}");
        assert_eq!(crate::cleanup::cleanup(&s, "um").unwrap(), "", "filler only is no speech");
        let (base, _) = ensure(&s).unwrap();
        let unauthorized = reqwest::blocking::Client::new().post(format!("{base}/chat/completions")).json(&crate::cleanup::request_body("x", "s", "t", false)).send().unwrap();
        assert_eq!(unauthorized.status(), 401, "other local processes cannot use it without the key");
        let pid = || SERVER.lock().unwrap().as_ref().unwrap().child.id();
        let kill = |sig: &str, pid: u32| Command::new("kill").args([sig, &pid.to_string()]).stderr(Stdio::null()).status().unwrap().success();
        let first = pid();
        assert!(kill("-9", first));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(crate::cleanup::cleanup(&s, "um").unwrap(), "", "a dead server is started again on the next cleanup");
        let second = pid();
        assert_ne!(first, second);
        stop();
        assert!(!kill("-0", second), "stop() ended the server");
    }

    /// The real download from the bucket into a scratch data folder: URL, SHA-256 and file name.
    /// `cargo test -- --ignored downloads_a_cleaner` (401 MB).
    #[test]
    #[ignore]
    fn downloads_a_cleaner_from_the_bucket() {
        let dir = std::env::temp_dir().join(format!("echotype-dl-test-{}", std::process::id()));
        // SAFETY: an ignored test run on its own; nothing else reads the environment meanwhile.
        unsafe { std::env::set_var("ECHOTYPE_CONFIG_DIR", &dir) };
        let mut seen = 0;
        download("echotype-tiny", |d, _| seen = d).unwrap();
        let c = find("echotype-tiny").unwrap();
        assert!(installed(c) && model_path(c).starts_with(&dir), "{}", model_path(c).display());
        assert_eq!(seen, 420_086_208);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
