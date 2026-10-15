//! Speech to text: the ggml model catalog, in-process whisper.cpp, and any
//! OpenAI-compatible /audio/transcriptions endpoint.

use crate::settings::{self, Settings};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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

/// Best first: `recommended` is the first entry that fits in RAM.
pub const MODELS: &[Model] = &[
    Model { id: "large-v3-turbo-q5_0", label: "Large v3 Turbo (Q5)", size_mb: 547, note: "Best default: near-large accuracy, fast, 99 languages. Cannot translate.", sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2" },
    Model { id: "large-v3-turbo", label: "Large v3 Turbo", size_mb: 1624, note: "Full-precision turbo. Slightly more accurate, twice the size. Cannot translate.", sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69" },
    Model { id: "large-v3-q5_0", label: "Large v3 (Q5)", size_mb: 1031, note: "Most accurate, best for Hindi and other languages. Several times slower than Turbo; translates.", sha256: "d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1" },
    Model { id: "medium", label: "Medium", size_mb: 1533, note: "Multilingual and translates to English (Hinglish → English).", sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208" },
    Model { id: "small", label: "Small", size_mb: 488, note: "Multilingual, translates; lighter and less accurate than medium.", sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b" },
    Model { id: "base.en", label: "Base (English)", size_mb: 148, note: "English only. For low-memory machines.", sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002" },
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
    ("large-v3-turbo", 1840),
    // 2026-10-16, same CLI run as the rest (peak footprint 1,608 MB, max RSS 1,579 MB); turbo-q5 read
    // 806 MB in that session, so this machine's numbers had not drifted.
    ("large-v3-q5_0", 1608),
    ("medium", 1869),
    ("small", 717),
    ("base.en", 316),
    ("tiny.en", 224),
    ("echotype-clean", 1113),
];

/// RAM_MB for a catalog id; anything unknown is assumed to be as large as the largest model.
pub fn ram_of(id: &str) -> u64 {
    RAM_MB.iter().find(|(m, _)| *m == id).map_or(1900, |(_, mb)| *mb)
}

/// Macs with this much memory or less get the light setup: whisper small, the smallest
/// Echotype cleaner and "save" memory. Above it, turbo, the 1.5B cleaner and "ready".
pub const LOW_RAM_MB: u64 = 8192;

pub fn low_ram(total_mb: u64) -> bool {
    total_mb <= LOW_RAM_MB
}

/// The speech model Automatic recommends for a Mac with this much memory.
pub fn recommended(total_mb: u64) -> &'static str {
    if low_ram(total_mb) { "small" } else { "large-v3-turbo-q5_0" }
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
    Err(format!(
        "Translate to English needs a multilingual model (medium or small); {} cannot translate. Download one under Models.",
        model_id(chosen)
    ))
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
    Ok(())
}

/// Downloads `url` to `dest` through a .part file, kept only if its SHA-256 is `want`.
/// Downloads the user called off (onboarding's speed slider moved past a model mid-download);
/// fetch stops at its next chunk and deletes the .part file.
static CANCELLED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
pub const DOWNLOAD_CANCELLED: &str = "Download cancelled";

pub fn cancel_download(id: &str) {
    CANCELLED.lock().unwrap_or_else(|e| e.into_inner()).push(models_dir().join(format!("ggml-{id}.bin")));
}

/// True once, for a download that was called off.
fn take_cancel(dest: &Path) -> bool {
    let mut c = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
    let was = c.iter().any(|p| p == dest);
    c.retain(|p| p != dest);
    was
}

pub fn fetch(url: &str, dest: &Path, want: &str, mut progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    std::fs::create_dir_all(dest.parent().ok_or("bad model path")?).map_err(|e| e.to_string())?;
    take_cancel(dest); // a cancel from an earlier attempt does not stop this one
    let part = dest.with_extension("part");
    let client = reqwest::blocking::Client::builder().timeout(None::<Duration>).connect_timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().and_then(|r| r.error_for_status()).map_err(|e| format!("Download failed: {e}"))?;
    let total = resp.content_length();
    let result = (|| {
        let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 1 << 16];
        let (mut done, mut last) = (0u64, Instant::now());
        let mut hasher = sha2::Sha256::new();
        loop {
            let n = resp.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            if take_cancel(dest) {
                return Err(DOWNLOAD_CANCELLED.to_string());
            }
            f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            hasher.update(&buf[..n]);
            done += n as u64;
            if last.elapsed() >= Duration::from_millis(250) {
                progress(done, total);
                last = Instant::now();
            }
        }
        f.sync_all().map_err(|e| e.to_string())?;
        if total.is_some_and(|t| t != done) {
            return Err("Download ended early; try again".to_string());
        }
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != want {
            return Err("Downloaded model failed its checksum and was discarded; try again".to_string());
        }
        progress(done, total);
        Ok(())
    })();
    match result {
        Ok(()) => std::fs::rename(&part, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            Err(e)
        }
    }
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

/// True when Silero finds speech in the clip. A detector that fails to load or run says yes, so
/// a broken file never swallows dictation.
pub fn has_voice(vad: &Path, pcm: &[f32]) -> bool {
    let mut g = SILERO.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let Some(path) = vad.to_str() else { return true };
        match whisper_rs::WhisperVadContext::new(path, whisper_rs::WhisperVadContextParams::new()) {
            Ok(ctx) => *g = Some(ctx),
            Err(e) => {
                eprintln!("echotype: Silero did not load ({e}); transcribing without it");
                return true;
            }
        }
    }
    let ctx = g.as_mut().expect("loaded above");
    // ponytail: 0.3 (Silero's default is 0.5) from synthetic clips: macOS clicks and pops pass at
    // 0.2, a TTS whisper at -55 dBFS is cut at 0.4. Re-check on owner-recorded taps and whispers.
    let mut params = whisper_rs::WhisperVadParams::new();
    params.set_threshold(0.3);
    ctx.segments_from_samples(params, pcm).map_or(true, |s| s.num_segments() > 0)
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

/// Whisper prompt from the dictionary: biases spelling of names at no latency cost. Whisper
/// copies the prompt's style, so it ends like a sentence: a bare comma list made it drop full
/// stops, which ran a meeting's sentences (and so its speakers) together.
pub fn prompt(dictionary: &[String]) -> String {
    let words: Vec<String> = dictionary.iter().map(|w| w.trim().replace('\0', "")).filter(|w| !w.is_empty()).collect();
    if words.is_empty() { String::new() } else { format!("{}.", words.join(", ")) }
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
pub fn transcribe_local(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, vad: Option<&Path>) -> Result<String, String> {
    if pcm.is_empty() || vad.is_some_and(|v| !has_voice(v, pcm)) {
        return Ok(String::new());
    }
    run_local(model, pcm, languages, translate, prompt, false, vad, |state| {
        let mut text = String::new();
        for i in 0..state.full_n_segments() {
            if let Some(seg) = state.get_segment(i) {
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
/// `vad`: a Silero model; whisper then transcribes only the speech it finds.
pub fn transcribe_words(model: &Path, pcm: &[f32], languages: &[String], prompt: &str, vad: Option<&Path>) -> Result<Vec<Word>, String> {
    if pcm.is_empty() {
        return Ok(vec![]);
    }
    run_local(model, pcm, languages, false, prompt, true, vad, |state| {
        let mut words: Vec<Word> = vec![];
        for i in 0..state.full_n_segments() {
            let Some(seg) = state.get_segment(i) else { continue };
            for j in 0..seg.n_tokens() {
                let Some(tok) = seg.get_token(j) else { continue };
                let text = tok.to_str_lossy().map_err(|e| e.to_string())?;
                if text.starts_with("[_") || text.starts_with("<|") {
                    continue; // [_BEG_], [_TT_123], <|endoftext|>
                }
                let d = tok.token_data();
                let (t0, t1) = (d.t0 as f32 / 100.0, d.t1 as f32 / 100.0);
                match words.last_mut() {
                    Some(w) if !text.starts_with(' ') => {
                        w.text.push_str(&text);
                        w.end = w.end.max(t1);
                    }
                    _ => words.push(Word { start: t0, end: t1.max(t0), text: text.trim_start().to_string() }),
                }
            }
        }
        // "[BLANK_AUDIO]" and "[Music]" arrive as ordinary tokens; a lone "." or "-" is noise.
        words.retain_mut(|w| {
            w.text = clean_text(&w.text);
            w.text.chars().any(char::is_alphanumeric)
        });
        Ok(words)
    })
}

/// whisper.cpp's own thread count (min(4, cores)), for the language check before a run.
const THREADS: usize = 4;

/// The likeliest of `codes` by whisper's language scores; the first one on a tie or an unknown code.
fn likeliest(probs: &[f32], codes: &[String]) -> Option<String> {
    let score = |c: &str| whisper_rs::get_lang_id(c).and_then(|i| probs.get(i as usize).copied()).unwrap_or(f32::MIN);
    let mut best = codes.first()?;
    for c in &codes[1..] {
        if score(c) > score(best) {
            best = c;
        }
    }
    Some(best.clone())
}

#[allow(clippy::too_many_arguments)]
fn run_local<T>(model: &Path, pcm: &[f32], languages: &[String], translate: bool, prompt: &str, timestamps: bool, vad: Option<&Path>, read: impl FnOnce(&WhisperState) -> Result<T, String>) -> Result<T, String> {
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
    let lang = match languages {
        [] => None, // whisper detects among all 99
        [one] => Some(one.replace('\0', "")),
        several => {
            // Whisper's own detection, scored over the first 30 s, read only for the user's languages:
            // a short Hindi clip once came out as Tibetan under auto.
            state.pcm_to_mel(pcm, THREADS).map_err(|e| format!("Language detection failed: {e}"))?;
            let (_, probs) = state.lang_detect(0, THREADS).map_err(|e| format!("Language detection failed: {e}"))?;
            likeliest(&probs, several)
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
    if let (false, [one]) = (s.translate_to_english, s.languages.as_slice()) {
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

    /// A quiet room with a knock every 600 ms (owner report: tapping the laptop typed "Thank you.",
    /// a finger snap "I'm going to go to the next one."), through the real model: with Silero,
    /// nothing comes back; speech still does.
    /// `ECHOTYPE_TEST_MODEL=<ggml file> ECHOTYPE_TEST_WAV=<speech> [ECHOTYPE_TEST_NOISE=<wav:wav>] cargo test --release no_speech_no_text -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn no_speech_no_text() {
        let Ok(model) = std::env::var("ECHOTYPE_TEST_MODEL") else { return eprintln!("skipped: no ECHOTYPE_TEST_MODEL") };
        let (model, vad) = (Path::new(&model), crate::speakers::vad().expect("Silero downloaded (speakers::ensure_vad)"));
        let mut seed = 1u32;
        let mut pcm: Vec<f32> = (0..16_000 * 8)
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
            assert!(has_voice(&vad, &speech));
            eprintln!("Silero on {:.1} s of speech: {} ms", speech.len() as f32 / 16_000.0, t.elapsed().as_millis());
            let text = transcribe_local(model, &speech, &[], false, "", Some(&vad)).unwrap();
            eprintln!("speech with Silero: {text}");
            assert!(text.split_whitespace().count() >= 3, "speech still comes through: {text:?}");
        }
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
    fn a_cancelled_download_stops_once() {
        let dest = models_dir().join("ggml-cancel-test.bin");
        cancel_download("cancel-test");
        assert!(take_cancel(&dest) && !take_cancel(&dest), "a cancel stops one download, not the next");
        assert!(MODELS.iter().any(|m| m.id == "large-v3-q5_0" && translates(m.id)) && ram_of("large-v3-q5_0") > ram_of("large-v3-turbo-q5_0"));
        assert_eq!(recommended(16 * 1024), "large-v3-turbo-q5_0", "adding large-v3 leaves Automatic's pick alone");
    }

    #[test]
    fn the_likeliest_of_the_users_languages_wins() {
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        let id = |c: &str| whisper_rs::get_lang_id(c).unwrap() as usize;
        probs[id("bo")] = 0.6; // Tibetan, which a short Hindi clip once came out as
        probs[id("hi")] = 0.3;
        probs[id("en")] = 0.1;
        let codes = |c: &[&str]| c.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(likeliest(&probs, &codes(&["en", "hi"])).as_deref(), Some("hi"));
        assert_eq!(likeliest(&probs, &codes(&["en", "fr"])).as_deref(), Some("en"), "unscored French loses to English");
        assert_eq!(likeliest(&probs, &codes(&["xx", "en"])).as_deref(), Some("en"), "an unknown code never wins");
        assert_eq!(likeliest(&probs, &[]), None);
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
        assert_eq!(pick(8192, &all), "small", "an 8 GB Mac");
        assert_eq!(pick(24_576, &all), "large-v3-turbo-q5_0", "a 24 GB Mac: today's default");
        assert_eq!(pick(16_384, &all), "large-v3-turbo-q5_0");
        // Until the recommendation is downloaded, the closest downloaded model, smaller first.
        assert_eq!(pick(8192, &|id| id == "large-v3-turbo-q5_0" || id == "base.en"), "base.en");
        assert_eq!(pick(8192, &|id| id == "medium"), "medium", "a larger one beats none");
        assert_eq!(pick(24_576, &|id| id == "small" || id == "medium"), "small");
        let m8 = list_models_with(8192, 64_000, all);
        assert_eq!(m8.iter().filter(|m| m.recommended).count(), 1);
        assert!(m8.iter().find(|m| m.id == "small").unwrap().recommended);
        // 3 GB free: turbo-q5 (+ 2048 headroom) fits, medium does not.
        let tight = list_models_with(24_576, 3_000, all);
        assert!(tight.iter().find(|m| m.id == "large-v3-turbo-q5_0").unwrap().fits);
        assert!(!tight.iter().find(|m| m.id == "medium").unwrap().fits);
    }

    #[test]
    fn bigger_models_warn_on_low_memory_macs_only() {
        let m8 = list_models_with(8192, 64_000, |_| true);
        let warn = |id: &str| m8.iter().find(|m| m.id == id).unwrap().warning.clone();
        assert_eq!(warn("medium").unwrap(), format!("Uses about {} while dictating. On an 8 GB Mac this can slow other apps.", fmt_mb(ram_of("medium"))));
        assert!(warn("small").is_none() && warn("tiny.en").is_none(), "Automatic's pick and smaller ones carry no warning");
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
        assert_eq!(prompt(&["Arnav".into(), " ".into(), "Xoxoday".into()]), "Arnav, Xoxoday.");
        assert_eq!(prompt(&[" ".into()]), "");
    }
}
