//! Speech to text: the ggml model catalog, in-process whisper.cpp, and any
//! OpenAI-compatible /audio/transcriptions endpoint.

use crate::settings::{self, Settings};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use sha2::Digest;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

/// Free RAM a model must leave behind for everything else on the machine.
const HEADROOM_MB: u64 = 2048;

pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub size_mb: u64,
    pub note: &'static str,
    /// SHA-256 of the file at HF_COMMIT; downloads that don't match are discarded.
    pub sha256: &'static str,
}

/// Pinned whisper.cpp model repo revision, so a moved branch can't swap files under us.
const HF_COMMIT: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// The speech models offered: Turbo, Automatic's pick on every Mac, and tiny.en for onboarding's
/// first practice run.
// ponytail: small, medium, large-v3 (full and Q5), full turbo and base.en were offered until
// 2026-10-16; the app says more are coming. Their rows (and RAM_MB numbers) are in git history.
// Small (488 MB) was the 8 GB pick but saved 44 MB of RAM over Turbo Q5; it only earned its place
// by translating, which comes back with a translating model.
pub const MODELS: &[Model] = &[
    Model { id: "large-v3-turbo-q5_0", label: "Large v3 Turbo (Q5)", size_mb: 547, note: "Best default: near-large accuracy, fast, 99 languages. Cannot translate.", sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2" },
    Model { id: "tiny.en", label: "Tiny (English)", size_mb: 78, note: "English only. Fastest, least accurate.", sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f" },
];

/// Peak RAM of each model, loaded and running one dictation, in MB: what Automatic chooses by,
/// what the warnings and the "about X RAM" labels say, and what the swap check at load time needs.
// ponytail: measured 2026-10-15 on this repo's 24 GB Apple-silicon Mac, headless, one model at a
// time. Whisper: `/usr/bin/time -l echotype --transcribe <8 s wav> --model <file>`, the larger of
// max RSS and peak memory footprint (so the CLI's own ~40 MB is in). Cleaner: llama-server -c 1024
// -ngl 99 with one cleanup, max RSS (mapped weights included). Measure again when whisper.cpp,
// llama.cpp, a model or llm::CONTEXT changes.
pub const RAM_MB: &[(&str, u64)] = &[
    ("large-v3-turbo-q5_0", 761),
    ("tiny.en", 224),
    // 2026-10-16, llama-server -c 1024 -ngl 99, one cleanup, max RSS.
    ("echotype-tiny", 552),
    ("echotype-medium", 1099),
];

/// RAM_MB for a catalog id; anything unknown is assumed to be as large as the largest model.
pub fn ram_of(id: &str) -> u64 {
    RAM_MB.iter().find(|(m, _)| *m == id).map_or(1900, |(_, mb)| *mb)
}

/// Macs with this much memory or less get the light setup: whisper small and "save" memory.
/// Above it, turbo and "ready". Every Mac gets Echotype Tiny for cleanup.
pub const LOW_RAM_MB: u64 = 8192;

pub fn low_ram(total_mb: u64) -> bool {
    total_mb <= LOW_RAM_MB
}

/// The speech model Automatic recommends for a Mac with this much memory.
pub fn recommended(total_mb: u64) -> &'static str {
    let _ = total_mb; // one speech model fits every Mac Echotype supports
    "large-v3-turbo-q5_0"
}

/// Of `ids`, the one `available` that is closest in RAM to `want`, smaller first: `want` itself
/// when it is available. Automatic uses it for both the speech model and the cleaner.
pub fn closest<'a>(ids: impl IntoIterator<Item = &'a str>, want: &str, available: impl Fn(&str) -> bool) -> Option<&'a str> {
    let w = ram_of(want);
    ids.into_iter().filter(|id| available(id)).min_by_key(|id| {
        let r = ram_of(id);
        (r > w, r.abs_diff(w))
    })
}

/// The speech model Automatic uses now: the recommendation once downloaded, else the closest one that is.
fn auto_model(total_mb: u64, downloaded: impl Fn(&str) -> bool) -> &'static str {
    let want = recommended(total_mb);
    closest(MODELS.iter().map(|m| m.id), want, downloaded).unwrap_or(want)
}

/// The local speech model these settings use: the manual pick with advanced settings on, else Automatic's.
pub fn local_model(s: &Settings) -> String {
    if let Some(m) = &s.stt.rerun_model {
        return m.clone();
    }
    if s.advanced { s.stt.local_model.clone() } else { auto_model(ram_mb().0, |id| model_file(id).is_some()).into() }
}

/// "Uses about 1.8 GB while dictating. On an 8 GB Mac this can slow other apps." for a model that
/// needs more than Automatic's pick on a low-memory Mac. A warning only: it stays selectable.
pub fn ram_warning(ram_mb: u64, auto_ram_mb: u64, total_mb: u64) -> Option<String> {
    let gb = total_mb.div_ceil(1024);
    (low_ram(total_mb) && ram_mb > auto_ram_mb).then(|| format!("Uses about {} while dictating. On {} {gb} GB Mac this can slow other apps.", fmt_mb(ram_mb), if gb == 8 { "an" } else { "a" }))
}

/// "800 MB", "1.8 GB".
pub fn fmt_mb(mb: u64) -> String {
    if mb < 1000 { format!("{mb} MB") } else { format!("{:.1} GB", mb as f64 / 1024.0).replace(".0 GB", " GB") }
}

pub fn translates(id: &str) -> bool {
    !id.contains("turbo") && !id.ends_with(".en")
}

pub fn models_dir() -> PathBuf {
    settings::data_dir().join("models")
}

fn catalog(id: &str) -> Result<&'static Model, String> {
    MODELS.iter().find(|m| m.id == id).ok_or_else(|| format!("Unknown model '{id}'"))
}

/// Echotype's own models dir first, then ~/.cache/whisper (whisper.cpp's usual download spot).
pub fn model_file(id: &str) -> Option<PathBuf> {
    let name = format!("ggml-{id}.bin");
    let mut dirs = vec![models_dir()];
    dirs.extend(dirs::home_dir().map(|h| h.join(".cache").join("whisper")));
    dirs.into_iter().map(|d| d.join(&name)).find(|p| p.is_file())
}

/// "large-v3-turbo-q5_0" for both the id and "/x/ggml-large-v3-turbo-q5_0.bin".
pub fn model_id(id_or_path: &str) -> String {
    let stem = Path::new(id_or_path).file_stem().and_then(|s| s.to_str()).unwrap_or(id_or_path);
    if id_or_path.ends_with(".bin") { stem.trim_start_matches("ggml-").to_string() } else { id_or_path.to_string() }
}

fn resolve(id_or_path: &str) -> Result<PathBuf, String> {
    let p = Path::new(id_or_path);
    if p.is_absolute() && p.is_file() {
        return Ok(p.to_path_buf());
    }
    model_file(id_or_path).ok_or_else(|| format!("Model '{id_or_path}' is not downloaded. Download it under Models."))
}

/// (total, available) in MB. ECHOTYPE_FAKE_RAM_MB (tests only) pretends the Mac has that much,
/// so the low-memory choices can be checked on a bigger one; available is capped to match.
pub fn ram_mb() -> (u64, u64) {
    let mut s = sysinfo::System::new();
    s.refresh_memory();
    #[cfg(target_os = "macos")]
    let avail = mac_available_bytes().unwrap_or(s.available_memory());
    #[cfg(not(target_os = "macos"))]
    let avail = s.available_memory();
    let (total, avail) = (s.total_memory() / 1_048_576, avail / 1_048_576);
    match std::env::var("ECHOTYPE_FAKE_RAM_MB").ok().and_then(|v| v.parse::<u64>().ok()).filter(|v| *v > 0) {
        Some(fake) => (fake, avail.min(fake)),
        None => (total, avail),
    }
}

/// free + inactive pages (speculative is inside free_count), what vm_stat users read as
/// reclaimable. sysinfo also subtracts compressor pages, which reports ~4 GB too little
/// on a busy Mac and would wrongly say medium does not fit.
#[cfg(target_os = "macos")]
fn mac_available_bytes() -> Option<u64> {
    // SAFETY: host_statistics64 fills a zeroed vm_statistics64 of the size given in `count`.
    unsafe {
        let mut stat: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT as libc::mach_msg_type_number_t;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        let ok = libc::host_statistics64(host, libc::HOST_VM_INFO64, &mut stat as *mut _ as *mut _, &mut count);
        let page = libc::sysconf(libc::_SC_PAGESIZE);
        (ok == libc::KERN_SUCCESS && page > 0).then(|| (stat.free_count as u64 + stat.inactive_count as u64) * page as u64)
    }
}

/// RAM a model needs once loaded, by catalog id; an unknown file is assumed to be large.
fn ram_need(model: &Path) -> u64 {
    ram_of(&model_id(&model.to_string_lossy()))
}

pub fn fits(ram_mb: u64, available_mb: u64) -> bool {
    ram_mb + HEADROOM_MB <= available_mb
}

#[derive(Serialize, Debug)]
pub struct ModelStatus {
    pub id: &'static str,
    pub label: &'static str,
    pub size_mb: u64,
    pub ram_mb: u64,
    pub english_only: bool,
    pub translates: bool,
    pub downloaded: bool,
    /// Fits in the memory free right now, with HEADROOM_MB to spare.
    pub fits: bool,
    /// What Automatic recommends for this Mac's total memory.
    pub recommended: bool,
    /// What Automatic uses right now (the recommendation, or the closest downloaded model).
    pub automatic: bool,
    /// The low-memory warning, when this model needs more than Automatic's pick.
    pub warning: Option<String>,
    pub note: &'static str,
}

pub fn list_models(total_mb: u64, available_mb: u64) -> Vec<ModelStatus> {
    list_models_with(total_mb, available_mb, |id| model_file(id).is_some())
}

