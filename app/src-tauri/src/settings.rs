//! Settings, presets, keychain secrets and history: everything Echotype persists.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const APP_ID: &str = "app.echotype.desktop";
const HISTORY_CAP: usize = 500;

// Same directories Tauri's path resolver returns for this identifier, so the CLI
// (which runs without an AppHandle) reads the same files as the app.
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(std::env::temp_dir).join(APP_ID)
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join(APP_ID)
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
    pub onboarded: bool,
}

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
            onboarded: false,
        }
    }
}

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
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                eprintln!("echotype: settings.json is invalid ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        std::fs::create_dir_all(config_dir()).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
    }
}

// ---------- presets ----------

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
    vec![
        p("openai", "OpenAI", true, true, "https://api.openai.com/v1", "gpt-4o-mini-transcribe", "gpt-4.1-nano", true),
        p("groq", "Groq", true, true, "https://api.groq.com/openai/v1", "whisper-large-v3-turbo", "openai/gpt-oss-20b", true),
        p("openrouter", "OpenRouter", false, true, "https://openrouter.ai/api/v1", "", "openai/gpt-4.1-nano", true),
        p("ollama", "Ollama", false, true, "http://localhost:11434/v1", "", "gemma3:1b", false),
        p("lmstudio", "LM Studio", false, true, "http://localhost:1234/v1", "", "google/gemma-3-1b", false),
        p("custom", "Custom (OpenAI-compatible)", true, true, "", "", "", false),
    ]
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
    } else {
        preset(provider).map(|p| p.base_url.to_string()).unwrap_or_default()
    };
    if url.is_empty() {
        return Err(format!("No base URL set for provider '{provider}'"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

// ---------- secrets (OS keychain) ----------

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(APP_ID, name).map_err(|e| e.to_string())
}

pub fn get_secret(name: &str) -> Option<String> {
    entry(name).ok()?.get_password().ok().filter(|s| !s.is_empty())
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
}

fn history_path() -> PathBuf {
    data_dir().join("history.jsonl")
}

pub fn append_history(entry: &HistoryEntry) -> Result<(), String> {
    let path = history_path();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = old.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect();
    lines.push(serde_json::to_string(entry).map_err(|e| e.to_string())?);
    let start = lines.len().saturating_sub(HISTORY_CAP);
    std::fs::create_dir_all(data_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&path, lines[start..].join("\n") + "\n").map_err(|e| e.to_string())
}

pub fn get_history(limit: usize) -> Vec<HistoryEntry> {
    let s = std::fs::read_to_string(history_path()).unwrap_or_default();
    s.lines().rev().filter_map(|l| serde_json::from_str(l).ok()).take(limit).collect()
}

pub fn clear_history() -> Result<(), String> {
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
        assert_eq!(ids, ["openai", "groq", "openrouter", "ollama", "lmstudio", "custom"]);
        assert_eq!(preset("openai").unwrap().default_stt_model, "gpt-4o-mini-transcribe");
        assert_eq!(preset("groq").unwrap().default_cleanup_model, "openai/gpt-oss-20b");
        assert!(!preset("openrouter").unwrap().stt);
        assert_eq!(base_url("groq", "").unwrap(), "https://api.groq.com/openai/v1");
        assert_eq!(base_url("openai", "http://box.lan:8000/v1").unwrap(), "https://api.openai.com/v1");
        assert_eq!(base_url("custom", "http://127.0.0.1:9/v1/").unwrap(), "http://127.0.0.1:9/v1");
        assert!(base_url("custom", " ").is_err());
    }
}
