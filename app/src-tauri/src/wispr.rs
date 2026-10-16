//! Import from Wispr Flow: what it keeps on this machine, mapped onto Echotype's settings.
//! Wispr is an Electron app. Its preferences are `config.json` (`prefs.user`) and its words and
//! snippets are the `Dictionary` table of `flow.sqlite`, both in its app-data folder. Only read,
//! and the database through a copy, so Wispr running is never disturbed.

use crate::settings::{AppStyle, Replacement, Settings};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What an import would bring over. Every field is optional: Wispr may not have set it.
#[derive(Serialize, Debug, Default, Clone, PartialEq)]
pub struct Found {
    pub words: Vec<String>,
    pub replacements: Vec<Replacement>,
    pub languages: Vec<String>,
    pub app_styles: Vec<AppStyle>,
    pub talk_key: Option<String>,
    pub sounds: Option<bool>,
    pub pause_music: Option<bool>,
    pub autostart: Option<bool>,
    pub look: Option<String>,
    pub display_name: Option<String>,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        *self == Found::default()
    }
}

/// Wispr Flow's app-data folder, if it is on this machine.
pub fn folder() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = dirs::home_dir()?.join("Library/Application Support/Wispr Flow");
    #[cfg(target_os = "windows")]
    let dir = dirs::config_dir()?.join("Wispr Flow"); // %APPDATA%
    #[cfg(target_os = "linux")]
    let dir = dirs::config_dir()?.join("Wispr Flow");
    dir.join("config.json").is_file().then_some(dir)
}

/// Everything importable from Wispr Flow on this machine; None when it is not installed.
pub fn detect() -> Option<Found> {
    let dir = folder()?;
    let config: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json")).ok()?).ok()?;
    let mut f = from_config(&config["prefs"]["user"], app_bundle_id);
    let rows = dictionary_rows(&dir.join("flow.sqlite")).unwrap_or_default();
    let (words, replacements) = from_dictionary(&rows);
    f.words = words;
    f.replacements = replacements;
    Some(f)
}

