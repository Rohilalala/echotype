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
    /// With `input_device` on System default and a Bluetooth default input (AirPods), record from
    /// the built-in mic instead, so the headphones keep their music quality (audio::pick_input).
    /// On for upgraders too: it never overrides a picked mic.
    pub prefer_builtin_mic: bool,
    /// Pause Music or Spotify while dictating and resume what Echotype paused (music.rs, macOS).
    pub pause_music: bool,
    /// Whisper language codes the user speaks, up to 3; empty is auto-detect (any language). One is
    /// transcribed as that language; with two or three, each clip goes to the likeliest of them.
    pub languages: Vec<String>,
    /// Builds before 2026-10-16 saved one code or "auto" here; migrate() moves it into `languages`.
    #[serde(skip_serializing)]
    pub language: String,
    /// How a language with its own script is typed: "native" (हिंदी) or "latin" (English letters,
    /// "Hinglish"). Only the languages in stt::ROMANISED have a "latin"; the rest stay native.
    pub script: String,
    pub translate_to_english: bool,
    pub local_api: LocalApi,
    pub stt: Stt,
    pub cleanup: Cleanup,
    pub dictionary: Vec<String>,
    pub replacements: Vec<Replacement>,
    pub restore_clipboard: bool,
    pub history_enabled: bool,
    /// Spoken "new line", "new paragraph", "bullet point" and "scratch that" act as commands in a
    /// dictation (cleanup::voice_commands). On for upgraders too.
    pub voice_commands: bool,
    /// Count words and speaking speed on this computer (stats.rs). Counts only; on for upgraders too.
    pub usage_stats: bool,
    /// Opt-in anonymous usage counts and crash reports for the beta (telemetry.rs). Off unless
    /// switched on; the only thing that ever sends them.
    pub share_telemetry: bool,
    /// Shown on the Stats page ("Aditya's stats"). Stays on this computer.
    pub display_name: String,
    pub onboarded: bool,
    /// The onboarding step on screen (1-6), so a relaunch resumes there. From step 4 on, a
    /// dictation into another app is pasted (the real-app try); before it, it only lands in
    /// onboarding's practice field.
    pub onboard_step: u8,
    /// macOS extra talk key next to the shortcut: "accelerator" (none) | "fn" | "left_control" |
    /// "right_control" and the same for option, command and shift (hotkey::KEYS). Hold or press it
    /// like the shortcut, per `mode`; it counts only when pressed alone.
    pub talk_key: String,
    /// Paste and copy the last result again. "" is unbound, the serde default, so upgraders
    /// are offered these instead of having keys taken; onboarding binds them for new installs.
    pub paste_last_hotkey: String,
    pub copy_last_hotkey: String,
    /// Command Mode: held like the dictation shortcut, it applies the spoken instruction to the
    /// selected text (or writes what it asks for). "" is unbound.
    pub command_hotkey: String,
    /// Launch at login. Off for upgraders; onboarding offers it pre-checked.
    pub autostart: bool,
    /// Start, stop and lock sounds.
    pub sounds: bool,
    /// One-time notices already dismissed: "upgrade_offer", "bluetooth_tip", "no_cleanup_banner".
    pub dismissed: Vec<String>,
    /// The idle pill (macOS): a thin bar bottom-centre while not dictating. On for upgraders too.
    pub pill_idle: bool,
    /// "Hide for 1 hour": the idle pill stays away until this time (ms since the epoch); 0 = not hidden.
    pub pill_hidden_until: u64,
    /// Let screenshots and screen sharing see the pill. Off = content protection (best effort).
    pub pill_in_recordings: bool,
    /// "Show words as you speak": the pill shows the last words recognised so far while recording
    /// (local speech model only). On for upgraders too.
    pub live_text: bool,
    /// The settings window's appearance: "system" (follows macOS, the default), "light" or "dark".
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
    /// The orb (Settings → Pill): its listening and thinking styles (thinking-orbs states, ORB_STYLES),
    /// how many dots it has (0.5..2 times the default) and their colour ("#rrggbb").
    pub orb_listen: String,
    pub orb_think: String,
    pub orb_detail: f64,
    pub orb_color: String,
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
    /// Watch for calls (calls.rs); off, nothing below applies.
    pub detect_meetings: bool,
    /// What a detected call does, per app.
    pub call_start: CallStart,
    /// Stop notes started for a call (automatically or from its offer) when that call ends.
    /// Off: ask. Notes started by hand are never stopped.
    pub meeting_auto_stop: bool,
    /// Builds before 2026-10-19: "ask" | "auto" here; migrate() moves it into `meeting_auto_stop`.
    #[serde(skip_serializing)]
    pub meeting_end: String,
    /// Meeting audio is deleted this many days after the meeting ends (7 or 30); 0 keeps it.
    /// The transcript, speakers and summary stay either way (meeting.rs prune_audio).
    pub meeting_audio_days: u32,
    /// A dictation's audio (play and Re-run in History) is deleted this many days after it was
    /// recorded (7, 14 or 30); 0 keeps none. Only while history is on (prune_clips).
    pub clip_days: u32,
}

