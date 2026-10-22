//! Notes: the scratchpad in the settings window (notes.js). Every note lives in one notes.json in
//! the data dir, rewritten atomically on each save. A note keeps its last VERSIONS texts, so an
//! accidental wipe or a bad Command Mode rewrite can be undone from "Earlier versions".

use crate::settings::{self, write_atomic};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::AppHandle;

type R<T> = Result<T, String>;

/// Versions kept per note (Wispr Flow keeps 50; 20 covers an afternoon of edits).
const VERSIONS: usize = 20;
/// Autosave runs while typing; a version is kept at most this often, unless most of the text went.
const VERSION_EVERY_MS: u64 = 5 * 60 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Note {
    /// Creation time in ms, unique.
    pub id: u64,
    pub text: String,
    pub updated_ms: u64,
    /// Earlier texts, oldest first.
    pub versions: Vec<Version>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Version {
    /// When this text was replaced.
    pub ms: u64,
    pub text: String,
}

/// One read-modify-write at a time: autosave and Delete can land together.
static LOCK: Mutex<()> = Mutex::new(());

fn path(dir: &Path) -> PathBuf {
    dir.join("notes.json")
}

/// The first line with words, as the list and the export name it.
pub fn title(text: &str) -> String {
    let line = text.lines().map(|l| l.trim().trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("New note");
    line.chars().take(80).collect()
}

/// Every note, newest edit first. An unreadable file is an error, never an empty list, so a save
/// can't overwrite notes it failed to read.
pub fn load(dir: &Path) -> R<Vec<Note>> {
    let mut notes: Vec<Note> = match std::fs::read_to_string(path(dir)) {
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("notes.json is unreadable ({e}), so Echotype left it as it is"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(format!("Could not read your notes: {e}")),
    };
    notes.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    Ok(notes)
}

fn store(dir: &Path, notes: &[Note]) -> R<()> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_atomic(&path(dir), &serde_json::to_string(notes).map_err(|e| e.to_string())?)
}

/// Saves `text` as note `id` (a new note for None) and returns it. The text it replaces becomes a
/// version when `keep` (restoring a version), when the last one is VERSION_EVERY_MS old, or when
/// more than half the text went at once (select all, delete).
pub fn save(dir: &Path, id: Option<u64>, text: &str, keep: bool, now: u64) -> R<Note> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut notes = load(dir)?;
    let i = match id.and_then(|id| notes.iter().position(|n| n.id == id)) {
        Some(i) => i,
        None => {
            // Two notes made in one millisecond still get their own ids.
            let id = notes.iter().map(|n| n.id + 1).max().unwrap_or(0).max(now);
            notes.push(Note { id, ..Note::default() });
            notes.len() - 1
        }
    };
    let n = &mut notes[i];
    if n.text == text && n.updated_ms > 0 {
        return Ok(n.clone());
    }
    let due = n.versions.last().is_none_or(|v| now.saturating_sub(v.ms) >= VERSION_EVERY_MS);
    let cut = text.chars().count() * 2 < n.text.chars().count();
    if !n.text.trim().is_empty() && (keep || due || cut) && n.versions.last().is_none_or(|v| v.text != n.text) {
        n.versions.push(Version { ms: now, text: std::mem::take(&mut n.text) });
        let extra = n.versions.len().saturating_sub(VERSIONS);
        n.versions.drain(..extra);
    }
    n.text = text.to_string();
    n.updated_ms = now;
    let saved = n.clone();
    store(dir, &notes)?;
    Ok(saved)
}

pub fn delete(dir: &Path, id: u64) -> R<()> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut notes = load(dir)?;
    notes.retain(|n| n.id != id);
    store(dir, &notes)
}

/// Notes holding every word of `query` (settings::word_match), newest edit first.
pub fn search<'a>(notes: &'a [Note], query: &str) -> Vec<&'a Note> {
    let has = crate::settings::word_match(query);
    notes.iter().filter(|n| !n.text.trim().is_empty() && has(&n.text)).collect()
}

// ---------- commands ----------

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> R<T> + Send + 'static) -> R<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn notes_list() -> R<Vec<Note>> {
    blocking(|| load(&settings::data_dir())).await
}

#[tauri::command]
pub async fn notes_save(id: Option<u64>, text: String, keep: bool) -> R<Note> {
    blocking(move || save(&settings::data_dir(), id, &text, keep, now_ms())).await
}

#[tauri::command]
pub async fn notes_delete(id: u64) -> R<()> {
    blocking(move || delete(&settings::data_dir(), id)).await
}