fn list_models_with(total_mb: u64, available_mb: u64, downloaded: impl Fn(&str) -> bool) -> Vec<ModelStatus> {
    let auto = auto_model(total_mb, &downloaded);
    MODELS
        .iter()
        .map(|m| ModelStatus {
            id: m.id,
            label: m.label,
            size_mb: m.size_mb,
            ram_mb: ram_of(m.id),
            english_only: m.id.ends_with(".en"),
            translates: translates(m.id),
            downloaded: downloaded(m.id),
            fits: fits(ram_of(m.id), available_mb),
            recommended: m.id == recommended(total_mb),
            automatic: m.id == auto,
            warning: ram_warning(ram_of(m.id), ram_of(recommended(total_mb)), total_mb),
            note: m.note,
        })
        .collect()
}

/// Picks the local model for these settings. When translation is on and the chosen model
/// cannot translate, falls back to the best downloaded translating model that fits in RAM,
/// returning a warning that says so.
pub fn pick_local_model(s: &Settings) -> Result<(PathBuf, Option<String>), String> {
    let chosen = &local_model(s);
    if !s.translate_to_english || translates(&model_id(chosen)) {
        return Ok((resolve(chosen)?, None));
    }
    let (_, avail) = ram_mb();
    for m in MODELS.iter().filter(|m| translates(m.id) && fits(ram_of(m.id), avail)) {
        if let Some(p) = model_file(m.id) {
            let w = format!("{} cannot translate, so {} was used for this translation.", model_id(chosen), m.id);
            return Ok((p, Some(w)));
        }
    }
    // No offered model translates yet: type it as spoken rather than fail the dictation.
    Ok((resolve(chosen)?, Some("Translate to English is coming soon, so this was typed as spoken.".into())))
}

/// Whether a dictation can start right now, checked before the microphone opens: a local model
/// on disk (and a translating one when translation is on), or a key for a remote provider that needs one.
pub fn ready(s: &Settings) -> Result<(), String> {
    ready_with(s, |stage, provider| settings::provider_key(stage, provider).is_some())
}

fn ready_with(s: &Settings, has_key: impl Fn(&str, &str) -> bool) -> Result<(), String> {
    if s.stt.provider == "local" {
        resolve(&local_model(s)).map_err(|_| "Choose a speech model".to_string())?;
        return pick_local_model(s).map(|_| ());
    }
    match settings::preset(&s.stt.provider) {
        Some(p) if p.id == "cloud" && !has_key("stt", "cloud") => Err("Sign in to Echotype Cloud to dictate".into()),
        Some(p) if p.needs_key && !has_key("stt", p.id) => Err(format!("Add your {} API key to dictate", p.label)),
        _ => Ok(()),
    }
}

// ---------- download / delete ----------

pub fn download(id: &str, progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    let want = catalog(id)?.sha256;
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{HF_COMMIT}/ggml-{id}.bin");
    fetch(&url, &models_dir().join(format!("ggml-{id}.bin")), want, progress)?;
    let _ = crate::speakers::ensure_vad(); // 1 MB; dictation works without it, just less guarded
    let _ = ensure_lid(); // 32 MB; without it turbo picks the language itself, 260 ms slower
    Ok(())
}

/// Downloads `url` to `dest` through a .part file, kept only if its SHA-256 is `want`.
/// Downloads the user called off (onboarding's speed slider moved past a model mid-download);
/// fetch stops at its next chunk and deletes the .part file.
static CANCELLED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
pub const DOWNLOAD_CANCELLED: &str = "Download cancelled";

pub fn cancel_download(id: &str) {
    let dest = crate::llm::find(id).map_or_else(|| models_dir().join(format!("ggml-{id}.bin")), crate::llm::model_path);
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).push(dest);
}

/// True once, for a download that was called off.
fn take_cancel(dest: &Path) -> bool {
    let mut c = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
    let was = c.iter().any(|p| p == dest);
    c.retain(|p| p != dest);
    was
}

/// Tries per download; each one after the first resumes the .part file with an HTTP Range request.
const ATTEMPTS: u32 = 5;
const BACKOFF: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_secs(2) };

pub fn fetch(url: &str, dest: &Path, want: &str, mut progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    std::fs::create_dir_all(dest.parent().ok_or("bad model path")?).map_err(|e| e.to_string())?;
    take_cancel(dest); // a cancel from an earlier attempt does not stop this one
    let part = dest.with_extension("part");
    // reqwest's blocking timeout is per read: it catches a stalled connection without capping a slow download.
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let mut validator = None;
    let mut attempt = 1;
    let result = 'retry: loop {
        match fetch_part(&client, url, dest, &part, &mut validator, &mut progress) {
            Err((true, _)) if attempt < ATTEMPTS => {
                // 2, 4, 8, 16 s, still answering a cancel.
                let until = Instant::now() + BACKOFF * 2u32.pow(attempt - 1);
                while Instant::now() < until {
                    if take_cancel(dest) {
                        break 'retry Err(DOWNLOAD_CANCELLED.to_string());
                    }
                    std::thread::sleep(BACKOFF.min(Duration::from_millis(100)));
                }
                attempt += 1;
            }
            Err((true, e)) => return Err(e), // keeps the .part, so the next try resumes it
            Err((false, e)) => break Err(e),
            Ok(()) => break Ok(()),
        }
    };
    let result = result.and_then(|()| {
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut std::fs::File::open(&part).map_err(|e| e.to_string())?, &mut hasher).map_err(|e| e.to_string())?;
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err("Downloaded model failed its checksum and was discarded; try again".to_string());
        }
        Ok(())
    });
    match result {
        Ok(()) => std::fs::rename(&part, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
}

/// One request for the rest of `part`; Err((true, _)) is a network failure worth retrying.
/// `validator` (the ETag or Last-Modified seen last) makes the server send the whole file, a 200,
/// if it changed since; a server without ranges answers 200 too, and either way we start over.
// ponytail: the validator lives for one fetch, so a .part left by an earlier one resumes unchecked
// and a changed file fails the checksum; store it beside the .part if that ever bites.
fn fetch_part(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    part: &Path,
    validator: &mut Option<String>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<(), (bool, String)> {
    use reqwest::{header, StatusCode};
    let have = part.metadata().map_or(0, |m| m.len());
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(header::RANGE, format!("bytes={have}-"));
        if let Some(v) = validator.as_deref() {
            req = req.header(header::IF_RANGE, v);
        }
    }
    let mut resp = req.send().map_err(|e| (true, format!("Download failed: {e}")))?;
    let status = resp.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        let _ = std::fs::remove_file(part); // longer than the file: start over
        return Err((true, "Download failed: bad partial file".to_string()));
    }
    if !status.is_success() {
        let retry = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::REQUEST_TIMEOUT;
        return Err((retry, format!("Download failed: HTTP {status}")));
    }
    let get = |h| resp.headers().get(h).and_then(|v| v.to_str().ok()).map(str::to_string);
    let etag = get(header::ETAG).filter(|e| !e.starts_with("W/")); // If-Range takes only a strong ETag
    let seen = etag.or_else(|| get(header::LAST_MODIFIED));
    let resumed = status == StatusCode::PARTIAL_CONTENT && have > 0 && get(header::CONTENT_RANGE).is_some_and(|r| r.starts_with(&format!("bytes {have}-")));
    // R2 answers 206 whatever If-Range says, so a changed file shows only in a new validator.
    if resumed && validator.is_some() && seen.is_some() && seen != *validator {
        let _ = std::fs::remove_file(part);
        *validator = seen;
        return Err((true, "Download failed: the file changed".to_string()));
    }
    *validator = seen;
    let mut done = if resumed { have } else { 0 };
    let total = resp.content_length().map(|n| n + done);
    let mut f = std::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(part).map_err(|e| (false, e.to_string()))?;
    let mut buf = vec![0u8; 1 << 16];
    let mut last = Instant::now();
    progress(done, total);
    loop {
        let n = resp.read(&mut buf).map_err(|e| (true, format!("Download interrupted: {e}")))?;
        if n == 0 {
            break;
        }
        if take_cancel(dest) {
            return Err((false, DOWNLOAD_CANCELLED.to_string()));
        }
        f.write_all(&buf[..n]).map_err(|e| (false, e.to_string()))?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            progress(done, total);
            last = Instant::now();
        }
    }
    f.sync_all().map_err(|e| (false, e.to_string()))?;
    if total.is_some_and(|t| t != done) {
        return Err((true, "Download ended early; try again".to_string()));
    }
    progress(done, total);
    Ok(())
}

/// Deletes only files inside Echotype's own models dir.
pub fn delete(id: &str) -> Result<(), String> {
    catalog(id)?;
    let path = models_dir().join(format!("ggml-{id}.bin"));
    if !path.is_file() {
        return Err(match model_file(id) {
            Some(p) => format!("{} is not managed by Echotype; delete it yourself if you want it gone.", p.display()),
            None => format!("Model '{id}' is not downloaded"),
        });
    }
    unload_if(&path);
    std::fs::remove_file(&path).map_err(|e| e.to_string())
}

// ---------- local whisper ----------

// One loaded model + state, reused across calls (state creation costs ~0.5 s with Metal).
// ponytail: one model resident at a time; a translate run swaps turbo for medium and back. Keep two slots if users flip often.
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

/// Silero on its own, loaded on first use (1 MB, CPU): whisper.cpp's built-in VAD still
/// transcribes a clip it finds no speech in (silence came back "Thank you." with it on).
static SILERO: Mutex<Option<whisper_rs::WhisperVadContext>> = Mutex::new(None);

/// The language the last dictation came out in, when the user listens for several: dictation runs
/// in one language for a while, so a close call leans to it (likeliest). Process memory only.
static LAST_LANG: Mutex<Option<String>> = Mutex::new(None);