/// Per calling app: "ask" (offer notes) | "start" (start them) | "never". calls::choice maps a call to its field.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CallStart {
    pub meet: String,
    pub teams: String,
    pub zoom: String,
    pub slack: String,
    pub facetime: String,
    pub webex: String,
    pub discord: String,
    pub other: String,
}

impl Default for CallStart {
    fn default() -> Self {
        let ask = || "ask".to_string();
        Self { meet: ask(), teams: ask(), zoom: ask(), slack: ask(), facetime: ask(), webex: ask(), discord: ask(), other: ask() }
    }
}

/// The orb's styles: the thinking-orbs states (app/src/vendor/thinking-orbs.js STATE_TO_MODE).
pub const ORB_STYLES: [&str; 9] = ["listening", "working", "searching", "solving", "connecting", "weaving", "composing", "breathing", "shaping"];

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
    /// Re-run last with another downloaded model: that model for this one run, Automatic or not. Never saved.
    #[serde(skip)]
    pub rerun_model: Option<String>,
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
    /// "Match the app you're typing in": the style comes from the app in front when recording
    /// starts (app_style). Off for everyone until switched on.
    pub match_app: bool,
    /// The user's picks per app, over APP_STYLES. A list, not a map: merge() and main.js merge3
    /// replace lists whole, so a removed row stays removed.
    pub app_styles: Vec<AppStyle>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct AppStyle {
    /// Bundle id ("com.tinyspeck.slackmacgap").
    pub id: String,
    pub name: String,
    /// A style, or "" for the user's own `Cleanup::style`.
    pub style: String,
}

/// Built-in styles by bundle id (an id ending in '.' matches the prefix). Browsers and every
/// other app keep the user's style.
pub const APP_STYLES: &[(&str, &str)] = &[
    ("com.tinyspeck.slackmacgap", "casual"),
    ("net.whatsapp.WhatsApp", "casual"),
    ("com.apple.MobileSMS", "casual"),
    ("com.hnc.Discord", "casual"),
    ("ru.keepcoder.Telegram", "casual"),
    ("com.tdesktop.Telegram", "casual"),
    ("com.apple.mail", "formal"),
    ("com.microsoft.Outlook", "formal"),
    ("com.apple.Terminal", "verbatim"),
    ("com.googlecode.iterm2", "verbatim"),
    ("dev.warp.Warp-Stable", "verbatim"),
    ("com.mitchellh.ghostty", "verbatim"),
    ("com.microsoft.VSCode", "verbatim"),
    ("com.microsoft.VSCodeInsiders", "verbatim"),
    ("com.todesktop.230313mzl4w4u92", "verbatim"), // Cursor
    ("com.apple.dt.Xcode", "verbatim"),
    ("com.jetbrains.", "verbatim"),
    ("dev.zed.Zed", "verbatim"),
];

pub fn builtin_style(id: &str) -> Option<&'static str> {
    APP_STYLES.iter().find(|(k, _)| if k.ends_with('.') { id.starts_with(k) } else { id == *k }).map(|(_, v)| *v)
}

