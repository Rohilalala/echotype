//! Settings, presets, keychain secrets and history: everything Echotype persists.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub const APP_ID: &str = "app.echotype.desktop";
const HISTORY_CAP: usize = 500;

// Same directories Tauri's path resolver returns for this identifier, so the CLI
// (which runs without an AppHandle) reads the same files as the app.
// ECHOTYPE_CONFIG_DIR puts settings, history and downloaded models in one other folder (tests, a second profile).
fn override_dir() -> Option<PathBuf> {
    std::env::var_os("ECHOTYPE_CONFIG_DIR").filter(|d| !d.is_empty()).map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    override_dir().unwrap_or_else(|| dirs::config_dir().unwrap_or_else(std::env::temp_dir).join(APP_ID))
}

pub fn data_dir() -> PathBuf {
    override_dir().unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(APP_ID))
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub hotkey: String,
    /// "hold" (push-to-talk) or "toggle".
    pub mode: String,
    pub input_device: Option<String>,
    /// Whisper language code, or "auto".
    pub language: String,
    pub translate_to_english: bool,
    pub local_api: LocalApi,
    pub stt: Stt,
    pub cleanup: Cleanup,
    pub dictionary: Vec<String>,
    pub replacements: Vec<Replacement>,
    pub restore_clipboard: bool,
    pub history_enabled: bool,
    /// Count words and speaking speed on this computer (stats.rs). Counts only; on for upgraders too.
    pub usage_stats: bool,
    /// Shown on the Stats page ("Aditya's stats"). Stays on this computer.
    pub display_name: String,
    pub onboarded: bool,
    /// macOS extra talk key next to the shortcut: "accelerator" (none) | "fn" | "right_option" |
    /// "right_command" | "right_control". Hold or press it like the shortcut, per `mode`.
    pub talk_key: String,
    /// Paste and copy the last result again. "" is unbound, the serde default, so upgraders
    /// are offered these instead of having keys taken; onboarding binds them for new installs.
    pub paste_last_hotkey: String,
    pub copy_last_hotkey: String,
    /// Launch at login. Off for upgraders; onboarding offers it pre-checked.
    pub autostart: bool,
    /// Start, stop and lock sounds (macOS).
    pub sounds: bool,
    /// One-time notices already dismissed: "upgrade_offer", "bluetooth_tip", "no_cleanup_banner".
    pub dismissed: Vec<String>,
    /// The idle pill (macOS): a thin bar bottom-centre while not dictating. On for upgraders too.
    pub pill_idle: bool,
    /// "Hide for 1 hour": the idle pill stays away until this time (ms since the epoch); 0 = not hidden.
    pub pill_hidden_until: u64,
    /// Let screenshots and screen sharing see the pill. Off = content protection (best effort).
    pub pill_in_recordings: bool,
    /// Settings window look: "classic" (the default) or "v2" (Ink).
    pub look: String,
    /// Where the pill was dropped on each display (macOS): its window's top-left as a fraction
    /// of the room it has in that display's visible frame, keyed by the display's name. A display
    /// with no entry gets bottom-centre; double-clicking the pill removes its entry.
    pub pill_positions: BTreeMap<String, [f64; 2]>,
    /// The display the pill was last dropped on (its `pill_positions` key).
    pub pill_screen: Option<String>,
    /// "Move to the screen I'm using": the pill follows the display under the cursor. Off keeps
    /// it on `pill_screen` (the primary display until it is dragged somewhere, or while that one is unplugged).
    pub pill_follow_cursor: bool,
    /// "pill" | "menubar" (macOS): in the menu bar the pill never shows and the status item is the live indicator.
    pub show_in: String,
    /// Pill size (Settings → Pill): how much larger than its 1x design the pill, orb, capsules,
    /// card and their text are drawn, 1.0 to 2.0.
    pub pill_size: f64,
    /// The idle ball's size relative to its 10 px design, 0.6 to 2.0 (on top of `pill_size`).
    pub ball_size: f64,
    /// The idle ball fades back while the pointer is away and brightens as it comes near; off = always full strength.
    pub pill_fade: bool,
    /// Which bundled sound pack plays (one of SOUND_PACKS) when `sounds` is on.
    pub sound_pack: String,
    /// "Show advanced settings". Off: Echotype picks the speech model, the cleaner and its memory
    /// mode for this Mac (Automatic). On: the manual picks below and `stt.local_model` apply. They
    /// stay stored while it is off, so turning it back on restores them.
    pub advanced: bool,
    /// The built-in cleaner's memory (advanced): "save" (macOS may page the model out) | "ready" (mlocked).
    pub cleaner_memory: String,
    /// Stop the built-in cleaner after `cleaner_idle_minutes` without use (advanced).
    pub cleaner_idle_unload: bool,
    pub cleaner_idle_minutes: u32,
    /// The built-in cleaner's context in tokens (advanced); llm::CONTEXT otherwise.
    pub cleaner_context: u32,
    /// A saved Echotype Cloud provider was moved to this Mac's models; Home says so once.
    pub cloud_moved: bool,
    /// A short hello at the top of Home, by time of day (Settings → Advanced). On for upgraders too.
    pub greetings: bool,
}