/// The speech Silero finds in the clip, as (start, end) seconds; empty when there is none. None
/// when the detector fails to load or run, so a broken file never swallows dictation.
pub fn speech(vad: &Path, pcm: &[f32]) -> Option<Vec<(f32, f32)>> {
    let mut g = SILERO.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        match whisper_rs::WhisperVadContext::new(vad.to_str()?, whisper_rs::WhisperVadContextParams::new()) {
            Ok(ctx) => *g = Some(ctx),
            Err(e) => {
                eprintln!("echotype: Silero did not load ({e}); transcribing without it");
                return None;
            }
        }
    }
    let ctx = g.as_mut().expect("loaded above");
    // ponytail: 0.3 (Silero's default is 0.5) from synthetic clips: macOS clicks and pops pass at
    // 0.2, a TTS whisper at -55 dBFS is cut at 0.4. Re-check on owner-recorded taps and whispers.
    let mut params = whisper_rs::WhisperVadParams::new();
    params.set_threshold(0.3);
    ctx.segments_from_samples(params, pcm).ok().map(|s| s.map(|s| (s.start / 100.0, s.end / 100.0)).collect())
}

/// Slack around Silero's speech, in seconds, before a whisper segment counts as outside it. In
/// TTS probes whisper's segment ends ran 0.2-0.35 s ahead of Silero's speech end, so 0.5 keeps a
/// real last segment whose start whisper places a little late.
const SPEECH_PAD_S: f32 = 0.5;

/// Whether a whisper segment [t0, t1] (seconds) touches the speech Silero heard. Whisper fills
/// silence with its training subtitles ("Thank you.", see SILERO); after the last word, or before
/// the first, that lands in a segment of its own outside the speech. No speech spans: keep it.
// ponytail: whole segments; an invented ending inside the last real segment survives. Token
// timestamps (as transcribe_words reads them) could cut there if one is seen.
fn heard(t0: f32, t1: f32, speech: &[(f32, f32)]) -> bool {
    let (Some(first), Some(last)) = (speech.first(), speech.last()) else { return true };
    t0 <= last.1 + SPEECH_PAD_S && t1 >= first.0 - SPEECH_PAD_S
}

struct Engine {
    path: PathBuf,
    ctx: WhisperContext,
    state: WhisperState,
    /// whisper.cpp keeps the Silero model inside the state once a call used it.
    vad: bool,
}

fn unload_if(path: &Path) {
    let mut g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|e| e.path == path) {
        *g = None;
    }
}

/// Frees the Silero speech detector loaded into the whisper state, keeping the model loaded.
/// Called when a meeting ends, however it ends; the next dictation loads it again (1 MB).
pub fn free_vad() {
    let mut g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = g.as_mut().filter(|e| e.vad) {
        match e.ctx.create_state() {
            Ok(state) => {
                e.state = state; // drops the old state, and the Silero context with it
                e.vad = false;
            }
            Err(_) => *g = None,
        }
    }
}

/// Logs a warning when a model sits on an external or network volume: macOS pages a model back
/// in from its file, which is slow (or gone, once unplugged) off the internal disk.
pub fn warn_if_not_internal(path: &Path) {
    #[cfg(target_os = "macos")]
    if let Some(why) = not_internal(path) {
        eprintln!("echotype: warning: {} is on {why}; keep models on the internal disk ({}) so they load fast", path.display(), models_dir().display());
    }
}

#[cfg(target_os = "macos")]
fn not_internal(path: &Path) -> Option<&'static str> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statfs fills a zeroed struct for a NUL-terminated path.
    let st = unsafe {
        let mut st: libc::statfs = std::mem::zeroed();
        (libc::statfs(c.as_ptr(), &mut st) == 0).then_some(st)?
    };
    let mount: Vec<u8> = st.f_mntonname.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect();
    if st.f_flags & libc::MNT_LOCAL as u32 == 0 {
        Some("a network volume")
    } else if mount.starts_with(b"/Volumes/") {
        Some("an external volume")
    } else {
        None
    }
}

/// Frees the loaded model. Must run before process exit: ggml-metal asserts at exit
/// if a context still holds GPU residency sets.
pub fn unload() {
    *ENGINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

pub fn quiet_whisper_logs() {
    whisper_rs::install_logging_hooks();
}

/// A few sentences of each language as people type it in English letters. Whisper writes in the
/// script of its prompt, so one of these first gets that language back in English letters
/// (Settings `script` "latin"): Hindi comes out as "Namaste, aap kaise hai?", not "नमस्ते". Only
/// languages checked with large-v3-turbo are here: their speech comes back in English letters (the
/// romanised_prompts test) and English clips decoded as them under their prompt stay English.
/// Telugu and Kannada romanised well but turned English into Dutch or mangled numbers; Tamil
/// looped; Arabic, Russian, Ukrainian, Greek and Japanese kept their own script.
pub const ROMANISED: &[(&str, &str)] = &[
    ("hi", "Namaste, aap kaise hain? Main theek hoon, shukriya. Kal milte hain."),
    ("mr", "Namaskar, tumhi kase aahat? Mi thik aahe, dhanyavaad. Udya bhetu."),
    ("ur", "Assalam alaikum, aap kaise hain? Main theek hoon, shukriya. Kal milte hain."),
    ("bn", "Nomoskar, apni kemon achhen? Ami bhalo achhi, dhonnobad. Kal dekha hobe."),
];

/// The user's ROMANISED languages when they want them in English letters ("latin"). A
/// translation is English whatever the prompt, so it has none.
fn romanised(s: &Settings) -> Vec<&'static (&'static str, &'static str)> {
    let latin = s.script == "latin" && !s.translate_to_english;
    ROMANISED.iter().filter(|(l, _)| latin && s.languages.iter().any(|x| x == l)).collect()
}

/// Dictations come back in English letters for one of the user's languages.
pub fn romanising(s: &Settings) -> bool {
    !romanised(s).is_empty()
}

/// Whisper prompt: the romanised sentences of the user's languages when they want English
/// letters, then the dictionary, which biases spelling of names at no latency cost. Whisper
/// copies the prompt's style, so it ends like a sentence: a bare comma list made it drop full
/// stops, which ran a meeting's sentences (and so its speakers) together.
pub fn prompt(s: &Settings) -> String {
    let mut parts: Vec<String> = romanised(s).iter().map(|(_, p)| p.to_string()).collect();
    let words: Vec<String> = s.dictionary.iter().map(|w| w.trim().replace('\0', "")).filter(|w| !w.is_empty()).collect();
    if !words.is_empty() {
        parts.push(format!("{}.", words.join(", ")));
    }
    parts.join(" ")
}

/// The languages whisper picks among: in English letters, only the romanised ones, so English and
/// mixed speech are decoded as Hindi (say) under its romanised prompt instead of being detected.
/// With [en, hi], detection called the owner's mixed "Hello aap kaise hai" English (0.37 to 0.13)
/// and whisper translated it to "Hello, how are you?"; as Hindi with the prompt it came back as
/// said, and his English clips stayed English word for word.
pub fn languages(s: &Settings) -> Vec<String> {
    let mine: Vec<String> = romanised(s).iter().map(|(l, _)| l.to_string()).collect();
    if mine.is_empty() { s.languages.clone() } else { mine }
}