/// The style for a dictation into app `id` (None: Echotype itself or unknown): the user's pick
/// for that app, else the built-in one, else the user's style. Switched off: always the user's style.
pub fn app_style<'a>(c: &'a Cleanup, id: Option<&str>) -> &'a str {
    let Some(id) = id.filter(|_| c.match_app) else { return &c.style };
    match c.app_styles.iter().find(|a| a.id == id) {
        Some(a) if a.style.is_empty() => &c.style,
        Some(a) => &a.style,
        None => builtin_style(id).unwrap_or(&c.style),
    }
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
            prefer_builtin_mic: true,
            pause_music: false,
            languages: vec![],
            language: String::new(),
            script: "native".into(),
            translate_to_english: false,
            local_api: LocalApi::default(),
            stt: Stt::default(),
            cleanup: Cleanup::default(),
            dictionary: vec![],
            replacements: vec![],
            restore_clipboard: true,
            history_enabled: true,
            voice_commands: true,
            usage_stats: true,
            share_telemetry: false,
            display_name: String::new(),
            onboarded: false,
            onboard_step: 0,
            talk_key: "accelerator".into(),
            paste_last_hotkey: String::new(),
            copy_last_hotkey: String::new(),
            // macOS only: elsewhere ⌥⇧ switches keyboard layouts, and no modifier wait guards the ⌘C.
            command_hotkey: if cfg!(target_os = "macos") { "Alt+Shift+Space" } else { "" }.into(),
            autostart: false,
            sounds: true,
            dismissed: vec![],
            pill_idle: true,
            pill_hidden_until: 0,
            pill_in_recordings: false,
            live_text: true,
            look: "system".into(),
            pill_positions: BTreeMap::new(),
            pill_screen: None,
            pill_follow_cursor: true,
            show_in: "pill".into(),
            pill_size: 1.5,
            ball_size: 1.0,
            sound_pack: "classic".into(),
            pill_fade: true,
            orb_listen: "listening".into(),
            orb_think: "working".into(),
            orb_detail: 1.0,
            orb_color: "#eef1ff".into(),
            advanced: false,
            cleaner_memory: "ready".into(),
            cleaner_idle_unload: false,
            cleaner_idle_minutes: 10,
            cleaner_context: 1024,
            cloud_moved: false,
            greetings: true,
            detect_meetings: true,
            call_start: CallStart::default(),
            meeting_auto_stop: true,
            meeting_end: String::new(),
            meeting_audio_days: 30,
            clip_days: 14,
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
            rerun_model: None,
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
            match_app: false,
            app_styles: vec![],
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
        // Upgraders keep what they had: "ask" stays asking, "auto" stops.
        match std::mem::take(&mut self.meeting_end).as_str() {
            "ask" => self.meeting_auto_stop = false,
            "auto" => self.meeting_auto_stop = true,
            _ => {}
        }
        let old = std::mem::take(&mut self.language);
        if self.languages.is_empty() && !matches!(old.as_str(), "" | "auto") {
            self.languages = vec![old];
        }
        let mut seen = vec![];
        self.languages.retain(|l| !matches!(l.as_str(), "" | "auto") && !seen.contains(l) && {
            seen.push(l.clone());
            true
        });
        self.languages.truncate(3);
        if !matches!(self.script.as_str(), "native" | "latin") {
            self.script = "native".into();
        }
        if !matches!(self.look.as_str(), "system" | "light" | "dark") {
            self.look = "system".into(); // the Classic, Ink and Foundry looks are gone: follow macOS
        }
        if self.local_api.port < MIN_PORT {
            self.local_api.port = LocalApi::default().port; // older builds saved 0 from a cleared field
        }
        let fit = |v: f64, lo: f64, hi: f64, def: f64| if v.is_finite() { v.clamp(lo, hi) } else { def };
        self.pill_size = fit(self.pill_size, 1.0, 2.0, 1.5);
        self.ball_size = fit(self.ball_size, 0.6, 2.0, 1.0);
        let style = |v: &mut String, def: &str| {
            if !ORB_STYLES.contains(&v.as_str()) {
                *v = def.into();
            }
        };
        style(&mut self.orb_listen, "listening");
        style(&mut self.orb_think, "working");
        self.orb_detail = fit(self.orb_detail, 0.5, 2.0, 1.0);
        let hex = self.orb_color.len() == 7 && self.orb_color.starts_with('#') && self.orb_color[1..].chars().all(|c| c.is_ascii_hexdigit());
        if !hex {
            self.orb_color = "#eef1ff".into();
        }
        if !SOUND_PACKS.contains(&self.sound_pack.as_str()) {
            self.sound_pack = "classic".into();
        }
        if !matches!(self.cleaner_memory.as_str(), "save" | "ready") {
            self.cleaner_memory = "ready".into();
        }
        if !matches!(self.meeting_audio_days, 0 | 7 | 30) {
            self.meeting_audio_days = 30;
        }
        if !matches!(self.clip_days, 0 | 7 | 14 | 30) {
            self.clip_days = 14;
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

/// A language picked from a menu: "auto" clears the list; a listed code leaves it; another joins
/// it, and a fourth replaces the oldest (Echotype listens for up to 3).
pub fn toggle_language(languages: &mut Vec<String>, code: &str) {
    if code == "auto" {
        languages.clear();
    } else if let Some(i) = languages.iter().position(|l| l == code) {
        languages.remove(i);
    } else {
        if languages.len() == 3 {
            languages.remove(0);
        }
        languages.push(code.to_string());
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
        p("echotype", "Echotype model (local)", false, true, "", "", "", false),
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
    /// The cleanup the guard rejected; `text` is the words as spoken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned: Option<String>,
    /// The text before the first Re-run from History, so the user can go back to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<String>,
    /// Audio from a dictation the app quit or crashed during (recover): no text until it is transcribed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recovered: bool,
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

/// A fix from the History page: replaces the text of the entry recorded at `ts`.
/// Replaces one entry's text; returns the text it had, so the in-memory "last transcript" can follow.
pub fn update_history(ts: u64, text: &str) -> Result<String, String> {
    update_history_in(&history_path(), ts, text)
}

fn update_history_in(path: &Path, ts: u64, text: &str) -> Result<String, String> {
    edit_history_in(path, ts, |e| e.text = text.to_string()).map(|was| was.text)
}

/// A re-run of entry `new.ts`: its result replaces the entry's, and the text it had before its
/// first re-run is kept as `original`.
pub fn rerun_history(new: &HistoryEntry) -> Result<(), String> {
    rerun_history_in(&history_path(), new)
}

fn rerun_history_in(path: &Path, new: &HistoryEntry) -> Result<(), String> {
    // A recovered dictation had no text to go back to.
    edit_history_in(path, new.ts, |e| *e = HistoryEntry { original: e.original.take().or_else(|| Some(e.text.clone()).filter(|t| !t.is_empty())), ..new.clone() }).map(drop)
}

/// Changes the entry recorded at `ts` and returns it as it was.
fn edit_history_in(path: &Path, ts: u64, edit: impl FnOnce(&mut HistoryEntry)) -> Result<HistoryEntry, String> {
    let _g = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let (mut found, mut edit) = (None, Some(edit));
    let mut lines = Vec::new();
    for l in old.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<HistoryEntry>(l) {
            Ok(mut e) if e.ts == ts && found.is_none() => {
                found = Some(e.clone());
                if let Some(f) = edit.take() {
                    f(&mut e);
                }
                lines.push(serde_json::to_string(&e).map_err(|e| e.to_string())?);
            }
            _ => lines.push(l.to_string()), // other entries stay byte for byte
        }
    }
    let Some(was) = found else { return Err("That entry is no longer in history.".into()) };
    write_atomic(path, &(lines.join("\n") + "\n"))?;
    Ok(was)
}

pub fn clear_history() -> Result<(), String> {
    let _g = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    clear_clips();
    match std::fs::remove_file(history_path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

// ---------- the audio of recent dictations, for play and Re-run in History ----------

/// At most this many dictations keep their audio (16 kHz 16-bit WAV, about 320 KB a 10 s
/// dictation), however recent: the History page lists 200.
const CLIPS_KEPT: usize = 200;
const DAY_MS: u64 = 86_400_000;

fn clips_dir() -> PathBuf {
    data_dir().join("clips")
}

/// The audio of the history entry recorded at `ts`, if it is still kept.
pub fn clip_path(ts: u64) -> PathBuf {
    clips_dir().join(format!("{ts}.wav"))
}

/// Keeps the audio of entry `ts`; only the newest CLIPS_KEPT stay. Only while history is on.
pub fn save_clip(ts: u64, wav: &[u8]) -> Result<(), String> {
    save_clip_in(&clips_dir(), ts, wav)
}

/// Every kept clip in `dir` as (ts, path); a recording in progress (`<ts>.part.wav`) is not one.
fn clips_in(dir: &Path) -> Vec<(u64, PathBuf)> {
    std::fs::read_dir(dir).map(|rd| rd.filter_map(|f| f.ok().map(|f| f.path())).filter_map(|p| Some((p.file_stem()?.to_str()?.parse().ok()?, p))).collect()).unwrap_or_default()
}

fn save_clip_in(dir: &Path, ts: u64, wav: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{ts}.wav")), wav).map_err(|e| e.to_string())?;
    let mut kept = clips_in(dir);
    kept.sort_unstable_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    for (_, p) in kept.iter().skip(CLIPS_KEPT) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// The ts of every entry whose audio is kept (History shows a play button for them).
pub fn kept_clips() -> Vec<u64> {
    clips_in(&clips_dir()).into_iter().map(|(ts, _)| ts).collect()
}

/// Deletes the audio of dictations recorded more than `days` ago (Settings → Keep dictation
/// audio); 0 deletes all of it. The entries and their text stay.
pub fn prune_clips(days: u32) {
    prune_clips_in(&clips_dir(), days, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64));
}

fn prune_clips_in(dir: &Path, days: u32, now_ms: u64) {
    for (ts, p) in clips_in(dir) {
        if now_ms.saturating_sub(ts) > u64::from(days) * DAY_MS {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Every kept clip: history was cleared or switched off.
pub fn clear_clips() {
    let _ = std::fs::remove_dir_all(clips_dir());
}

// ---------- recovery: a dictation the app quit or crashed during ----------

/// Where recording `ts` is written while it goes (lib.rs Spool); deleted once it is transcribed.
pub fn recording_path(ts: u64) -> PathBuf {
    clips_dir().join(format!("{ts}.part.wav"))
}

/// Under 0.3 s of audio (WAV header + 16-bit samples) is nothing to transcribe.
const MIN_RECOVERED_BYTES: u64 = 44 + 2 * 16_000 * 3 / 10;

/// At launch: each recording left behind becomes that entry's audio and a "Recovered dictation"
/// in History, to transcribe from there. With `keep` off (history off, or no audio kept) they are
/// deleted. Returns the entries added.
pub fn recover(keep: bool) -> Vec<u64> {
    recover_in(&clips_dir(), &history_path(), keep)
}

fn recover_in(dir: &Path, history: &Path, keep: bool) -> Vec<u64> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let parts: Vec<(u64, PathBuf)> = rd.filter_map(|f| f.ok().map(|f| f.path())).filter_map(|p| Some((p.file_name()?.to_str()?.strip_suffix(".part.wav")?.parse().ok()?, p))).collect();
    let mut out = vec![];
    for (ts, part) in parts {
        let big = std::fs::metadata(&part).is_ok_and(|m| m.len() >= MIN_RECOVERED_BYTES);
        if !keep || !big || std::fs::rename(&part, dir.join(format!("{ts}.wav"))).is_err() {
            let _ = std::fs::remove_file(&part);
            continue;
        }
        let entry = HistoryEntry { ts, raw: String::new(), text: String::new(), stt_ms: 0, cleanup_ms: 0, stt_provider: String::new(), cleanup_provider: None, no_cleanup: false, cleaned: None, original: None, recovered: true };
        match append_history_to(history, &entry) {
            Ok(()) => out.push(ts),
            Err(e) => eprintln!("echotype: could not add a recovered dictation to history: {e}"),
        }
    }
    out
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
        assert!(s.detect_meetings && s.call_start == CallStart::default() && s.call_start.zoom == "ask", "calls are offered, never recorded unasked");
        assert!(s.meeting_auto_stop);
        assert_eq!(s.meeting_audio_days, 30);
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
        let mut s: Settings = serde_json::from_str(old).unwrap();
        s.migrate(); // as load() does
        assert!(s.onboarded && s.mode == "toggle" && s.local_api.enabled);
        assert!(s.languages.is_empty(), "\"auto\" loads as auto-detect");
        assert_eq!((s.paste_last_hotkey.as_str(), s.copy_last_hotkey.as_str()), ("", ""), "upgraders get no new global shortcuts");
        assert!(!s.autostart, "upgraders are offered launch at login, not opted in");
        assert_eq!(s.talk_key, "accelerator");
        assert!(s.prefer_builtin_mic && s.input_device.is_none(), "System default skips Bluetooth mics for upgraders too");
        assert!(!s.pause_music, "music is never paused unasked");
        assert!(s.sounds && s.dismissed.is_empty());
        assert!(s.pill_idle && s.pill_hidden_until == 0 && !s.pill_in_recordings, "the idle pill is on for upgraders, hidden from capture");
        assert!(s.live_text, "upgraders see their words as they speak");
        assert_eq!(s.look, "system", "upgraders follow the system appearance");
        let mut f: Settings = serde_json::from_str(r#"{"look":"foundry","look_chosen":true}"#).unwrap();
        f.migrate();
        assert_eq!(f.look, "system", "a saved Foundry look loads as System");
        let mut ink: Settings = serde_json::from_str(r#"{"look":"v2"}"#).unwrap();
        ink.migrate();
        assert_eq!(ink.look, "system", "a saved Ink look loads as System");
        assert!(s.pill_positions.is_empty() && s.show_in == "pill", "upgraders keep the pill, bottom-centre");
        assert!(s.pill_follow_cursor && s.pill_screen.is_none(), "...on the screen they are using");
        assert!(!s.cleanup.match_app && s.cleanup.app_styles.is_empty(), "upgraders keep one style everywhere");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn the_old_call_end_choice_carries_over() {
        let load = |j: &str| {
            let mut s: Settings = serde_json::from_str(j).unwrap();
            s.migrate();
            s
        };
        assert!(!load(r#"{"meeting_end":"ask"}"#).meeting_auto_stop, "upgraders who were asked are still asked");
        assert!(load(r#"{"meeting_end":"auto","meeting_auto_stop":false}"#).meeting_auto_stop);
        let fresh = load("{}");
        assert!(fresh.meeting_auto_stop && fresh.call_start == CallStart::default(), "new installs stop with the call, ask per app");
        assert!(!serde_json::to_string(&fresh).unwrap().contains("meeting_end"), "the old field is not written back");
        let saved = load(r#"{"meeting_auto_stop":false,"call_start":{"zoom":"start"}}"#);
        assert!(!saved.meeting_auto_stop && saved.call_start.zoom == "start" && saved.call_start.teams == "ask", "a saved choice survives the next load");
    }

    #[test]
    fn languages_carry_over_and_stay_at_three() {
        let mut hi: Settings = serde_json::from_str(r#"{"language":"hi"}"#).unwrap();
        hi.migrate();
        assert_eq!(hi.languages, ["hi"], "one saved language carries over");
        let mut auto: Settings = serde_json::from_str(r#"{"language":"auto"}"#).unwrap();
        auto.migrate();
        assert!(auto.languages.is_empty(), "auto is the empty list");
        assert!(!serde_json::to_string(&hi).unwrap().contains("\"language\""), "the old field is not written back");
        let mut many: Settings = serde_json::from_str(r#"{"languages":["en","hi","en","","auto","fr","de"]}"#).unwrap();
        many.migrate();
        assert_eq!(many.languages, ["en", "hi", "fr"], "deduplicated, at most 3");
        let mut l: Vec<String> = vec![];
        for c in ["en", "hi", "fr", "de"] {
            toggle_language(&mut l, c);
        }
        assert_eq!(l, ["hi", "fr", "de"], "a fourth replaces the oldest");
        toggle_language(&mut l, "fr");
        assert_eq!(l, ["hi", "de"]);
        toggle_language(&mut l, "auto");
        assert!(l.is_empty());
    }

    #[test]
    fn the_script_choice_defaults_to_native() {
        let load = |j: &str| {
            let mut s: Settings = serde_json::from_str(j).unwrap();
            s.migrate();
            s.script
        };
        assert_eq!(load("{}"), "native", "upgraders keep their language's own script");
        assert_eq!(load(r#"{"script":"latin"}"#), "latin");
        assert_eq!(load(r#"{"script":"cyrillic"}"#), "native", "an unknown value loads as native");
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
    fn the_app_in_front_picks_the_style_only_when_switched_on() {
        let mut c = Cleanup { style: "formal".into(), ..Cleanup::default() };
        let slack = Some("com.tinyspeck.slackmacgap");
        assert_eq!(app_style(&c, slack), "formal", "off: the user's style everywhere");
        c.match_app = true;
        assert_eq!(app_style(&c, slack), "casual");
        assert_eq!(app_style(&c, Some("com.apple.mail")), "formal");
        assert_eq!(app_style(&c, Some("com.apple.Terminal")), "verbatim");
        assert_eq!(app_style(&c, Some("com.jetbrains.pycharm")), "verbatim", "any JetBrains IDE");
        assert_eq!(app_style(&c, Some("com.jetbrains")), "formal", "the prefix needs its dot");
        assert_eq!(app_style(&c, Some("com.google.Chrome")), "formal", "browsers keep the user's style");
        assert_eq!(app_style(&c, Some("")), "formal");
        assert_eq!(app_style(&c, None), "formal", "Echotype itself or not known");
        let row = |id: &str, style: &str| AppStyle { id: id.into(), name: id.into(), style: style.into() };
        c.app_styles = vec![row("com.tinyspeck.slackmacgap", "formal"), row("com.apple.Terminal", ""), row("com.google.Chrome", "verbatim")];
        assert_eq!(app_style(&c, slack), "formal", "the user's pick beats the built-in one");
        assert_eq!(app_style(&c, Some("com.apple.Terminal")), "formal", "\"\" is the user's style");
        assert_eq!(app_style(&c, Some("com.google.Chrome")), "verbatim");
        c.match_app = false;
        assert_eq!(app_style(&c, Some("com.google.Chrome")), "formal", "picks only apply while switched on");
    }

    #[test]
    fn settings_round_trip() {
        let mut s = Settings::default();
        s.dictionary = vec!["Arnav".into()];
        s.replacements = vec![Replacement { from: "my email".into(), to: "me@example.com".into() }];
        s.cleanup.app_styles = vec![AppStyle { id: "com.apple.mail".into(), name: "Mail".into(), style: "casual".into() }];
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
                        let e = HistoryEntry { ts: t * 1000 + i, raw: "r".into(), text: "t".into(), stt_ms: 1, cleanup_ms: 0, stt_provider: "x".into(), cleanup_provider: None, no_cleanup: false, cleaned: None, original: None, recovered: false };
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
    fn update_history_changes_only_that_entry() {
        let path = scratch("history-update").join("history.jsonl");
        for ts in 1..=3u64 {
            let e = HistoryEntry { ts, cleaned: None, raw: format!("r{ts}"), text: format!("t{ts}"), stt_ms: 1, cleanup_ms: 0, stt_provider: "x".into(), cleanup_provider: None, no_cleanup: false, original: None, recovered: false };
            append_history_to(&path, &e).unwrap();
        }
        let before: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().map(String::from).collect();
        assert_eq!(update_history_in(&path, 2, "Ping Midhun").unwrap(), "t2", "returns what it replaced, for Paste last");
        let after: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().map(String::from).collect();
        assert_eq!((after[0].as_str(), after[2].as_str()), (before[0].as_str(), before[2].as_str()));
        let e: HistoryEntry = serde_json::from_str(&after[1]).unwrap();
        assert_eq!((e.ts, e.raw.as_str(), e.text.as_str()), (2, "r2", "Ping Midhun"));
        assert!(update_history_in(&path, 9, "x").is_err(), "a missing entry is an error, not a silent no-op");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "temp files left behind");
    }

    #[test]
    fn a_rerun_replaces_the_entry_and_keeps_its_first_text() {
        let path = scratch("history-rerun").join("history.jsonl");
        let e = |ts, text: &str| HistoryEntry { ts, raw: "r".into(), text: text.into(), stt_ms: 1, cleanup_ms: 0, stt_provider: "local:tiny.en".into(), cleanup_provider: None, no_cleanup: false, cleaned: None, original: None, recovered: false };
        for ts in 1..=2 {
            append_history_to(&path, &e(ts, "first")).unwrap();
        }
        let rerun = HistoryEntry { stt_provider: "local:large-v3-turbo-q5_0".into(), ..e(1, "second") };
        rerun_history_in(&path, &rerun).unwrap();
        rerun_history_in(&path, &HistoryEntry { text: "third".into(), ..rerun.clone() }).unwrap();
        let all: Vec<HistoryEntry> = std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!((all[0].text.as_str(), all[0].original.as_deref(), all[0].stt_provider.as_str()), ("third", Some("first"), "local:large-v3-turbo-q5_0"), "the text before the first re-run");
        assert_eq!(all[1], e(2, "first"), "other entries untouched");
        assert!(rerun_history_in(&path, &e(9, "x")).is_err());
        assert!(!std::fs::read_to_string(&path).unwrap().lines().nth(1).unwrap().contains("original"), "no field until a re-run");
    }

    #[test]
    fn only_the_newest_clips_are_kept() {
        let dir = scratch("clips");
        let n = CLIPS_KEPT as u64;
        for ts in (1..=n + 5).rev().chain([n + 10]) {
            save_clip_in(&dir, ts * 1000, b"RIFF").unwrap();
        }
        std::fs::write(dir.join("notes.txt"), "").unwrap(); // not a clip: never counted or removed
        std::fs::write(dir.join("1.part.wav"), "").unwrap(); // a recording in progress, likewise
        let mut kept: Vec<u64> = clips_in(&dir).into_iter().map(|(ts, _)| ts).collect();
        kept.sort();
        assert_eq!(kept, (7..=n + 5).chain([n + 10]).map(|t| t * 1000).collect::<Vec<_>>(), "the newest by time, whatever order they were written in");
        assert!(dir.join("notes.txt").exists() && dir.join("1.part.wav").exists());
    }

    #[test]
    fn old_clips_go_after_their_days_and_zero_keeps_none() {
        let dir = scratch("clips-prune");
        let now = 100 * DAY_MS;
        for days_ago in [1, 6, 8, 13, 15, 29, 31] {
            save_clip_in(&dir, now - days_ago * DAY_MS, b"RIFF").unwrap();
        }
        std::fs::write(dir.join(format!("{}.part.wav", now - 40 * DAY_MS)), "").unwrap();
        let left = |days| {
            prune_clips_in(&dir, days, now);
            let mut v: Vec<u64> = clips_in(&dir).into_iter().map(|(ts, _)| (now - ts) / DAY_MS).collect();
            v.sort();
            v
        };
        assert_eq!(left(30), [1, 6, 8, 13, 15, 29]);
        assert_eq!(left(14), [1, 6, 8, 13]);
        assert_eq!(left(7), [1, 6]);
        assert_eq!(left(0), Vec::<u64>::new(), "0: no audio kept");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "the recording in progress is not pruned");
    }

    #[test]
    fn a_recording_left_behind_comes_back_as_a_recovered_entry() {
        let dir = scratch("recover");
        let history = dir.join("history.jsonl");
        let clips = dir.join("clips");
        std::fs::create_dir_all(&clips).unwrap();
        let wav = crate::audio::wav_bytes(&vec![0.1; 16_000]);
        std::fs::write(clips.join("5000.part.wav"), &wav).unwrap();
        std::fs::write(clips.join("6000.part.wav"), crate::audio::wav_bytes(&[0.1; 100])).unwrap(); // under 0.3 s
        std::fs::write(clips.join("7000.wav"), &wav).unwrap(); // an ordinary kept clip
        assert_eq!(recover_in(&clips, &history, true), [5000]);
        assert_eq!(std::fs::read(clips.join("5000.wav")).unwrap(), wav, "the audio becomes the entry's clip");
        assert!(!clips.join("5000.part.wav").exists() && !clips.join("6000.part.wav").exists());
        let e: HistoryEntry = serde_json::from_str(std::fs::read_to_string(&history).unwrap().trim()).unwrap();
        assert!(e.recovered && e.ts == 5000 && e.text.is_empty());
        assert!(recover_in(&clips, &history, true).is_empty(), "only once");
        // Transcribing it replaces the entry; there was no text to keep as the original.
        rerun_history_in(&history, &HistoryEntry { text: "Hello".into(), raw: "hello".into(), recovered: false, ..e }).unwrap();
        let e: HistoryEntry = serde_json::from_str(std::fs::read_to_string(&history).unwrap().trim()).unwrap();
        assert!(!e.recovered && e.text == "Hello" && e.original.is_none());
        std::fs::write(clips.join("8000.part.wav"), &wav).unwrap();
        assert!(recover_in(&clips, &history, false).is_empty() && !clips.join("8000.part.wav").exists(), "history off: deleted, not kept");
        assert!(clips.join("7000.wav").exists());
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
