//! Speech to text: the ggml model catalog, in-process whisper.cpp, and any
//! OpenAI-compatible /audio/transcriptions endpoint.

use crate::settings::{self, Settings};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

/// Free RAM a model must leave behind for everything else on the machine.
const HEADROOM_MB: u64 = 2048;

pub struct Model {
    pub id: &'static str,
    pub label: &'static str,
    pub size_mb: u64,
    pub ram_mb: u64,
    pub note: &'static str,
}

/// Best first: `recommended` is the first entry that fits in RAM.
pub const MODELS: &[Model] = &[
    Model { id: "large-v3-turbo-q5_0", label: "Large v3 Turbo (Q5)", size_mb: 547, ram_mb: 800, note: "Best default: near-large accuracy, fast, 99 languages. Cannot translate." },
    Model { id: "large-v3-turbo", label: "Large v3 Turbo", size_mb: 1624, ram_mb: 1700, note: "Full-precision turbo. Slightly more accurate, twice the size. Cannot translate." },
    Model { id: "medium", label: "Medium", size_mb: 1533, ram_mb: 1800, note: "Multilingual and translates to English (Hinglish → English)." },
    Model { id: "small", label: "Small", size_mb: 488, ram_mb: 852, note: "Multilingual, translates; lighter and less accurate than medium." },
    Model { id: "base.en", label: "Base (English)", size_mb: 148, ram_mb: 388, note: "English only. For low-memory machines." },
    Model { id: "tiny.en", label: "Tiny (English)", size_mb: 78, ram_mb: 273, note: "English only. Fastest, least accurate." },
];

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

/// (total, available) in MB.
pub fn ram_mb() -> (u64, u64) {
    let mut s = sysinfo::System::new();
    s.refresh_memory();
    #[cfg(target_os = "macos")]
    let avail = mac_available_bytes().unwrap_or(s.available_memory());
    #[cfg(not(target_os = "macos"))]
    let avail = s.available_memory();
    (s.total_memory() / 1_048_576, avail / 1_048_576)
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
    pub fits: bool,
    pub recommended: bool,
    pub note: &'static str,
}

pub fn list_models(available_mb: u64) -> Vec<ModelStatus> {
    let best = MODELS.iter().find(|m| fits(m.ram_mb, available_mb)).map(|m| m.id);
    MODELS
        .iter()
        .map(|m| ModelStatus {
            id: m.id,
            label: m.label,
            size_mb: m.size_mb,
            ram_mb: m.ram_mb,
            english_only: m.id.ends_with(".en"),
            translates: translates(m.id),
            downloaded: model_file(m.id).is_some(),
            fits: fits(m.ram_mb, available_mb),
            recommended: Some(m.id) == best,
            note: m.note,
        })
        .collect()
}