pub fn norm(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Whisper stuck in a loop on noise ("the top of the top of the top of the top"): a long
/// sentence that is mostly the same few words again.
pub fn looped(text: &str) -> bool {
    let words: Vec<String> = text.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
    words.len() >= 8 && words.iter().collect::<std::collections::HashSet<_>>().len() * 2 <= words.len()
}

/// Longest phrase, in words, checked for a repeat.
// ponytail: a looped sentence longer than 10 words survives; raise it if one shows up.
const LOOP_MAX_WORDS: usize = 10;

/// Whisper repeating itself in dictation ("I'm a little, little, little.", "the the the the"):
/// three or more copies of a word or phrase in a row become one, ending in the last copy's
/// punctuation. Two copies ("very very") are speech, and so is a sentence that is one word said
/// over ("Hello, hello, hello." as a mic test, "No, no, no.") unless it is a long loop (looped).
pub fn collapse_loops(text: &str) -> String {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let keys: Vec<String> = toks.iter().map(|t| norm(t)).collect();
    let ends = |t: &str| t.ends_with(['.', '?', '!', '\u{0964}']); // । ends a Hindi sentence
    let stem = |t: &str| t.trim_end_matches(|c: char| !c.is_alphanumeric()).len();
    let (mut out, mut i, mut changed) = (Vec::<String>::new(), 0, false);
    while i < toks.len() {
        let run = (1..=LOOP_MAX_WORDS).find_map(|n| {
            let copy = |c: usize| (0..n).all(|j| keys.get(i + c * n + j).is_some_and(|k| !k.is_empty() && *k == keys[i + j]));
            let copies = 1 + (1..).take_while(|&c| copy(c)).count();
            (copies >= 3).then_some((n, i + n * copies))
        });
        let Some((n, end)) = run else {
            out.push(toks[i].to_string());
            i += 1;
            continue;
        };
        let sentence = (i == 0 || ends(toks[i - 1])) && (end == toks.len() || ends(toks[end - 1]));
        if n == 1 && sentence && !looped(&toks[i..end].join(" ")) {
            out.extend(toks[i..end].iter().map(|t| t.to_string()));
        } else {
            out.extend(toks[i..i + n - 1].iter().map(|t| t.to_string()));
            let (first, last) = (toks[i + n - 1], toks[end - 1]);
            out.push(format!("{}{}", &first[..stem(first)], &last[stem(last)..]));
            changed = true;
        }
        i = end;
    }
    if changed { out.join(" ") } else { text.to_string() }
}

/// Drops whisper's non-speech tags such as "[BLANK_AUDIO]" and trims.
pub fn clean_text(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `vad`: the Silero speech detector. With it, a clip with no speech in it (taps, a snap, the
/// room) comes back empty instead of whisper's "Thank you." or "I'm going to go to the next one."
/// Segments wholly outside Silero's speech are dropped (heard).
pub fn transcribe_local(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, vad: Option<&Path>) -> Result<String, String> {
    transcribe(model, pcm, languages, translate, prompt, vad, None)
}

/// The words so far while still recording (lib.rs partials): like transcribe_local, but it never
/// moves LAST_LANG, and whisper gives up as soon as `stop` is set, so the final run on release
/// gets the model at once instead of waiting for this one.
pub fn transcribe_partial(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, vad: Option<&Path>, stop: &AtomicBool) -> Result<String, String> {
    transcribe(model, pcm, languages, translate, prompt, vad, Some(stop))
}

/// `stop`: None for a dictation (sticky language, never aborted), Some for a partial.
fn transcribe(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, vad: Option<&Path>, stop: Option<&AtomicBool>) -> Result<String, String> {
    if pcm.is_empty() {
        return Ok(String::new());
    }
    let spans = vad.and_then(|v| speech(v, pcm));
    if spans.as_ref().is_some_and(Vec::is_empty) {
        return Ok(String::new());
    }
    run_local(model, pcm, languages, translate, prompt, false, stop.is_none(), vad, stop, |state| {
        let mut text = String::new();
        for i in 0..state.full_n_segments() {
            let Some(seg) = state.get_segment(i) else { continue };
            let (t0, t1) = (seg.start_timestamp() as f32 / 100.0, seg.end_timestamp() as f32 / 100.0);
            if spans.as_deref().is_none_or(|s| heard(t0, t1, s)) {
                text.push_str(&seg.to_str_lossy().map_err(|e| e.to_string())?);
            }
        }
        Ok(clean_text(&text))
    })
}

/// A recognised word with its time in seconds from the start of the clip it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub start: f32,
    pub end: f32,
    pub text: String,
}

/// Local whisper with token timestamps, grouped into words (a token that starts with a space
/// starts a word). Meeting notes use the times to cut overlapping chunks and to line words up
/// with speaker turns.
/// `vad`: a Silero model. A chunk with no speech in it gives no words, and whisper segments
/// before the first speech or after the last are dropped (as `heard` does for dictation).
/// whisper.cpp's own VAD never runs here (whisper-rs calls whisper_full_with_state, which skips
/// it), so a meeting's quiet stretches came back as "Thank you." and the like.
// ponytail: whole segments at a chunk's edges; something invented in a short gap between two
// speakers inside one chunk survives. Dropping words by their token times lost real words (they
// drift into a long gap), and joining the speech for whisper made soft speech read worse.
pub fn transcribe_words(model: &Path, pcm: &[f32], languages: &[String], prompt: &str, vad: Option<&Path>) -> Result<Vec<Word>, String> {
    if pcm.is_empty() {
        return Ok(vec![]);
    }
    let spans = vad.and_then(|v| speech(v, pcm));
    if spans.as_ref().is_some_and(Vec::is_empty) {
        return Ok(vec![]);
    }
    words_of(model, pcm, languages, prompt, vad, spans.as_deref())
}

fn words_of(model: &Path, pcm: &[f32], languages: &[String], prompt: &str, vad: Option<&Path>, spans: Option<&[(f32, f32)]>) -> Result<Vec<Word>, String> {
    run_local(model, pcm, languages, false, prompt, true, false, vad, None, |state| {
        let mut tokens: Vec<(Vec<u8>, f32, f32)> = vec![];
        for i in 0..state.full_n_segments() {
            let Some(seg) = state.get_segment(i) else { continue };
            let (t0, t1) = (seg.start_timestamp() as f32 / 100.0, seg.end_timestamp() as f32 / 100.0);
            if spans.is_some_and(|s| !heard(t0, t1, s)) {
                continue;
            }
            for j in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(j) else { continue };
                let d = tok.token_data();
                tokens.push((tok.to_bytes().map_err(|e| e.to_string())?.to_vec(), d.t0 as f32 / 100.0, d.t1 as f32 / 100.0));
            }
        }
        Ok(join_tokens(&tokens))
    })
}

/// Whisper tokens (bytes, start, end) → words: a token starting with a space starts a word. A
/// word's bytes are decoded together, since one Devanagari or emoji character can span two tokens
/// (decoding each token alone turned Hindi into "��हा��").
fn join_tokens(tokens: &[(Vec<u8>, f32, f32)]) -> Vec<Word> {
    let mut words: Vec<(Word, Vec<u8>)> = vec![];
    for (bytes, t0, t1) in tokens {
        if bytes.starts_with(b"[_") || bytes.starts_with(b"<|") {
            continue; // [_BEG_], [_TT_123], <|endoftext|>
        }
        match words.last_mut() {
            Some((w, b)) if !bytes.starts_with(b" ") => {
                b.extend_from_slice(bytes);
                w.end = w.end.max(*t1);
            }
            _ => words.push((Word { start: *t0, end: t1.max(*t0), text: String::new() }, bytes.clone())),
        }
    }
    // "[BLANK_AUDIO]" and "[Music]" arrive as ordinary tokens; a lone "." or "-" is noise.
    words
        .into_iter()
        .map(|(w, b)| Word { text: clean_text(String::from_utf8_lossy(&b).trim_start()), ..w })
        .filter(|w| w.text.chars().any(char::is_alphanumeric))
        .collect()
}

/// whisper tiny (multilingual, Q5_1, 32 MB), only to pick the language before turbo transcribes
/// with it set: 13 ms against turbo's own 273 ms detection pass on an M5, and as accurate
/// (FLEURS en/hi/de/es/fr, 30 clips each: 149/150 at full length and 115/150 on their first 3 s,
/// turbo 145 and 125; on 3 s of Hindi 18/30 against 15).
const LID_FILE: &str = "ggml-tiny-q5_1.bin";
const LID_SHA256: &str = "818710568da3ca15689e31a743197b520007872ff9576237bda97bd1b469c3d7";

pub fn lid_path() -> PathBuf {
    models_dir().join("lid").join(LID_FILE)
}

/// Fetches the language picker if it is missing; transcription works without it (turbo detects).
pub fn ensure_lid() -> Result<(), String> {
    if lid_path().is_file() {
        return Ok(());
    }
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/{HF_COMMIT}/{LID_FILE}");
    fetch(&url, &lid_path(), LID_SHA256, |_, _| {})
}

static LID: Mutex<Option<(WhisperContext, WhisperState)>> = Mutex::new(None);

/// The tiny model's language scores for the clip (whisper's language ids index them). None when
/// it is not downloaded or fails, and the caller falls back to turbo's own detection.
fn lid_probs(pcm: &[f32]) -> Option<Vec<f32>> {
    let mut g = LID.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let path = lid_path();
        let mut cp = WhisperContextParameters::default();
        cp.flash_attn(true);
        let ctx = WhisperContext::new_with_params(path.to_str()?, cp).ok()?;
        let state = ctx.create_state().ok()?;
        *g = Some((ctx, state));
    }
    let (_, state) = g.as_mut()?;
    state.pcm_to_mel(pcm, THREADS).ok()?;
    state.lang_detect(0, THREADS).ok().map(|(_, probs)| probs)
}

/// Another language's score that takes a clip away from the likely ones. Detecting among all 99 put
/// the owner's short English and Hinglish clips into Thai, Swedish and French; on FLEURS
/// (de/es/fr, 90 clips) every full-length clip clears 0.8, and 50 of 90 of their first 3 s do.
const OTHER_LANGUAGE_P: f32 = 0.8;

/// A language to offer the user (Settings.language_offer) after a dictation of 3 s or more: one
/// the tiny model is OTHER_LANGUAGE_P sure of, not in their languages and not offered before.
/// Never with auto-detect, which already types every language, or with 3 picked (the most).
// ponytail: runs the tiny model (32 MB, kept loaded) for one-language users too, after the paste;
// sample every few dictations if its memory or 13 ms matter.
pub fn language_to_offer(s: &Settings, pcm: &[f32]) -> Option<String> {
    let rate = crate::audio::RATE as usize;
    if s.languages.is_empty() || s.languages.len() >= 3 || pcm.len() < 3 * rate || !lid_path().is_file() {
        return None;
    }
    offer(&lid_probs(&pcm[..pcm.len().min(30 * rate)])?, &s.languages, &s.dismissed)
}

fn offer(probs: &[f32], languages: &[String], dismissed: &[String]) -> Option<String> {
    let top = (0..probs.len()).max_by(|&a, &b| probs[a].total_cmp(&probs[b]))?;
    let code = whisper_rs::get_lang_str(top as i32)?;
    let new = probs[top] >= OTHER_LANGUAGE_P && !languages.iter().any(|l| l == code) && !dismissed.contains(&format!("lang_offer:{code}"));
    new.then(|| code.to_string())
}

/// "Detect any language": the likeliest of `likely` (and the last language, `last`), unless another
/// language scores OTHER_LANGUAGE_P or more.
fn auto_pick(probs: &[f32], likely: &[String], last: Option<&str>) -> Option<String> {
    let top = (0..probs.len()).max_by(|&a, &b| probs[a].total_cmp(&probs[b]))?;
    let top_code = whisper_rs::get_lang_str(top as i32)?;
    if likely.iter().any(|l| l == top_code) || probs[top] >= OTHER_LANGUAGE_P || likely.is_empty() {
        return Some(top_code.to_string());
    }
    likeliest(probs, likely, last)
}

/// The languages this user likely speaks: the Mac's preferred languages, English, and the region's
/// main other language where Echotype's users speak it mixed with English (India: Hindi).
// ponytail: macOS only, one region rule; other platforms and regions fall back to English.
fn likely_languages() -> &'static [String] {
    static LIKELY: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIKELY.get_or_init(|| {
        let read = |key: &str| std::process::Command::new("/usr/bin/defaults").args(["read", "-g", key]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        likely_from(&read("AppleLanguages"), &read("AppleLocale"))
    })
}

