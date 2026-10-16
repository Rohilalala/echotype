//! What the owner tells every copy of Echotype: notices for the sidebar panel and the oldest
//! version still allowed to run (`min_version`), from app.json beside the models in the R2 bucket.
//! Fetched at launch and every 6 hours. The last copy is kept on disk, so a retired version stays
//! retired offline; with no copy at all (first launch offline), nothing is blocked.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// Edit this file in the bucket to send a notice or retire a version; no app release needed.
pub const URL: &str = "https://pub-da62fbe34de644598313554b0a5ccc51.r2.dev/app.json";
const EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Notice {
    /// Stable id: a dismissed notice stays dismissed (Settings.dismissed holds "notice:<id>").
    pub id: String,
    pub title: String,
    pub body: String,
    /// The button, e.g. "Update" or "Open Models"; none when empty.
    pub action_label: String,
    /// "update", "page:<page>" or an https:// link.
    pub action: String,
    /// Shown only to versions in this range (inclusive); empty for any.
    pub min_version: String,
    pub max_version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Remote {
    /// Versions older than this stop dictating and transcribing meetings until they update.
    pub min_version: String,
    pub notices: Vec<Notice>,
}

static BLOCKED: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<Option<Remote>> = Mutex::new(None);

/// "1.2.3" (anything after '-' or '+' ignored, missing parts 0) for comparing versions.
pub fn version(v: &str) -> (u64, u64, u64) {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next().unwrap_or("");
    let mut n = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0), n.next().unwrap_or(0))
}

/// Whether `current` is older than the oldest allowed version.
pub fn retired(current: &str, min: &str) -> bool {
    !min.trim().is_empty() && version(current) < version(min)
}

/// The notices for this version: in range and with something to say.
pub fn for_version(r: &Remote, current: &str) -> Vec<Notice> {
    let v = version(current);
    r.notices
        .iter()
        .filter(|n| !n.id.is_empty() && !n.title.is_empty())
        .filter(|n| n.min_version.is_empty() || v >= version(&n.min_version))
        .filter(|n| n.max_version.is_empty() || v <= version(&n.max_version))
        .cloned()
        .collect()
}

/// Whether this version is retired: dictation and meetings refuse to start until it updates.
pub fn blocked() -> bool {
    BLOCKED.load(Ordering::SeqCst)
}

fn cache() -> PathBuf {
    crate::settings::data_dir().join("app.json")
}

fn apply(r: Remote, current: &str) {
    BLOCKED.store(retired(current, &r.min_version), Ordering::SeqCst);
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

fn fetch() -> Result<Remote, String> {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
    let resp = client.get(URL).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Remote>().map_err(|e| e.to_string())
}

/// The cached copy at once, then a fetch now and every 6 hours. Call once at startup.
pub fn start(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    if let Some(r) = std::fs::read(cache()).ok().and_then(|b| serde_json::from_slice::<Remote>(&b).ok()) {
        apply(r, &current);
    }
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            match fetch() {
                Ok(r) => {
                    if let Ok(b) = serde_json::to_vec_pretty(&r) {
                        let _ = std::fs::write(cache(), b);
                    }
                    apply(r, &current);
                    let _ = app.emit("notices", ());
                }
                Err(e) => eprintln!("echotype: notices: {e}"),
            }
            std::thread::sleep(EVERY);
        }
    });
}

/// For the window: this version, whether it is retired, and the notices it should see.
#[tauri::command]
pub async fn app_notices(app: AppHandle) -> Result<serde_json::Value, String> {
    let current = app.package_info().version.to_string();
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    Ok(serde_json::json!({ "current": current, "blocked": blocked(), "min_version": last.min_version, "notices": for_version(&last, &current) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_versions_and_targeted_notices() {
        assert!(retired("0.1.0", "0.2.0") && !retired("0.2.0", "0.2.0") && !retired("0.3.1", "0.2.0"));
        assert!(!retired("0.1.0", ""), "no minimum: nothing retired");
        assert!(version("1.10.0") > version("1.9.9"), "numeric, not text");
        assert_eq!(version("v0.2.0-beta.1"), (0, 2, 0));
        let n = |id: &str, min: &str, max: &str| Notice { id: id.into(), title: "t".into(), min_version: min.into(), max_version: max.into(), ..Default::default() };
        let r = Remote { min_version: String::new(), notices: vec![n("all", "", ""), n("old", "", "0.1.9"), n("new", "0.2.0", ""), Notice { id: "untitled".into(), ..Default::default() }] };
        let ids = |v: &str| for_version(&r, v).into_iter().map(|n| n.id).collect::<Vec<_>>();
        assert_eq!(ids("0.1.0"), ["all", "old"]);
        assert_eq!(ids("0.2.0"), ["all", "new"]);
        let parsed: Remote = serde_json::from_str(r#"{"min_version":"0.1.0","notices":[{"id":"a","title":"Hi","extra":1}]}"#).unwrap();
        assert_eq!(parsed.notices[0].title, "Hi", "unknown fields are ignored");
    }
}