/// Picks the local model for these settings. When translation is on and the chosen model
/// cannot translate, falls back to the best downloaded translating model that fits in RAM,
/// returning a warning that says so.
pub fn pick_local_model(s: &Settings) -> Result<(PathBuf, Option<String>), String> {
    let chosen = &s.stt.local_model;
    if !s.translate_to_english || translates(&model_id(chosen)) {
        return Ok((resolve(chosen)?, None));
    }
    let (_, avail) = ram_mb();
    for m in MODELS.iter().filter(|m| translates(m.id) && fits(m.ram_mb, avail)) {
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

// ---------- download / delete ----------

pub fn download(id: &str, mut progress: impl FnMut(u64, Option<u64>)) -> Result<(), String> {
    catalog(id)?;
    let dir = models_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(format!("ggml-{id}.bin"));
    let part = dir.join(format!("ggml-{id}.bin.part"));
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{id}.bin");
    let client = reqwest::blocking::Client::builder().timeout(None::<Duration>).connect_timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    let mut resp = client.get(&url).send().and_then(|r| r.error_for_status()).map_err(|e| format!("Download failed: {e}"))?;
    let total = resp.content_length();
    let result = (|| {
        let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 1 << 16];
        let (mut done, mut last) = (0u64, Instant::now());
        loop {
            let n = resp.read(&mut buf).map_err(|e| format!("Download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
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
        progress(done, total);
        Ok(())
    })();
    match result {
        Ok(()) => std::fs::rename(&part, &dest).map_err(|e| e.to_string()),
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
static ENGINE: Mutex<Option<(PathBuf, WhisperState)>> = Mutex::new(None);

fn unload_if(path: &Path) {
    let mut g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|(p, _)| p == path) {
        *g = None;
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

/// Whisper prompt from the dictionary: biases spelling of names at no latency cost.
pub fn prompt(dictionary: &[String]) -> String {
    dictionary.iter().map(|w| w.trim().replace('\0', "")).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(", ")
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

pub fn transcribe_local(model: &Path, pcm: &[f32], language: Option<&str>, translate: bool, prompt: &str) -> Result<String, String> {
    if pcm.is_empty() {
        return Ok(String::new());
    }
    let mut g = ENGINE.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_none_or(|(p, _)| p != model) {
        *g = None; // free the old model before loading the next one
        let path = model.to_str().ok_or("Model path is not valid UTF-8")?;
        let mut cp = WhisperContextParameters::default();
        cp.flash_attn(true); // whisper.cpp's own CLI default; faster on Metal
        let ctx = WhisperContext::new_with_params(path, cp).map_err(|e| format!("Could not load {}: {e}", model.display()))?;
        let state = ctx.create_state().map_err(|e| format!("Could not start whisper: {e}"))?;
        *g = Some((model.to_path_buf(), state));
    }
    let state = &mut g.as_mut().expect("loaded above").1;
    let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let lang = language.map(|l| l.replace('\0', ""));
    p.set_language(lang.as_deref());
    p.set_translate(translate);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_special(false);
    p.set_print_timestamps(false);
    if !prompt.is_empty() {
        p.set_initial_prompt(prompt);
    }
    state.full(p, pcm).map_err(|e| format!("Transcription failed: {e}"))?;
    let mut text = String::new();
    for i in 0..state.full_n_segments() {
        if let Some(seg) = state.get_segment(i) {
            text.push_str(&seg.to_str_lossy().map_err(|e| e.to_string())?);
        }
    }
    Ok(clean_text(&text))
}

/// Loads the model and runs one second of silence through it so the first real
/// dictation does not pay for model load and Metal kernel compilation.
pub fn warm_up(s: &Settings) {
    if s.stt.provider != "local" {
        return;
    }
    let Ok((path, _)) = pick_local_model(s) else { return };
    let ram = MODELS.iter().find(|m| m.id == model_id(&path.to_string_lossy())).map_or(1700, |m| m.ram_mb);
    if !fits(ram, ram_mb().1) {
        eprintln!("echotype: not warming up {}: not enough free RAM", path.display());
        return;
    }
    let t = Instant::now();
    match transcribe_local(&path, &vec![0.0; 16_000], Some("en"), false, "") {
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
    if !s.translate_to_english && s.language != "auto" && !s.language.is_empty() {
        form = form.text("language", s.language.clone());
    }
    if !prompt.is_empty() {
        form = form.text("prompt", prompt.to_string());
    }
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build().map_err(|e| e.to_string())?;
    let mut req = client.post(format!("{base}/{endpoint}")).multipart(form);
    if let Some(key) = settings::get_secret(&format!("stt:{provider}")) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().map_err(|e| format!("Speech-to-text request failed: {e}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(format!("Speech-to-text failed ({status}): {}", body.chars().take(300).collect::<String>()));
    }
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|_| format!("Unexpected speech-to-text response: {}", body.chars().take(200).collect::<String>()))?;
    Ok(v["text"].as_str().map(clean_text).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn recommended_is_best_that_fits() {
        let all = list_models(64_000);
        assert_eq!(all.iter().filter(|m| m.recommended).count(), 1);
        assert!(all.iter().find(|m| m.id == "large-v3-turbo-q5_0").unwrap().recommended);
        // 3 GB free: turbo-q5 (800 + 2048) fits, medium (1800 + 2048) does not.
        let tight = list_models(3_000);
        assert!(tight.iter().find(|m| m.id == "large-v3-turbo-q5_0").unwrap().fits);
        assert!(!tight.iter().find(|m| m.id == "medium").unwrap().fits);
        let tiny = list_models(2_500);
        assert_eq!(tiny.iter().find(|m| m.recommended).unwrap().id, "base.en");
        assert!(list_models(100).iter().all(|m| !m.recommended && !m.fits));
    }

    #[test]
    fn unknown_ids_cannot_reach_the_filesystem() {
        assert!(delete("../../etc/passwd").is_err());
        assert!(download("../x", |_, _| {}).is_err());
    }

    #[test]
    fn whisper_text_cleanup_and_prompt() {
        assert_eq!(clean_text(" [BLANK_AUDIO] "), "");
        assert_eq!(clean_text(" Hello [Music] world. "), "Hello world.");
        assert_eq!(prompt(&["Arnav".into(), " ".into(), "Xoxoday".into()]), "Arnav, Xoxoday");
    }
}