/// The bundled sound packs (sounds/<pack>/{start,stop,lock,nopaste}.wav).
pub const SOUND_PACKS: [&str; 5] = ["classic", "glass", "wood", "pop", "soft"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LocalApi {
    pub enabled: bool,
    pub port: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Stt {
    /// "local" or a preset id.
    pub provider: String,
    /// Catalog id ("large-v3-turbo-q5_0") or a path to a ggml file.
    pub local_model: String,
    /// Remote model name.
    pub model: String,
    /// Overrides the preset base URL when non-empty.
    pub base_url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Cleanup {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    /// "default" | "casual" | "formal" | "verbatim".
    pub style: String,
    pub custom_instructions: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "Alt+Space".into(),
            mode: "hold".into(),
            input_device: None,
            language: "auto".into(),
            translate_to_english: false,
            local_api: LocalApi::default(),
            stt: Stt::default(),
            cleanup: Cleanup::default(),
            dictionary: vec![],
            replacements: vec![],
            restore_clipboard: true,
            history_enabled: true,
            usage_stats: true,
            display_name: String::new(),
            onboarded: false,
            talk_key: "accelerator".into(),
            paste_last_hotkey: String::new(),
            copy_last_hotkey: String::new(),
            autostart: false,
            sounds: true,
            dismissed: vec![],
            pill_idle: true,
            pill_hidden_until: 0,
            pill_in_recordings: false,
            look: "classic".into(),
            pill_positions: BTreeMap::new(),
            pill_screen: None,
            pill_follow_cursor: true,
            show_in: "pill".into(),
            pill_size: 1.5,
            ball_size: 1.0,
            sound_pack: "classic".into(),
            pill_fade: true,
            advanced: false,
            cleaner_memory: "ready".into(),
            cleaner_idle_unload: false,
            cleaner_idle_minutes: 10,
            cleaner_context: 1024,
            cloud_moved: false,
            greetings: true,
        }
    }
}

/// Lowest local API port: below this are privileged ports, and 0 means "any port".
pub const MIN_PORT: u16 = 1024;

impl Default for LocalApi {
    fn default() -> Self {
        Self { enabled: false, port: 7811 }
    }
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            provider: "local".into(),
            local_model: "large-v3-turbo-q5_0".into(),
            model: "whisper-large-v3-turbo".into(),
            base_url: String::new(),
        }
    }
}