/// `likely_languages` from `defaults read -g AppleLanguages` / `AppleLocale` output.
fn likely_from(apple_languages: &str, apple_locale: &str) -> Vec<String> {
    let mut out = vec!["en".to_string()];
    let codes = apple_languages.split(['"', ',', '(', ')', '\n', ' ']).filter(|c| c.len() >= 2).map(|c| c[..2].to_lowercase());
    for c in codes.chain((apple_locale.contains("_IN") || apple_locale.contains("rg=in")).then(|| "hi".to_string())) {
        if whisper_rs::get_lang_id(&c).is_some() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// whisper.cpp's own thread count (min(4, cores)), for the language check before a run.
const THREADS: usize = 4;

/// How many times `last`'s score another language must beat to take over from it. Measured with
/// en+hi on 40 FLEURS English and 40 Hindi clips: full-length clips never change (the loser's share
/// of the pair stays under 0.2); on their first 2 s (26/40 Hindi right), 2x fixes 1 or breaks 2
/// depending on `last`, 4x fixes 3 or breaks 7. Hinglish "Report bhej do, main dekh lunga" scores
/// hi 0.44 / en 0.34: a coin toss that 2x settles to the language already in use.
const LEAN: f32 = 2.0;

/// The likeliest of `codes` by whisper's language scores; the first one on a tie or an unknown
/// code. `last` (one of `codes`, the previous dictation's) wins unless beaten LEAN times over.
fn likeliest(probs: &[f32], codes: &[String], last: Option<&str>) -> Option<String> {
    let score = |c: &str| whisper_rs::get_lang_id(c).and_then(|i| probs.get(i as usize).copied()).unwrap_or(f32::MIN);
    let mut best = codes.first()?;
    for c in &codes[1..] {
        if score(c) > score(best) {
            best = c;
        }
    }
    match last.and_then(|l| codes.iter().find(|c| *c == l)) {
        Some(l) if score(l) * LEAN >= score(best) => Some(l.clone()),
        _ => Some(best.clone()),
    }
}

/// whisper.cpp's abort callback: `data` is the &AtomicBool run_local was given.
unsafe extern "C" fn stop_set(data: *mut std::ffi::c_void) -> bool {
    // SAFETY: run_local passes a pointer to an AtomicBool that outlives the whisper_full call.
    unsafe { (*(data as *const AtomicBool)).load(Ordering::SeqCst) }
}

/// `sticky`: lean to, and remember, the language of the last such run (LAST_LANG); dictation only.
/// `stop`: whisper aborts (an Err) once it is set.
#[allow(clippy::too_many_arguments)]
fn run_local<T>(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, timestamps: bool, sticky: bool, vad: Option<&Path>, stop: Option<&AtomicBool>, read: impl FnOnce(&WhisperState) -> Result<T, String>) -> Result<T, String> {
    let mut g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_none_or(|e| e.path != model) {
        *g = None; // free the old model before loading the next one
        // No headroom here (that is for recommendations): refuse only what would push the machine into swap.
        let (need, free) = (ram_need(model), ram_mb().1);
        if need > free {
            return Err(format!("Not enough free memory for {}: it needs about {need} MB and {free} MB is free. Close some apps or pick a smaller model.", model_id(&model.to_string_lossy())));
        }
        let path = model.to_str().ok_or("Model path is not valid UTF-8")?;
        warn_if_not_internal(model);
        let mut cp = WhisperContextParameters::default();
        cp.flash_attn(true); // whisper.cpp's own CLI default; faster on Metal
        let ctx = WhisperContext::new_with_params(path, cp).map_err(|e| format!("{} could not load: {e}", model_id(&model.to_string_lossy())))?;
        let state = ctx.create_state().map_err(|e| format!("Could not start whisper: {e}"))?;
        *g = Some(Engine { path: model.to_path_buf(), ctx, state, vad: false });
    }
    let engine = g.as_mut().expect("loaded above");
    engine.vad |= vad.is_some();
    let state = &mut engine.state;
    let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let multilingual = engine.ctx.is_multilingual();
    let tiny = (languages.len() != 1 && multilingual).then(|| lid_probs(pcm)).flatten();
    let lang = match languages {
        [one] => Some(one.replace('\0', "")),
        _ if !multilingual => None, // an English-only model: nothing to pick
        [] => match &tiny {
            // The tiny model's pick, so turbo skips its own detection pass (273 ms on an M5), among
            // the languages this user is likely to speak unless another is clearly what was said.
            Some(probs) => {
                let mut last = LAST_LANG.lock().unwrap_or_else(|e| e.into_inner());
                let mut likely = likely_languages().to_vec();
                if let Some(l) = last.as_ref().filter(|l| sticky && !likely.contains(l)) {
                    likely.push(l.clone());
                }
                let pick = auto_pick(probs, &likely, last.as_deref().filter(|_| sticky));
                if sticky {
                    last.clone_from(&pick);
                }
                pick
            }
            None => None, // whisper detects among all 99
        },
        several if tiny.is_some() => {
            let mut last = LAST_LANG.lock().unwrap_or_else(|e| e.into_inner());
            let pick = likeliest(tiny.as_deref().unwrap_or_default(), several, last.as_deref().filter(|_| sticky));
            if sticky {
                last.clone_from(&pick);
            }
            pick
        }
        several => {
            // Whisper's own detection, scored over the first 30 s, read only for the user's languages:
            // a short Hindi clip once came out as Tibetan under auto.
            state.pcm_to_mel(pcm, THREADS).map_err(|e| format!("Language detection failed: {e}"))?;
            let (_, probs) = state.lang_detect(0, THREADS).map_err(|e| format!("Language detection failed: {e}"))?;
            let mut last = LAST_LANG.lock().unwrap_or_else(|e| e.into_inner());
            let pick = likeliest(&probs, several, last.as_deref().filter(|_| sticky));
            if sticky {
                last.clone_from(&pick);
            }
            pick
        }
    };
    p.set_language(lang.as_deref());
    p.set_translate(translate);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_special(false);
    p.set_print_timestamps(false);
    p.set_token_timestamps(timestamps);
    if !prompt.is_empty() {
        p.set_initial_prompt(prompt);
    }
    let vad = vad.and_then(Path::to_str);
    if vad.is_some() {
        p.set_vad_model_path(vad); // before enable_vad, which panics without it
        p.enable_vad(true);
        let mut v = whisper_rs::WhisperVadParams::new();
        v.set_speech_pad(200); // keep word edges; the default 30 ms clips first and last sounds
        p.set_vad_params(v);
    }
    if let Some(stop) = stop {
        // A partial runs whisper's full 30 s window like the final text. Sizing the encoder to the
        // clip (set_audio_ctx) was 3x cheaper but turbo answers it with garbage ("itcks", "TH" for
        // the start of "However, due to the slow communication channels"), as it did for dictation.
        // The raw setters: set_abort_callback_safe's trampoline reads its boxed closure as the wrong type.
        // SAFETY: `stop` is borrowed for this whole function, so it outlives the state.full call below.
        unsafe {
            p.set_abort_callback(Some(stop_set));
            p.set_abort_callback_user_data(stop as *const AtomicBool as *mut std::ffi::c_void);
        }
    }
    state.full(p, pcm).map_err(|e| format!("Transcription failed: {e}"))?;
    read(state)
}

/// Loads the model and runs one second of silence through it so the first real
/// dictation does not pay for model load and Metal kernel compilation.
pub fn warm_up(s: &Settings) {
    if s.stt.provider != "local" {
        return;
    }
    let Ok((path, _)) = pick_local_model(s) else { return };
    if !fits(ram_need(&path), ram_mb().1) {
        eprintln!("echotype: not warming up {}: not enough free RAM", path.display());
        return;
    }
    let t = Instant::now();
    match transcribe_local(&path, &vec![0.0; 16_000], &["en".into()], false, "", None) {
        Ok(_) => eprintln!("echotype: warmed up {} in {} ms", path.display(), t.elapsed().as_millis()),
        Err(e) => eprintln!("echotype: warm-up failed: {e}"),
    }
}

// ---------- remote (OpenAI-compatible) ----------

pub fn transcribe_remote(s: &Settings, pcm: &[f32], prompt: &str) -> Result<String, String> {
    let provider = s.stt.provider.as_str();
    if settings::preset(provider).is_some_and(|p| !p.stt) {
        return Err(format!("{provider} does not offer speech to text; pick another STT provider"));
    }
    let base = settings::base_url(provider, &s.stt.base_url)?;
    let model = if s.stt.model.trim().is_empty() { settings::preset(provider).map(|p| p.default_stt_model).unwrap_or_default().to_string() } else { s.stt.model.trim().to_string() };
    // OpenAI's translation endpoint returns English; it takes no language.
    let endpoint = if s.translate_to_english { "audio/translations" } else { "audio/transcriptions" };
    let file = reqwest::blocking::multipart::Part::bytes(crate::audio::wav_bytes(pcm)).file_name("audio.wav").mime_str("audio/wav").map_err(|e| e.to_string())?;
    let mut form = reqwest::blocking::multipart::Form::new().part("file", file).text("model", model).text("response_format", "json");
    // An upload names at most one language: with two or three the provider detects among all.
    if let (false, [one]) = (s.translate_to_english, languages(s).as_slice()) {
        form = form.text("language", one.clone());
    }
    if !prompt.is_empty() {
        form = form.text("prompt", prompt.to_string());
    }
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())?;
    let mut req = client.post(format!("{base}/{endpoint}")).multipart(form);
    if let Some(key) = settings::provider_key("stt", provider) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().map_err(|e| format!("Speech-to-text request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        if provider == "cloud" {
            return Err(crate::cloud::error_message(status.as_u16(), &body)); // e.g. 402: this week's words are used up
        }
        return Err(format!("Speech-to-text failed ({status}): {}", body.chars().take(300).collect::<String>()));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| format!("Unexpected speech-to-text response: {}", body.chars().take(200).collect::<String>()))?;
    Ok(v["text"].as_str().map(clean_text).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each ROMANISED language spoken, in both scripts: native comes back in its own script, English
    /// letters with no other. ECHOTYPE_TEST_MODEL=<large-v3-turbo> ECHOTYPE_TEST_CLIPS=<folder of
    /// <code>.wav, e.g. `say -v Lekha -o hi.aiff "…"` then 16 kHz mono>
    /// `cargo test --release romanised_prompts -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn romanised_prompts() {
        let (Ok(model), Ok(dir)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_CLIPS")) else { return eprintln!("skipped") };
        for (code, _) in ROMANISED {
            let Ok(wav) = std::fs::read(Path::new(&dir).join(format!("{code}.wav"))) else { eprintln!("{code}: no clip"); continue };
            let pcm = crate::audio::decode(wav, Some("wav")).unwrap();
            let mut s = Settings { languages: vec![code.to_string()], ..Default::default() };
            let run = |s: &Settings| transcribe_local(Path::new(&model), &pcm, &languages(s), false, &prompt(s), None).unwrap();
            let native = run(&s);
            s.script = "latin".into();
            let latin = run(&s);
            s.languages.insert(0, "en".into());
            let mixed = run(&s);
            eprintln!("{code} native: {native}\n{code} latin: {latin}\n{code} en+{code} latin: {mixed}");
            assert!(native.chars().any(|c| c.is_alphabetic() && !c.is_ascii()), "{code} in its own script: {native:?}");
            for t in [&latin, &mixed] {
                assert!(t.chars().all(|c| !c.is_alphabetic() || c.is_ascii()) && t.chars().any(char::is_alphabetic), "{code} in English letters: {t:?}");
            }
        }
    }

    /// A quiet room with a knock every 600 ms (owner report: tapping the laptop typed "Thank you.",
    /// a finger snap "I'm going to go to the next one."), through the real model: with Silero,
    /// nothing comes back; speech still does.
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_WAV=<speech> [ECHOTYPE_TEST_NOISE=<wav:wav>] cargo test --release no_speech_no_text -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn no_speech_no_text() {
        let Ok(model) = std::env::var("ECHOTYPE_TEST_MODEL") else { return eprintln!("skipped: no ECHOTYPE_TEST_MODEL") };
        let (model, vad) = (Path::new(&model), crate::speakers::vad().expect("Silero downloaded (speakers::ensure_vad)"));
        let pcm = knocks(8);
        // Text with no letter or digit ("." or "...") is no speech to the app (cleanup::has_speech).
        let nothing = |pcm: &[f32], what: &str| {
            eprintln!("{what} without Silero: {:?}", transcribe_local(model, pcm, &[], false, "", None));
            let t = transcribe_local(model, pcm, &[], false, "", Some(&vad)).unwrap();
            assert!(!crate::cleanup::has_speech(&t), "{what} is not speech: {t:?}");
        };
        nothing(&pcm, "knocks");
        nothing(&vec![0.0; 16_000 * 5], "silence");
        // Recorded non-speech: taps, clicks, a snap (`ECHOTYPE_TEST_NOISE=a.wav:b.wav`).
        for f in std::env::var("ECHOTYPE_TEST_NOISE").unwrap_or_default().split(':').filter(|f| !f.is_empty()) {
            nothing(&crate::audio::decode(std::fs::read(f).unwrap(), Some("wav")).unwrap(), f);
        }
        if let Ok(wav) = std::env::var("ECHOTYPE_TEST_WAV") {
            let speech = crate::audio::decode(std::fs::read(&wav).unwrap(), Some("wav")).unwrap();
            let t = Instant::now();
            assert!(super::speech(&vad, &speech).is_some_and(|s| !s.is_empty()));
            eprintln!("Silero on {:.1} s of speech: {} ms", speech.len() as f32 / 16_000.0, t.elapsed().as_millis());
            let text = transcribe_local(model, &speech, &[], false, "", Some(&vad)).unwrap();
            eprintln!("speech with Silero: {text}");
            assert!(text.split_whitespace().count() >= 3, "speech still comes through: {text:?}");
        }
    }

    /// What a partial costs on 5, 10 and 20 s of speech, and how soon a stopped one hands the
    /// model back to the final run.
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_WAV=<16 kHz speech wav> cargo test --release partial_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn partial_timing() {
        let (Ok(model), Ok(wav)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_WAV")) else { return eprintln!("skipped: no ECHOTYPE_TEST_MODEL / ECHOTYPE_TEST_WAV") };
        let (model, vad) = (Path::new(&model), crate::speakers::vad());
        let speech = crate::audio::decode(std::fs::read(&wav).unwrap(), Some("wav")).unwrap();
        let clip = |secs: usize| speech.iter().copied().cycle().take(16_000 * secs).collect::<Vec<f32>>();
        let (go, en) = (AtomicBool::new(false), ["en".to_string()]);
        transcribe_partial(model, &clip(1), &en, false, "", vad.as_deref(), &go).unwrap(); // load + Metal warm-up
        for secs in [5, 10, 20] {
            let pcm = clip(secs);
            let runs: Vec<u128> = (0..3)
                .map(|_| {
                    let t = Instant::now();
                    transcribe_partial(model, &pcm, &en, false, "", vad.as_deref(), &go).unwrap();
                    t.elapsed().as_millis()
                })
                .collect();
            eprintln!("partial on {secs} s: {runs:?} ms; {:?}", transcribe_partial(model, &pcm, &en, false, "", vad.as_deref(), &go).unwrap());
        }
        // Released mid-partial: the final run waits only for the abort.
        let pcm = clip(crate::PARTIAL_TAIL.as_secs() as usize);
        for after in [10, 40, 70] {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let (m, p, s, v, en) = (model.to_path_buf(), pcm.clone(), stop.clone(), vad.clone(), en.clone());
            let partial = std::thread::spawn(move || {
                let r = transcribe_partial(&m, &p, &en, false, "", v.as_deref(), &s);
                (r.is_err(), Instant::now())
            });
            std::thread::sleep(Duration::from_millis(after));
            let t = Instant::now();
            stop.store(true, Ordering::SeqCst);
            let (aborted, done) = partial.join().unwrap();
            eprintln!("stopped {after} ms into a partial on {} s: model free {} ms later (aborted: {aborted})", pcm.len() / 16_000, done.saturating_duration_since(t).as_millis());
        }
        let final_text = transcribe_local(model, &clip(20), &["en".into()], false, "", vad.as_deref()).unwrap();
        unload(); // ggml-metal asserts at exit while a model is resident
        assert!(final_text.contains("country"), "the final run is unharmed by the aborted one: {final_text}");
    }

    /// `secs` of a quiet room with a knock every 600 ms.
    fn knocks(secs: usize) -> Vec<f32> {
        let mut seed = 1u32;
        let mut pcm: Vec<f32> = (0..16_000 * secs)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.002 - 0.001 // about -60 dBFS of hiss
            })
            .collect();
        for knock in (4_000..pcm.len() - 800).step_by(9_600) {
            for (i, x) in pcm[knock..knock + 480].iter_mut().enumerate() {
                *x += 0.6 * (-(i as f32) / 60.0).exp() * if i % 2 == 0 { 1.0 } else { -1.0 };
            }
        }
        pcm
    }

    /// Speech, then knocks until the hotkey is let go 40 s later, through the real model. Without
    /// trimming whisper invented "Yes, that works. Thank you." and "Send the report to Priya by the
    /// end of the day. The report to Priya by the end of the day is the end of the day."; trimmed to
    /// Silero's speech, the clip reads as the speech alone. (3-8 s of silence, hiss, brown noise,
    /// clicks or knocks after 9 TTS clips, 84 runs, never made it invent anything.)
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_WAV=<speech> cargo test --release speech_then_knocks -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn speech_then_knocks_keeps_only_the_speech() {
        let (Ok(model), Ok(wav)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_WAV")) else { return eprintln!("skipped") };
        let (model, vad) = (Path::new(&model), crate::speakers::vad().expect("Silero downloaded (speakers::ensure_vad)"));
        let mut pcm = knocks(1)[..8_000].to_vec();
        pcm.extend(crate::audio::decode(std::fs::read(&wav).unwrap(), Some("wav")).unwrap());
        let alone = transcribe_local(model, &pcm, &[], false, "", Some(&vad)).unwrap();
        pcm.extend(knocks(40));
        let all = transcribe_local(model, &pcm, &[], false, "", None).unwrap();
        let kept = transcribe_local(model, &pcm, &[], false, "", Some(&vad)).unwrap();
        eprintln!("speech alone: {alone}\nuntrimmed:    {all}\ntrimmed:      {kept}");
        assert!(alone.split_whitespace().count() >= 3, "speech comes through: {alone:?}");
        // Whisper words the same speech a little differently in a longer clip ("5" / "5:00"), so
        // the check is that nothing was added, not that the text is identical.
        let words = |t: &str| t.split_whitespace().count();
        assert!(words(&kept) <= words(&alone) + 2, "only the speech is kept: {kept:?} vs {alone:?}");
    }

    /// Meeting chunks through the real model: two speakers with 12 s of knocks between keep every
    /// word; a chunk of knocks gives none; speech then 20 s of knocks keeps nothing after the speech.
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_WAV=<speech> [ECHOTYPE_TEST_WAV2=<other speech>] cargo test --release meeting_chunk_keeps_only_speech -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn meeting_chunk_keeps_only_speech() {
        let (Ok(model), Ok(wav)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_WAV")) else { return eprintln!("skipped") };
        let (model, vad) = (Path::new(&model), crate::speakers::vad().expect("Silero downloaded (speakers::ensure_vad)"));
        let read = |f: &str| crate::audio::decode(std::fs::read(f).unwrap(), Some("wav")).unwrap();
        let (one, two) = (read(&wav), read(&std::env::var("ECHOTYPE_TEST_WAV2").unwrap_or(wav.clone())));
        let text = |w: &[Word]| w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
        let words = |pcm: &[f32], vad: Option<&Path>| transcribe_words(model, pcm, &[], "", vad).unwrap();

        let mut both = one.clone();
        both.extend(knocks(12));
        both.extend(&two);
        let (all, kept) = (words(&both, None), words(&both, Some(&vad)));
        eprintln!("two speakers, without Silero: {}\n              with Silero:    {}", text(&all), text(&kept));
        assert!(kept.len() + 2 >= all.len(), "no real word lost: {} of {}", kept.len(), all.len());

        assert!(words(&knocks(10), Some(&vad)).is_empty(), "a chunk of knocks gives no words");

        let mut tail = one.clone();
        tail.extend(knocks(20));
        let end = one.len() as f32 / 16_000.0 + 1.0;
        let (all, kept) = (words(&tail, None), words(&tail, Some(&vad)));
        eprintln!("speech then knocks, without: {}\n                    with:    {}", text(&all), text(&kept));
        assert!(kept.iter().all(|w| w.start < end), "nothing after the speech: {kept:?}");
    }

    #[test]
    fn segments_outside_the_speech_are_dropped() {
        let speech = [(1.0, 2.0), (3.0, 5.8)];
        assert!(heard(0.0, 5.4, &speech), "whisper's first segment starts at 0 s");
        assert!(heard(2.1, 2.9, &speech), "a pause between spans is not trimmed");
        assert!(heard(6.2, 7.0, &speech), "within the pad after the last word");
        assert!(!heard(6.4, 8.0, &speech), "\"Thank you.\" in the silence after");
        assert!(!heard(0.0, 0.4, &speech), "\"Thank you.\" over the taps before the first word");
        assert!(heard(0.0, 0.6, &speech), "within the pad before it");
        assert!(heard(9.0, 9.5, &[]), "no spans: keep everything");
    }

    #[test]
    fn three_repeats_collapse_and_speech_survives() {
        let c = collapse_loops;
        assert_eq!(c("- I'm a little, little, little."), "- I'm a little."); // owner history
        assert_eq!(c("so the the the the plan is"), "so the plan is");
        assert_eq!(c("The, the, the meeting"), "The meeting");
        assert_eq!(c("We went to the top of the top of the top of the hill."), "We went to the top of the hill.");
        assert_eq!(c("Ship it. I said ship it. I said ship it. I said ship it."), "Ship it. I said ship it.");
        assert_eq!(c("the the the the the the the the the the"), "the", "a whole-text loop collapses too");
        // Speech: doubles, a sentence that is one word said over, words that only look alike.
        for keep in ["It was very very good.", "Hello, hello, hello. Mic test, hello.", "No, no, no.", "It is what it is.", "1 1 2 3 5 8", "line\nbreaks stay"] {
            assert_eq!(c(keep), keep);
        }
        // Devanagari: the vowel signs are letters (Other_Alphabetic), so का की के stay distinct.
        assert_eq!(c("मैं बहुत बहुत बहुत खुश हूँ।"), "मैं बहुत खुश हूँ।");
        assert_eq!(c("यह का की के लिए है"), "यह का की के लिए है");
        assert_eq!(c("वो बहुत बहुत अच्छा है।"), "वो बहुत बहुत अच्छा है।");
        assert_eq!(c("नहीं, नहीं, नहीं।"), "नहीं, नहीं, नहीं।");
    }

    /// A Hindi clip with the user's languages set to English + Hindi comes out in Devanagari.
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_HINDI=<hindi wav> cargo test --release hindi_among_mine -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hindi_among_mine() {
        let (Ok(model), Ok(wav)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_HINDI")) else { return eprintln!("skipped") };
        let pcm = crate::audio::decode(std::fs::read(&wav).unwrap(), Some("wav")).unwrap();
        let mine = ["en".to_string(), "hi".to_string()];
        let text = transcribe_local(Path::new(&model), &pcm, &mine, false, "", None).unwrap();
        eprintln!("en+hi: {text}");
        assert!(text.chars().any(|c| ('\u{0900}'..='\u{097F}').contains(&c)), "Devanagari expected: {text:?}");
    }

    #[test]
    fn a_character_split_across_tokens_stays_whole() {
        let ha = "हाँ".as_bytes(); // 9 bytes: whisper can cut it mid-character
        let toks = vec![(b" ".iter().chain(&ha[..4]).copied().collect::<Vec<u8>>(), 0.0, 0.2), (ha[4..].to_vec(), 0.2, 0.4), (b" ok".to_vec(), 0.5, 0.7), (b"[_TT_50]".to_vec(), 0.7, 0.7)];
        let w = join_tokens(&toks);
        assert_eq!(w.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(), ["हाँ", "ok"]);
        assert_eq!((w[0].start, w[0].end), (0.0, 0.4));
    }

    #[test]
    fn a_cancelled_download_stops_once() {
        let dest = models_dir().join("ggml-cancel-test.bin");
        cancel_download("cancel-test");
        assert!(take_cancel(&dest) && !take_cancel(&dest), "a cancel stops one download, not the next");
        assert_eq!(MODELS.iter().map(|m| m.id).collect::<Vec<_>>(), ["large-v3-turbo-q5_0", "tiny.en"], "Automatic's pick and the practice model");
    }

    /// A 127.0.0.1 server whose first reply sends half of `body` and drops the connection. Its second
    /// honours the Range header ("206"), ignores it ("200"), or sends a 206 of a file with a new
    /// ETag ("changed", then a 200 on a third). Returns the URL and the requests it saw.
    fn flaky_server(body: Vec<u8>, second: &'static str) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for i in 0..if second == "changed" { 3 } else { 2 } {
                let (mut sock, _) = listener.accept().unwrap();
                let mut req = String::new();
                let mut r = std::io::BufReader::new(sock.try_clone().unwrap());
                while r.read_line(&mut req).unwrap() > 2 {}
                let from = req.lines().find_map(|l| l.to_lowercase().strip_prefix("range: bytes=")?.trim_end_matches('-').parse::<usize>().ok());
                let reply = match (i, from) {
                    (0, _) => [format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"v1\"\r\n\r\n", body.len()).into_bytes(), body[..body.len() / 2].to_vec()].concat(),
                    (1, Some(a)) if second != "200" => {
                        // The changed file's bytes differ, so accepting them would fail the checksum.
                        let (etag, rest) = if second == "206" { ("v1", body[a..].to_vec()) } else { ("v2", vec![0; body.len() - a]) };
                        [format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{}/{}\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n", body.len() - a, body.len() - 1, body.len()).into_bytes(), rest].concat()
                    }
                    _ => [format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes(), body.clone()].concat(),
                };
                let _ = sock.write_all(&reply); // the client may have hung up (the cancel test)
                seen.push(req);
            }
            seen
        });
        (url, server)
    }

    fn sha256_hex(b: &[u8]) -> String {
        sha2::Sha256::digest(b).iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_dropped_download_resumes_or_starts_over() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        let dir = std::env::temp_dir().join(format!("echotype-fetch-test-{}", std::process::id()));
        for second in ["206", "200", "changed"] {
            let (url, server) = flaky_server(body.clone(), second);
            let dest = dir.join(format!("resume-{second}.bin"));
            let mut last = (0, None);
            fetch(&url, &dest, &sha256_hex(&body), |d, t| last = (d, t)).unwrap();
            let seen: Vec<String> = server.join().unwrap().iter().map(|r| r.to_lowercase()).collect();
            assert_eq!(std::fs::read(&dest).unwrap(), body, "{second}");
            assert!(!dest.with_extension("part").exists());
            assert_eq!(last, (body.len() as u64, Some(body.len() as u64)), "progress counts the resumed bytes");
            assert!(seen[1].contains(&format!("range: bytes={}-", body.len() / 2)) && seen[1].contains("if-range: \"v1\""), "{}", seen[1]);
            assert!(seen.get(2).is_none_or(|r| !r.contains("range:")), "a changed file starts over");
        }

        // A cancel after the resume still deletes the .part; a bad checksum discards the file.
        let (url, server) = flaky_server(body.clone(), "206");
        let dest = dir.join("cancel.bin");
        let err = fetch(&url, &dest, &sha256_hex(&body), |d, _| {
            if d > 0 {
                CANCELLED.lock().unwrap().push(dest.clone());
            }
        });
        assert_eq!(err.unwrap_err(), DOWNLOAD_CANCELLED);
        assert!(!dest.with_extension("part").exists() && !dest.exists());
        server.join().unwrap();
        let (url, server) = flaky_server(body.clone(), "206");
        assert!(fetch(&url, &dest, &"0".repeat(64), |_, _| {}).unwrap_err().contains("checksum"));
        assert!(!dest.with_extension("part").exists() && !dest.exists());
        server.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detect_any_stays_with_the_users_languages_unless_another_is_clear() {
        assert_eq!(likely_from("(\n    \"en-US\",\n    \"en-IN\"\n)", "en_US@rg=inzzzz"), ["en", "hi"], "the owner's Mac");
        assert_eq!(likely_from("(\"de-DE\", \"fr-FR\")", "de_DE"), ["en", "de", "fr"]);
        assert_eq!(likely_from("", ""), ["en"]);
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        let id = |c: &str| whisper_rs::get_lang_id(c).unwrap() as usize;
        let likely = ["en".to_string(), "hi".to_string()];
        probs[id("th")] = 0.45; // a short Hinglish clip once came out in Thai
        probs[id("hi")] = 0.30;
        probs[id("en")] = 0.20;
        assert_eq!(auto_pick(&probs, &likely, None).as_deref(), Some("hi"));
        probs[id("th")] = 0.85;
        assert_eq!(auto_pick(&probs, &likely, None).as_deref(), Some("th"), "clearly Thai is Thai");
        probs[id("th")] = 0.0;
        probs[id("en")] = 0.6;
        assert_eq!(auto_pick(&probs, &likely, None).as_deref(), Some("en"));
    }

    #[test]
    fn a_clear_language_the_user_has_not_picked_is_offered_once() {
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        let id = |c: &str| whisper_rs::get_lang_id(c).unwrap() as usize;
        let (en, none) = (vec!["en".to_string()], Vec::<String>::new());
        probs[id("hi")] = 0.9;
        assert_eq!(offer(&probs, &en, &none).as_deref(), Some("hi"));
        assert_eq!(offer(&probs, &en, &["lang_offer:hi".to_string()]), None, "never twice");
        assert_eq!(offer(&probs, &["en".into(), "hi".into()], &none), None, "already theirs");
        probs[id("hi")] = 0.6; // a short Hinglish clip: not sure enough
        assert_eq!(offer(&probs, &en, &none), None);
        let mut s = Settings::default();
        assert_eq!(language_to_offer(&s, &vec![0.0; 16_000 * 5]), None, "auto-detect types every language");
        s.languages = en;
        assert_eq!(language_to_offer(&s, &vec![0.0; 16_000]), None, "under 3 s");
    }

    #[test]
    fn the_likeliest_of_the_users_languages_wins() {
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        let id = |c: &str| whisper_rs::get_lang_id(c).unwrap() as usize;
        probs[id("bo")] = 0.6; // Tibetan, which a short Hindi clip once came out as
        probs[id("hi")] = 0.3;
        probs[id("en")] = 0.1;
        let codes = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(likeliest(&probs, &codes(&["en", "hi"]), None).as_deref(), Some("hi"));
        assert_eq!(likeliest(&probs, &codes(&["en", "fr"]), None).as_deref(), Some("en"), "unscored French loses to English");
        assert_eq!(likeliest(&probs, &codes(&["xx", "en"]), None).as_deref(), Some("en"), "an unknown code never wins");
        assert_eq!(likeliest(&probs, &[], None), None);
    }

    #[test]
    fn a_close_call_leans_to_the_last_language() {
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        let id = |c: &str| whisper_rs::get_lang_id(c).unwrap() as usize;
        let codes = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pick = |probs: &[f32], c: &[&str], last| likeliest(probs, &codes(c), last);
        probs[id("hi")] = 0.444; // "Report bhej do, main dekh lunga." (TTS, Lekha)
        probs[id("en")] = 0.337;
        assert_eq!(pick(&probs, &["en", "hi"], None).as_deref(), Some("hi"));
        assert_eq!(pick(&probs, &["en", "hi"], Some("en")).as_deref(), Some("en"), "close: stays in English");
        assert_eq!(pick(&probs, &["en", "hi"], Some("hi")).as_deref(), Some("hi"));
        probs[id("en")] = 0.2;
        assert_eq!(pick(&probs, &["en", "hi"], Some("en")).as_deref(), Some("hi"), "more than twice as likely: Hindi");
        assert_eq!(pick(&probs, &["en", "hi", "fr"], Some("fr")).as_deref(), Some("hi"), "unscored last language loses");
        assert_eq!(pick(&probs, &["en", "hi"], Some("de")).as_deref(), Some("hi"), "a last language no longer chosen is ignored");
        probs[id("en")] = 0.25;
        assert_eq!(pick(&probs, &["hi", "en", "fr"], Some("en")).as_deref(), Some("en"), "three languages, exactly 2x: last wins");
    }

    #[test]
    fn translate_capability_by_id() {
        assert!(!translates("large-v3-turbo-q5_0"));
        assert!(!translates("large-v3-turbo"));
        assert!(!translates("base.en"));
        assert!(translates("medium"));
        assert!(translates("small"));
        assert_eq!(model_id("/Users/x/.cache/whisper/ggml-medium.bin"), "medium");
        assert_eq!(model_id("large-v3-turbo-q5_0"), "large-v3-turbo-q5_0");
    }

    #[test]
    fn automatic_picks_by_total_ram_and_uses_what_is_downloaded() {
        let all = |_: &str| true;
        let pick = |total, dl: &dyn Fn(&str) -> bool| list_models_with(total, 64_000, dl).into_iter().find(|m| m.automatic).unwrap().id;
        for total in [8192, 16_384, 24_576] {
            assert_eq!(pick(total, &all), "large-v3-turbo-q5_0", "Turbo on every Mac ({total} MB)");
        }
        // Until Turbo is downloaded, the practice model.
        assert_eq!(pick(8192, &|id| id == "tiny.en"), "tiny.en");
        let m8 = list_models_with(8192, 64_000, all);
        assert_eq!(m8.iter().filter(|m| m.recommended).map(|m| m.id).collect::<Vec<_>>(), ["large-v3-turbo-q5_0"]);
        // 2.7 GB free: tiny.en (+ 2048 headroom) fits, turbo-q5 does not.
        let tight = list_models_with(24_576, 2_780, all);
        assert!(tight.iter().find(|m| m.id == "tiny.en").unwrap().fits);
        assert!(!tight.iter().find(|m| m.id == "large-v3-turbo-q5_0").unwrap().fits);
    }

    #[test]
    fn bigger_models_warn_on_low_memory_macs_only() {
        let m8 = list_models_with(8192, 64_000, |_| true);
        let warn = |id: &str| m8.iter().find(|m| m.id == id).unwrap().warning.clone();
        assert!(warn("large-v3-turbo-q5_0").is_none() && warn("tiny.en").is_none(), "Automatic's pick and smaller ones carry no warning");
        assert_eq!(ram_warning(1869, ram_of("large-v3-turbo-q5_0"), 8192).unwrap(), format!("Uses about {} while dictating. On an 8 GB Mac this can slow other apps.", fmt_mb(1869)));
        assert!(list_models_with(24_576, 64_000, |_| true).iter().all(|m| m.warning.is_none()));
        assert_eq!(fmt_mb(800), "800 MB");
        assert_eq!(fmt_mb(2048), "2 GB");
        assert_eq!(fmt_mb(1869), "1.8 GB");
        assert_eq!(ram_of("not-in-the-table"), 1900, "unknown files are assumed large");
    }

    #[test]
    fn advanced_off_ignores_the_stored_manual_pick_and_on_restores_it() {
        let mut s = Settings::default();
        s.stt.local_model = "tiny.en".into();
        assert_ne!(local_model(&s), "tiny.en", "Automatic");
        s.advanced = true;
        assert_eq!(local_model(&s), "tiny.en", "the manual pick, kept while advanced was off");
    }

    #[test]
    fn every_catalog_model_has_a_measured_ram_row() {
        for m in MODELS {
            assert!(RAM_MB.iter().any(|(id, mb)| *id == m.id && *mb > 0), "{}", m.id);
        }
    }

    #[test]
    fn unknown_ids_cannot_reach_the_filesystem() {
        assert!(delete("../../etc/passwd").is_err());
        assert!(download("../x", |_, _| {}).is_err());
    }

    #[test]
    fn ready_needs_a_local_model_or_a_remote_key() {
        let mut s = Settings { advanced: true, ..Default::default() };
        s.stt.local_model = "not-a-model".into();
        assert_eq!(ready_with(&s, |_, _| true).unwrap_err(), "Choose a speech model");
        s.stt.provider = "groq".into();
        assert_eq!(ready_with(&s, |_, _| false).unwrap_err(), "Add your Groq API key to dictate");
        assert!(ready_with(&s, |stage, p| (stage, p) == ("stt", "groq")).is_ok());
        if settings::CLOUD {
            s.stt.provider = "cloud".into();
            assert_eq!(ready_with(&s, |_, _| false).unwrap_err(), "Sign in to Echotype Cloud to dictate");
        }
        s.stt.provider = "custom".into();
        assert!(ready_with(&s, |_, _| false).is_ok(), "custom servers may need no key");
    }

    #[test]
    fn whisper_text_cleanup_and_prompt() {
        assert_eq!(clean_text(" [BLANK_AUDIO] "), "");
        assert_eq!(clean_text(" Hello [Music] world. "), "Hello world.");
        let mut s = Settings { dictionary: vec!["Arnav".into(), " ".into(), "Xoxoday".into()], ..Default::default() };
        assert_eq!(prompt(&s), "Arnav, Xoxoday.");
        assert_eq!(prompt(&Settings { dictionary: vec![" ".into()], ..Default::default() }), "");
        let hi = ROMANISED.iter().find(|(l, _)| *l == "hi").unwrap().1;
        s.languages = vec!["en".into(), "hi".into()];
        assert_eq!(prompt(&s), "Arnav, Xoxoday.", "native script: the dictionary only");
        s.script = "latin".into();
        assert_eq!(prompt(&s), format!("{hi} Arnav, Xoxoday."), "English letters: Hindi's romanised sentence first");
        s.dictionary.clear();
        assert_eq!(prompt(&s), hi);
        s.translate_to_english = true;
        assert_eq!(prompt(&s), "", "a translation is English anyway");
        s.translate_to_english = false;
        s.languages = vec!["en".into(), "fr".into()];
        assert_eq!(prompt(&s), "", "no romanised sentence for a Latin-script language");
        s.languages.clear();
        assert_eq!(prompt(&s), "", "auto-detect: no language to romanise");
    }

    #[test]
    fn english_letters_decode_as_the_romanised_language() {
        let mut s = Settings { languages: vec!["en".into(), "hi".into()], ..Default::default() };
        assert_eq!(languages(&s), ["en", "hi"], "native script: each clip goes to the likeliest");
        s.script = "latin".into();
        assert_eq!(languages(&s), ["hi"], "English letters: no detection, English and mixed speech decode as Hindi under its prompt");
        s.translate_to_english = true;
        assert_eq!(languages(&s), ["en", "hi"], "a translation detects as before");
        s.translate_to_english = false;
        s.languages = vec!["en".into(), "fr".into()];
        assert_eq!(languages(&s), ["en", "fr"], "nothing to romanise: unchanged");
        s.languages.clear();
        assert!(languages(&s).is_empty(), "auto-detect stays auto-detect");
    }

    /// Live words against the final text on the same audio: the start of real speech, 2, 4 and 6 s.
    /// `ECHOTYPE_TEST_MODEL=<ggml> ECHOTYPE_TEST_WAV=<speech> cargo test --release partials_match_the_final_text -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn partials_match_the_final_text() {
        let (Ok(model), Ok(wav)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_WAV")) else { return eprintln!("skipped") };
        let model = Path::new(&model);
        let pcm = crate::audio::decode(std::fs::read(&wav).unwrap(), Some("wav")).unwrap();
        let en = ["en".to_string()];
        let stop = AtomicBool::new(false);
        for secs in [2usize, 4, 6] {
            let clip = &pcm[..(secs * 16_000).min(pcm.len())];
            let t = std::time::Instant::now();
            let partial = transcribe_partial(model, clip, &en, false, "", None, &stop).unwrap();
            let ms = t.elapsed().as_millis();
            let full = transcribe_local(model, clip, &en, false, "", None).unwrap();
            eprintln!("{secs} s ({ms} ms)\n  live:  {partial}\n  final: {full}");
        }
    }
}