/// Writes the note to Downloads as Markdown and shows it in Finder, as meeting_save does.
#[tauri::command]
pub async fn notes_export(app: AppHandle, id: u64) -> R<String> {
    blocking(move || {
        use tauri_plugin_opener::OpenerExt;
        let notes = load(&settings::data_dir())?;
        let n = notes.iter().find(|n| n.id == id).ok_or("That note is gone")?;
        let name: String = title(&n.text).chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
        let name = name.split('-').filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-");
        let dir = dirs::download_dir().ok_or("No Downloads folder")?;
        let path = dir.join(format!("{}.md", if name.is_empty() { "note" } else { &name }));
        std::fs::write(&path, format!("{}\n", n.text.trim_end())).map_err(|e| format!("Could not save: {e}"))?;
        let _ = app.opener().reveal_item_in_dir(&path);
        Ok(path.to_string_lossy().into_owned())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("echotype-notes-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn notes_save_load_and_delete() {
        let d = scratch("crud");
        assert_eq!(load(&d).unwrap(), vec![], "no file yet: no notes");
        let a = save(&d, None, "Groceries\nmilk", false, 1000).unwrap();
        let b = save(&d, None, "Ideas", false, 1000).unwrap();
        assert_ne!(a.id, b.id, "same millisecond, own ids");
        save(&d, Some(a.id), "Groceries\nmilk, eggs", false, 2000).unwrap();
        let all = load(&d).unwrap();
        assert_eq!(all.iter().map(|n| n.text.as_str()).collect::<Vec<_>>(), ["Groceries\nmilk, eggs", "Ideas"], "newest edit first");
        assert_eq!(save(&d, Some(a.id), "Groceries\nmilk, eggs", false, 9000).unwrap().updated_ms, 2000, "no change, no write");
        delete(&d, b.id).unwrap();
        assert_eq!(load(&d).unwrap().len(), 1);
        assert!(std::fs::read_dir(&d).unwrap().all(|e| e.unwrap().file_name() == "notes.json"), "no temp files left behind");
    }

    #[test]
    fn versions_are_kept_now_and_then_on_a_wipe_and_on_restore_and_capped() {
        let d = scratch("versions");
        let n = save(&d, None, "one", false, 0).unwrap();
        // Typing: the first edit keeps the text before it, the next ones within 5 minutes do not.
        save(&d, Some(n.id), "one two", false, 1000).unwrap();
        save(&d, Some(n.id), "one two three", false, 2000).unwrap();
        let v = save(&d, Some(n.id), "one two three four", false, 3000).unwrap().versions;
        assert_eq!(v.iter().map(|v| v.text.as_str()).collect::<Vec<_>>(), ["one"]);
        let v = save(&d, Some(n.id), "one two three four five", false, 3000 + VERSION_EVERY_MS).unwrap().versions;
        assert_eq!(v.last().unwrap().text, "one two three four", "five minutes on: another version");
        let v = save(&d, Some(n.id), "", false, 3001 + VERSION_EVERY_MS).unwrap().versions;
        assert_eq!(v.last().unwrap().text, "one two three four five", "a wipe always keeps what was there");
        let back = save(&d, Some(n.id), &v.last().unwrap().text, true, 3002 + VERSION_EVERY_MS).unwrap();
        assert_eq!(back.text, "one two three four five");
        assert_eq!(back.versions.len(), 3, "restoring from empty keeps nothing empty");
        for i in 0..30 {
            save(&d, Some(n.id), &format!("draft {i}"), true, 10_000_000 + i).unwrap();
        }
        let v = load(&d).unwrap()[0].versions.clone();
        assert_eq!(v.len(), VERSIONS);
        assert_eq!((v[0].text.as_str(), v[VERSIONS - 1].text.as_str()), ("draft 9", "draft 28"), "the oldest go first");
    }

    #[test]
    fn an_unreadable_file_is_never_overwritten() {
        let d = scratch("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(path(&d), "{torn").unwrap();
        assert!(load(&d).is_err());
        assert!(save(&d, None, "new", false, 1).is_err() && delete(&d, 1).is_err());
        assert_eq!(std::fs::read_to_string(path(&d)).unwrap(), "{torn", "left for the user to recover");
    }

    #[test]
    fn search_needs_every_word_and_titles_come_from_the_first_line() {
        let note = |id, text: &str| Note { id, text: text.into(), ..Note::default() };
        let notes = vec![note(1, "Call Arnav about the invoice"), note(2, "Invoice template"), note(3, "  ")];
        assert_eq!(search(&notes, "INVOICE").iter().map(|n| n.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(search(&notes, "invoice arnav").len(), 1);
        assert_eq!(search(&notes, "").len(), 2, "empty notes never match");
        assert_eq!(title("\n\n## Launch plan  \nsteps"), "Launch plan");
        assert_eq!(title("   "), "New note");
        assert_eq!(title(&"x".repeat(200)).len(), 80);
    }
}