impl Default for Cleanup {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "ollama".into(),
            model: "gemma3:1b".into(),
            base_url: String::new(),
            style: "default".into(),
            custom_instructions: String::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        config_dir().join("settings.json")
    }

    /// Missing file → defaults. Unreadable file → defaults (the file is left alone
    /// until the next save, so a hand-edit typo never silently destroys it).
    pub fn load() -> Self {
        let mut s: Self = match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("echotype: settings.json is invalid ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        s.migrate();
        s
    }

    /// Fixes values older builds wrote, and keeps settings off providers this build hides.
    pub(crate) fn migrate(&mut self) {
        if self.look == "foundry" {
            self.look = "classic".into(); // the Foundry look is gone
        }
        if self.local_api.port < MIN_PORT {
            self.local_api.port = LocalApi::default().port; // older builds saved 0 from a cleared field
        }
        let fit = |v: f64, lo: f64, hi: f64, def: f64| if v.is_finite() { v.clamp(lo, hi) } else { def };
        self.pill_size = fit(self.pill_size, 1.0, 2.0, 1.5);
        self.ball_size = fit(self.ball_size, 0.6, 2.0, 1.0);
        if !SOUND_PACKS.contains(&self.sound_pack.as_str()) {
            self.sound_pack = "classic".into();
        }
        if !matches!(self.cleaner_memory.as_str(), "save" | "ready") {
            self.cleaner_memory = "ready".into();
        }
        // No Echotype Cloud in this build: its users land on this Mac's own models, on Automatic.
        if !CLOUD && self.stt.provider == "cloud" {
            self.stt.provider = "local".into();
            self.advanced = false;
            self.cloud_moved = true;
        }
        if !CLOUD && self.cleanup.provider == "cloud" {
            self.cleanup.provider = "echotype".into();
            self.cleanup.model = String::new();
            self.advanced = false;
            self.cloud_moved = true;
        }
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
        write_atomic(&Self::path(), &serde_json::to_string_pretty(self).map_err(|e| e.to_string())?)
    }
}

/// Three-way merge for a writer holding a stale copy: every field `incoming` changed relative
/// to `base` (what that writer last saw) wins; everything else keeps `current`. So a queued or
/// stale save from the settings window only applies that window's own edits and never reverts a
/// change committed since it loaded (another save, or an R2 Rust-side writer such as the pill menu).
pub fn merge(current: &Settings, base: &Settings, incoming: &Settings) -> Settings {
    use serde_json::Value;
    fn walk(cur: &mut Value, base: &Value, inc: &Value) {
        match (cur, base, inc) {
            (Value::Object(c), Value::Object(b), Value::Object(i)) => {
                for (k, iv) in i {
                    match (c.get_mut(k), b.get(k)) {
                        (Some(cv), Some(bv)) => walk(cv, bv, iv),
                        (Some(cv), None) => *cv = iv.clone(),
                        (None, _) => {}
                    }
                }
            }
            (c, b, i) if i != b => *c = i.clone(),
            _ => {}
        }
    }
    let to = |s: &Settings| serde_json::to_value(s).expect("settings serialize");
    let mut out = to(current);
    walk(&mut out, &to(base), &to(incoming));
    serde_json::from_value(out).unwrap_or_else(|_| incoming.clone())
}

/// Write to a uniquely named temp file, fsync, then rename over `path`: a crash leaves the
/// old file or the new one, never a torn one, and concurrent writers never share a temp file.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("tmp.{}.{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let res = std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(contents.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(|e| e.to_string())
}

// ---------- presets ----------

/// Echotype Cloud is offered: `cargo build --features cloud`. Off for the launch build, which then
/// shows no Cloud provider, sign-in, plan or upgrade anywhere (cloud.rs and server/ stay).
pub const CLOUD: bool = cfg!(feature = "cloud");

/// Echotype Cloud, the optional hosted tier (server/ in this repo).
pub const CLOUD_URL: &str = "https://cloud.echotype.app/v1";

/// CLOUD_URL, or ECHOTYPE_CLOUD_URL for people who run server/ themselves.
pub fn cloud_url() -> String {
    std::env::var("ECHOTYPE_CLOUD_URL").ok().map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).unwrap_or_else(|| CLOUD_URL.into())
}

#[derive(Serialize, Clone, Debug)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub stt: bool,
    pub cleanup: bool,
    pub base_url: &'static str,
    pub default_stt_model: &'static str,
    pub default_cleanup_model: &'static str,
    pub needs_key: bool,
}