/// Wispr's `prefs.user` mapped onto Echotype. `bundle_id` finds an app by its name ("Slack").
pub fn from_config(u: &Value, bundle_id: impl Fn(&str) -> Option<String>) -> Found {
    let mut f = Found::default();
    // "en", "engb", "hi", "hinglish": whisper wants the two-letter code; Hinglish is spoken Hindi.
    for code in u["selectedLanguages"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let c = if code.eq_ignore_ascii_case("hinglish") { "hi".to_string() } else { code.chars().take(2).collect::<String>().to_lowercase() };
        if whisper_rs::get_lang_id(&c).is_some() && !f.languages.contains(&c) && f.languages.len() < 3 {
            f.languages.push(c);
        }
    }
    // Per-category styles ("Work Messages": Slack, Teams; casual), applied to the apps installed here.
    for voice in u["userVoices"].as_object().into_iter().flat_map(|m| m.values()) {
        let style = match voice["stylePreference"].as_str().unwrap_or_default() {
            "formal" => "formal",
            "casual" | "very_casual" | "excited" => "casual",
            _ => continue,
        };
        for name in voice["appNames"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(id) = bundle_id(name)
                && !f.app_styles.iter().any(|a| a.id == id)
            {
                f.app_styles.push(AppStyle { id, name: name.to_string(), style: style.to_string() });
            }
        }
    }
    // Push-to-talk: a single key (macOS keycode) Echotype can use as its talk key.
    f.talk_key = u["shortcuts"].as_object().into_iter().flatten().filter(|(_, v)| v.as_str() == Some("ptt")).find_map(|(k, _)| crate::hotkey::talk_key_for(k.parse().ok()?)).map(String::from);
    f.sounds = u["enableSounds"].as_bool();
    f.pause_music = u["shouldPauseAudio"].as_bool();
    f.autostart = u["openAtLogin"].as_bool();
    f.look = u["theme"].as_str().filter(|t| matches!(*t, "light" | "dark" | "system")).map(String::from);
    f.display_name = u["firstName"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(String::from);
    f
}

/// Words (phrases without a replacement) and replacements (Wispr's snippets: say the phrase, get
/// the text). Deleted rows and Wispr's own seeded snippets (source "default": its referral link,
/// its prompt templates) are left behind; a replacement longer than a sentence is a prompt, not one.
pub fn from_dictionary(rows: &[Value]) -> (Vec<String>, Vec<Replacement>) {
    let (mut words, mut reps) = (Vec::<String>::new(), Vec::<Replacement>::new());
    for r in rows {
        let phrase = r["phrase"].as_str().unwrap_or_default().trim().trim_end_matches('.').to_string();
        if phrase.is_empty() || r["isDeleted"].as_i64() == Some(1) || r["source"].as_str() == Some("default") {
            continue;
        }
        match r["replacement"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
            Some(to) if to.len() <= 200 => {
                if !reps.iter().any(|x| x.from.eq_ignore_ascii_case(&phrase)) {
                    reps.push(Replacement { from: phrase, to: to.to_string() });
                }
            }
            Some(_) => {}
            None => {
                if !words.iter().any(|w| w.eq_ignore_ascii_case(&phrase)) {
                    words.push(phrase);
                }
            }
        }
    }
    (words, reps)
}

/// The Dictionary table, read from a copy of the database (and its WAL) with the system sqlite3.
// ponytail: macOS ships /usr/bin/sqlite3; Windows and Linux import the preferences only. A sqlite
// crate if Wispr users there ask for their dictionary.
fn dictionary_rows(db: &Path) -> Option<Vec<Value>> {
    if !cfg!(target_os = "macos") || !db.is_file() {
        return None;
    }
    let tmp = std::env::temp_dir().join(format!("echotype-wispr-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).ok()?;
    let copy = tmp.join("flow.sqlite");
    for ext in ["", "-wal", "-shm"] {
        let from = PathBuf::from(format!("{}{ext}", db.display()));
        if from.is_file() {
            std::fs::copy(&from, format!("{}{ext}", copy.display())).ok()?;
        }
    }
    let out = std::process::Command::new("/usr/bin/sqlite3")
        .args(["-readonly", "-json"])
        .arg(&copy)
        .arg("select phrase, replacement, isDeleted, source from Dictionary")
        .output()
        .ok();
    let _ = std::fs::remove_dir_all(&tmp);
    let out = out.filter(|o| o.status.success())?;
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Some(vec![]); // sqlite3 prints nothing for no rows
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// An installed app's bundle id by its name, from its Info.plist.
fn app_bundle_id(name: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let home = dirs::home_dir().unwrap_or_default();
    let dirs = [PathBuf::from("/Applications"), PathBuf::from("/System/Applications"), home.join("Applications")];
    let app = dirs.iter().map(|d| d.join(format!("{name}.app"))).find(|p| p.is_dir())?;
    let out = std::process::Command::new("/usr/bin/plutil").args(["-extract", "CFBundleIdentifier", "raw"]).arg(app.join("Contents/Info.plist")).output().ok()?;
    let id = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !id.is_empty()).then_some(id)
}

/// Brings `f` into `s` without taking anything away: words and replacements are added if new,
/// languages and the talk key only fill what is still at its default, app styles are added and
/// switch "Match the app you're typing in" on. Returns what changed, for the confirmation.
pub fn apply(s: &mut Settings, f: &Found) -> Vec<String> {
    let mut done = vec![];
    let new_words: Vec<&String> = f.words.iter().filter(|w| !s.dictionary.iter().any(|d| d.eq_ignore_ascii_case(w))).collect();
    if !new_words.is_empty() {
        done.push(format!("{} dictionary {}", new_words.len(), if new_words.len() == 1 { "word" } else { "words" }));
        s.dictionary.extend(new_words.into_iter().cloned());
    }
    let new_reps: Vec<&Replacement> = f.replacements.iter().filter(|r| !s.replacements.iter().any(|x| x.from.eq_ignore_ascii_case(&r.from))).collect();
    if !new_reps.is_empty() {
        done.push(format!("{} {}", new_reps.len(), if new_reps.len() == 1 { "snippet" } else { "snippets" }));
        s.replacements.extend(new_reps.into_iter().cloned());
    }
    if s.languages.is_empty() && !f.languages.is_empty() {
        s.languages = f.languages.clone();
        done.push("your languages".into());
    }
    let new_styles: Vec<&AppStyle> = f.app_styles.iter().filter(|a| !s.cleanup.app_styles.iter().any(|x| x.id == a.id)).collect();
    if !new_styles.is_empty() {
        done.push(format!("styles for {}", new_styles.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")));
        s.cleanup.app_styles.extend(new_styles.into_iter().cloned());
        s.cleanup.match_app = true;
    }
    if let Some(k) = f.talk_key.as_ref().filter(|_| s.talk_key == "accelerator") {
        s.talk_key = k.clone();
        done.push(format!("your talk key ({})", crate::hotkey::talk_key_label(k)));
    }
    if s.display_name.trim().is_empty()
        && let Some(n) = &f.display_name
    {
        s.display_name = n.clone();
    }
    if let Some(v) = f.sounds {
        s.sounds = v;
    }
    if let Some(v) = f.pause_music {
        s.pause_music = v;
    }
    if let Some(v) = f.autostart {
        s.autostart = v;
    }
    if let Some(v) = &f.look {
        s.look = v.clone();
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the owner's own Wispr Flow 1.6 config (values changed).
    fn sample() -> Value {
        serde_json::json!({
            "selectedLanguages": ["en", "engb", "hinglish", "xx"],
            "userVoices": {
                "work": { "name": "Work Messages", "appNames": ["Slack", "Microsoft Teams"], "stylePreference": "casual" },
                "email": { "name": "Email", "appNames": ["Gmail", "Microsoft Outlook"], "stylePreference": "formal" },
                "other": { "name": "Others", "appNames": [], "stylePreference": "casual" }
            },
            "shortcuts": { "53": "dismiss", "59": "ptt", "4098": "ptt", "55+59+9": "paste_last_text" },
            "enableSounds": false, "shouldPauseAudio": true, "openAtLogin": true, "theme": "dark", "firstName": " Aditya "
        })
    }

    #[test]
    fn wispr_preferences_map_onto_echotype() {
        let installed = |n: &str| match n {
            "Slack" => Some("com.tinyspeck.slackmacgap".to_string()),
            "Microsoft Outlook" => Some("com.microsoft.Outlook".to_string()),
            _ => None, // Teams not installed; Gmail is a website
        };
        let f = from_config(&sample(), installed);
        assert_eq!(f.languages, ["en", "hi"], "en-GB is English, Hinglish is Hindi, an unknown code is dropped");
        let mut styles: Vec<_> = f.app_styles.iter().map(|a| (a.name.as_str(), a.style.as_str())).collect();
        styles.sort();
        assert_eq!(styles, [("Microsoft Outlook", "formal"), ("Slack", "casual")], "installed apps only");
        assert_eq!(f.talk_key.as_deref(), Some("left_control"), "Wispr's push-to-talk on Left Control (keycode 59)");
        assert_eq!((f.sounds, f.pause_music, f.autostart), (Some(false), Some(true), Some(true)));
        assert_eq!((f.look.as_deref(), f.display_name.as_deref()), (Some("dark"), Some("Aditya")));
        assert!(from_config(&serde_json::json!({}), |_| None).is_empty(), "an empty config imports nothing");
    }

    #[test]
    fn dictionary_words_and_snippets_but_not_wisprs_own() {
        let rows = serde_json::json!([
            { "phrase": "Midhun", "replacement": null, "isDeleted": 0, "source": "manual" },
            { "phrase": "xoxoday", "replacement": null, "isDeleted": 0, "source": "user_edits" },
            { "phrase": "Anshi.", "replacement": null, "isDeleted": 0, "source": "manual" },
            { "phrase": "Old", "replacement": null, "isDeleted": 1, "source": "manual" },
            { "phrase": "my email address", "replacement": "me@example.com", "isDeleted": 0, "source": "manual" },
            { "phrase": "my Flow referral", "replacement": "Hey, use my referral link", "isDeleted": 0, "source": "default" },
            { "phrase": "long prompt", "replacement": "x".repeat(300), "isDeleted": 0, "source": "manual" }
        ]);
        let (words, reps) = from_dictionary(rows.as_array().unwrap());
        assert_eq!(words, ["Midhun", "xoxoday", "Anshi"]);
        assert_eq!(reps, [Replacement { from: "my email address".into(), to: "me@example.com".into() }]);
    }

    #[test]
    fn importing_adds_and_never_takes_away() {
        let mut s = Settings::default();
        s.dictionary = vec!["Midhun".into()];
        s.replacements = vec![Replacement { from: "My Email Address".into(), to: "old@example.com".into() }];
        s.languages = vec![];
        let f = Found {
            words: vec!["midhun".into(), "Xoxoday".into()],
            replacements: vec![Replacement { from: "my email address".into(), to: "me@example.com".into() }, Replacement { from: "btw".into(), to: "by the way".into() }],
            languages: vec!["en".into(), "hi".into()],
            app_styles: vec![AppStyle { id: "com.tinyspeck.slackmacgap".into(), name: "Slack".into(), style: "casual".into() }],
            talk_key: Some("left_control".into()),
            display_name: Some("Aditya".into()),
            ..Default::default()
        };
        let done = apply(&mut s, &f);
        assert_eq!(s.dictionary, ["Midhun", "Xoxoday"], "case-insensitive: Midhun is already there");
        assert_eq!(s.replacements.len(), 2, "the user's own email replacement is kept, btw added");
        assert_eq!(s.replacements[0].to, "old@example.com");
        assert_eq!(s.languages, ["en", "hi"]);
        assert!(s.cleanup.match_app && s.cleanup.app_styles.len() == 1);
        assert_eq!((s.talk_key.as_str(), s.display_name.as_str()), ("left_control", "Aditya"));
        assert_eq!(done.len(), 5, "{done:?}");
        // A second import changes nothing; set languages and talk key are not overwritten.
        s.languages = vec!["fr".into()];
        assert!(apply(&mut s, &f).is_empty());
        assert_eq!(s.languages, ["fr"]);
    }

    /// The real Wispr Flow on this machine, read-only: `cargo test wispr_on_this_mac -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn wispr_on_this_mac() {
        eprintln!("{:#?}", detect());
    }
}