pub fn presets() -> Vec<Preset> {
    let p = |id, label, stt, cleanup, base_url, default_stt_model, default_cleanup_model, needs_key| Preset {
        id,
        label,
        stt,
        cleanup,
        base_url,
        default_stt_model,
        default_cleanup_model,
        needs_key,
    };
    let mut all = vec![
        p("cloud", "Echotype Cloud", true, true, CLOUD_URL, "whisper-large-v3-turbo", "openai/gpt-oss-20b", true),
        p("openai", "OpenAI", true, true, "https://api.openai.com/v1", "gpt-4o-mini-transcribe", "gpt-4.1-nano", true),
        p("groq", "Groq", true, true, "https://api.groq.com/openai/v1", "whisper-large-v3-turbo", "openai/gpt-oss-20b", true),
        p("openrouter", "OpenRouter", false, true, "https://openrouter.ai/api/v1", "", "openai/gpt-4.1-nano", true),
        p("ollama", "Ollama", false, true, "http://localhost:11434/v1", "", "gemma3:1b", false),
        p("lmstudio", "LM Studio", false, true, "http://localhost:1234/v1", "", "google/gemma-3-1b", false),
        // Built in: llm.rs runs it on a free localhost port, so it has no fixed URL.
        p("echotype", "Echotype model (local)", false, true, "", "", "echotype-clean", false),
        p("custom", "Custom (OpenAI-compatible)", true, true, "", "", "", false),
    ];
    all.retain(|p| CLOUD || p.id != "cloud");
    all
}

pub fn preset(id: &str) -> Option<Preset> {
    presets().into_iter().find(|p| p.id == id)
}

/// Only the `custom` provider uses the settings URL; every preset uses its own,
/// so a stale custom URL can never receive another provider's API key.
/// Never ends with '/'.
pub fn base_url(provider: &str, override_url: &str) -> Result<String, String> {
    let url = if provider == "custom" {
        override_url.trim().to_string()
    } else if provider == "cloud" {
        cloud_url()
    } else {
        preset(provider).map(|p| p.base_url.to_string()).unwrap_or_default()
    };
    if url.is_empty() {
        return Err(format!("No base URL set for provider '{provider}'"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

// ---------- secrets (OS keychain) ----------

/// Keychain name of a provider's API key for one stage ("stt" or "llm"). Echotype Cloud
/// signs in once, so both stages share the one `cloud` key.
pub fn key_name(stage: &str, provider: &str) -> String {
    if provider == "cloud" { "cloud".into() } else { format!("{stage}:{provider}") }
}

pub fn provider_key(stage: &str, provider: &str) -> Option<String> {
    get_secret(&key_name(stage, provider))
}

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(APP_ID, name).map_err(|e| e.to_string())
}

pub fn get_secret(name: &str) -> Option<String> {
    secret(name).ok().flatten()
}

/// `Ok(None)` only when the keychain has no such entry. A locked or denied keychain is an
/// error, so a caller never mistakes it for "missing" and overwrites the secret.
pub fn secret(name: &str) -> Result<Option<String>, String> {
    match entry(name)?.get_password() {
        Ok(s) => Ok(Some(s).filter(|s| !s.is_empty())),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Could not read '{name}' from the keychain: {e}")),
    }
}

pub fn set_secret(name: &str, value: &str) -> Result<(), String> {
    entry(name)?.set_password(value).map_err(|e| e.to_string())
}

pub fn delete_secret(name: &str) -> Result<(), String> {
    match entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

// ---------- history (JSONL, newest last on disk) ----------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// Unix time in milliseconds.
    pub ts: u64,
    pub raw: String,
    pub text: String,
    pub stt_ms: u64,
    pub cleanup_ms: u64,
    pub stt_provider: String,
    pub cleanup_provider: Option<String>,
    /// Cleanup was off or had no key, so this is the raw transcript ("No cleanup").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_cleanup: bool,
}

fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

// ponytail: one process-wide lock; the CLI and local API never write history, so no cross-process lock is needed.
static HISTORY: Mutex<()> = Mutex::new(());

pub fn append_history(entry: &HistoryEntry) -> Result<(), String> {
    append_history_to(&history_path(), entry)
}

fn append_history_to(path: &Path, entry: &HistoryEntry) -> Result<(), String> {
    let _g = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
    lines.push(serde_json::to_string(entry).map_err(|e| e.to_string())?);
    let start = lines.len().saturating_sub(HISTORY_CAP);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    write_atomic(path, &(lines[start..].join("\n") + "\n"))
}

pub fn get_history(limit: usize) -> Vec<HistoryEntry> {
    let s = std::fs::read_to_string(history_path()).unwrap_or_default();
    s.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(limit).collect()
}

pub fn clear_history() -> Result<(), String> {
    let _g = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    match std::fs::remove_file(history_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_contract() {
        let s = Settings::default();
        assert_eq!(s.hotkey, "Alt+Space");
        assert_eq!(s.mode, "hold");
        assert_eq!(s.stt.provider, "local");
        assert_eq!(s.stt.local_model, "large-v3-turbo-q5_0");
        assert!(!s.cleanup.enabled);
        assert_eq!(s.cleanup.provider, "ollama");
        assert_eq!(s.cleanup.model, "gemma3:1b");
        assert_eq!(s.cleanup.style, "default");
        assert_eq!(s.local_api, LocalApi { enabled: false, port: 7811 });
        assert!(!s.onboarded);
    }

    #[test]
    fn old_and_partial_files_still_load() {
        let s: Settings = serde_json::from_str(r#"{"hotkey":"Ctrl+Shift+D","stt":{"provider":"groq"},"unknown_field":1}"#).unwrap();
        assert_eq!(s.hotkey, "Ctrl+Shift+D");
        assert_eq!(s.stt.provider, "groq");
        assert_eq!(s.stt.local_model, "large-v3-turbo-q5_0");
        assert_eq!(s.cleanup, Cleanup::default());
        assert!(s.history_enabled);
    }

    #[test]
    fn a_pre_v11_settings_file_loads_with_the_upgrade_defaults() {
        // A real settings.json from v1.0: every field it has, none of the v1.1 ones.
        let old = r#"{"hotkey":"Alt+Space","mode":"toggle","input_device":null,"language":"auto","translate_to_english":false,
            "local_api":{"enabled":true,"port":7811},"stt":{"provider":"local","local_model":"large-v3-turbo-q5_0","model":"whisper-large-v3-turbo","base_url":""},
            "cleanup":{"enabled":true,"provider":"ollama","model":"gemma3:1b","base_url":"","style":"default","custom_instructions":""},
            "dictionary":["Arnav"],"replacements":[],"restore_clipboard":true,"history_enabled":true,"onboarded":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(s.onboarded && s.mode == "toggle" && s.local_api.enabled);
        assert_eq!((s.paste_last_hotkey.as_str(), s.copy_last_hotkey.as_str()), ("", ""), "upgraders get no new global shortcuts");
        assert!(!s.autostart, "upgraders are offered launch at login, not opted in");
        assert_eq!(s.talk_key, "accelerator");
        assert!(s.sounds && s.dismissed.is_empty());
        assert!(s.pill_idle && s.pill_hidden_until == 0 && !s.pill_in_recordings, "the idle pill is on for upgraders, hidden from capture");
        assert_eq!(s.look, "classic", "upgraders keep the classic look");
        let mut f: Settings = serde_json::from_str(r#"{"look":"foundry","look_chosen":true}"#).unwrap();
        f.migrate();
        assert_eq!(f.look, "classic", "a saved Foundry look loads as Classic");
        assert!(s.pill_positions.is_empty() && s.show_in == "pill", "upgraders keep the pill, bottom-centre");
        assert!(s.pill_follow_cursor && s.pill_screen.is_none(), "...on the screen they are using");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn a_stale_save_keeps_what_rust_wrote_and_applies_what_the_window_changed() {
        let base = Settings::default(); // what the window loaded
        let mut current = base.clone(); // Rust wrote since then
        current.dismissed = vec!["bluetooth_tip".into()];
        current.cleanup.model = "gemma3:4b".into();
        let mut incoming = base.clone(); // the window's stale copy plus its own edit
        incoming.mode = "toggle".into();
        incoming.cleanup.style = "casual".into();
        let m = merge(&current, &base, &incoming);
        assert_eq!(m.dismissed, ["bluetooth_tip"]);
        assert_eq!(m.cleanup.model, "gemma3:4b");
        assert_eq!((m.mode.as_str(), m.cleanup.style.as_str()), ("toggle", "casual"));
        // An up-to-date window (base == current) simply wins.
        assert_eq!(merge(&current, &current, &incoming), incoming);
    }

    #[test]
    fn settings_round_trip() {
        let mut s = Settings::default();
        s.dictionary = vec!["Arnav".into()];
        s.replacements = vec![Replacement { from: "my email".into(), to: "me@example.com".into() }];
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn presets_and_base_urls() {
        let ids: Vec<_> = presets().iter().map(|p| p.id).collect();
        let all = ["cloud", "openai", "groq", "openrouter", "ollama", "lmstudio", "echotype", "custom"];
        assert_eq!(ids, if CLOUD { &all[..] } else { &all[1..] }, "Echotype Cloud only with the cloud feature");
        assert_eq!(preset("openai").unwrap().default_stt_model, "gpt-4o-mini-transcribe");
        assert_eq!(preset("groq").unwrap().default_cleanup_model, "openai/gpt-oss-20b");
        assert!(!preset("openrouter").unwrap().stt);
        assert_eq!(base_url("groq", "").unwrap(), "https://api.groq.com/openai/v1");
        assert_eq!(base_url("openai", "http://box.lan:8000/v1").unwrap(), "https://api.openai.com/v1");
        assert_eq!(base_url("custom", "http://127.0.0.1:9/v1/").unwrap(), "http://127.0.0.1:9/v1");
        assert_eq!(base_url("cloud", "").unwrap(), CLOUD_URL);
        assert!(base_url("custom", " ").is_err());
    }

    #[test]
    fn a_saved_cloud_provider_lands_on_local_automatic_without_the_cloud_feature() {
        let mut s: Settings = serde_json::from_str(r#"{"advanced":true,"stt":{"provider":"cloud"},"cleanup":{"enabled":true,"provider":"cloud","model":"openai/gpt-oss-20b"}}"#).unwrap();
        s.migrate();
        if CLOUD {
            assert_eq!((s.stt.provider.as_str(), s.cleanup.provider.as_str(), s.cloud_moved), ("cloud", "cloud", false));
            return;
        }
        assert_eq!((s.stt.provider.as_str(), s.cleanup.provider.as_str(), s.cleanup.model.as_str()), ("local", "echotype", ""));
        assert!(s.cleanup.enabled && !s.advanced && s.cloud_moved);
        assert!(presets().iter().all(|p| p.id != "cloud"), "settings never point at a hidden provider");
        let mut plain = Settings::default();
        plain.migrate();
        assert!(!plain.cloud_moved);
    }

    #[test]
    fn advanced_is_off_and_its_picks_have_defaults() {
        let s: Settings = serde_json::from_str(r#"{"cleaner_memory":"bogus"}"#).map(|mut s: Settings| { s.migrate(); s }).unwrap();
        assert!(!s.advanced && !s.cleaner_idle_unload);
        assert!(s.greetings, "the Home greeting is on for upgraders");
        assert_eq!((s.cleaner_memory.as_str(), s.cleaner_idle_minutes, s.cleaner_context), ("ready", 10, 1024));
    }

    #[test]
    fn cloud_shares_one_key_across_stages() {
        assert_eq!(key_name("stt", "cloud"), "cloud");
        assert_eq!(key_name("llm", "cloud"), "cloud");
        assert_eq!(key_name("stt", "groq"), "stt:groq");
        assert_eq!(key_name("llm", "openrouter"), "llm:openrouter");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("echotype-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn concurrent_history_appends_keep_every_entry() {
        let path = scratch("history").join("history.jsonl");
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..25u64 {
                        let e = HistoryEntry { ts: t * 1000 + i, raw: "r".into(), text: "t".into(), stt_ms: 1, cleanup_ms: 0, stt_provider: "x".into(), cleanup_provider: None, no_cleanup: false };
                        append_history_to(&path, &e).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let s = std::fs::read_to_string(&path).unwrap();
        let mut ts: Vec<u64> = s.lines().map(|l| serde_json::from_str::<HistoryEntry>(l).unwrap().ts).collect();
        ts.sort();
        let mut want: Vec<u64> = (0..8u64).flat_map(|t| (0..25u64).map(move |i| t * 1000 + i)).collect();
        want.sort();
        assert_eq!(ts, want);
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files left behind");
    }

    #[test]
    fn concurrent_atomic_writes_never_tear_the_file() {
        let path = scratch("atomic").join("settings.json");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut s = Settings::default();
                    s.dictionary = vec![format!("w{t}"); 2000]; // big enough that a shared temp file would interleave
                    for _ in 0..20 {
                        write_atomic(&path, &serde_json::to_string_pretty(&s).unwrap()).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let s: Settings = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(s.dictionary.len(), 2000);
        assert!(s.dictionary.iter().all(|w| w == &s.dictionary[0]));
    }
}
