//! Files: recordings dropped on Home or picked there, transcribed one after another, and a
//! watched folder (Settings → Files) whose new recordings get a transcript written next to them.
//! The recordings themselves are only ever read.

use crate::meeting;
use crate::settings::{self, Settings};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

type R<T> = Result<T, String>;

/// What the watched folder picks up: audio, and video whose sound track decodes.
pub const EXTS: [&str; 9] = ["wav", "mp3", "m4a", "mp4", "mov", "aac", "flac", "ogg", "opus"];
/// The watched folder is listed this often...
const POLL: Duration = Duration::from_secs(5);
/// ...and a recording is taken once its size has held this long (before that it may still be
/// copying in or recording).
const SETTLE: Duration = Duration::from_secs(4);
/// Settings → Files "Save in a Transcripts folder".
const SUBFOLDER: &str = "Transcripts";

pub struct Job {
    pub id: u64,
    pub path: String,
    /// From the watched folder: its transcript is written to disk, and Cancel leaves it queued.
    pub watched: bool,
}

/// Files waiting their turn. One is transcribed at a time, so a batch loads the model once and
/// never competes with dictation for memory.
pub struct Queue {
    jobs: VecDeque<Job>,
    running: Option<String>,
    next: u64,
}

impl Queue {
    pub const fn new() -> Self {
        Self { jobs: VecDeque::new(), running: None, next: 1 }
    }

    /// Queues `path` unless it is already waiting or running; its id.
    pub fn push(&mut self, path: &str, watched: bool) -> Option<u64> {
        if self.running.as_deref() == Some(path) || self.jobs.iter().any(|j| j.path == path) {
            return None;
        }
        let id = self.next;
        self.next += 1;
        self.jobs.push_back(Job { id, path: path.into(), watched });
        Some(id)
    }

    /// The next job, running until `done`.
    pub fn pop(&mut self) -> Option<Job> {
        let job = self.jobs.pop_front()?;
        self.running = Some(job.path.clone());
        Some(job)
    }

    pub fn done(&mut self) {
        self.running = None;
    }

    /// Cancel on Home: the dropped files still waiting leave the queue (the one running finishes,
    /// the watched folder's keep their turn). Their ids.
    pub fn cancel(&mut self) -> Vec<u64> {
        let (gone, keep): (VecDeque<Job>, VecDeque<Job>) = self.jobs.drain(..).partition(|j| !j.watched);
        self.jobs = keep;
        gone.into_iter().map(|j| j.id).collect()
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static WAKE: Condvar = Condvar::new();
/// The remembered-files store is read and written by the poll and the worker.
static STORE: Mutex<()> = Mutex::new(());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A recording's size over the polls: steady once unchanged (and not empty) for SETTLE.
#[derive(Default)]
pub struct Settle(HashMap<String, (u64, Instant)>);

impl Settle {
    pub fn steady(&mut self, path: &str, size: u64, now: Instant) -> bool {
        let seen = self.0.entry(path.to_string()).or_insert((size, now));
        if seen.0 != size {
            *seen = (size, now);
        }
        size > 0 && now.duration_since(seen.1) >= SETTLE
    }
}

/// The watched folders' memory, so a restart does not transcribe anything twice.
#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Seen {
    /// Folders already watched: what was in one when it was picked is left alone.
    pub folders: BTreeSet<String>,
    /// Recordings transcribed, or tried and failed (a file that cannot be read is not retried every poll).
    // ponytail: grows by one path per recording; prune paths that no longer exist if it ever matters.
    pub done: BTreeSet<String>,
}

impl Seen {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> R<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, serde_json::to_vec(self).map_err(|e| e.to_string())?).map_err(|e| format!("Could not remember watched files: {e}"))
    }
}

fn store() -> PathBuf {
    settings::data_dir().join("watched-files.json")
}

/// The recordings directly inside `dir` (not its subfolders, so Transcripts is never read), with their sizes.
fn recordings(dir: &Path) -> Vec<(String, u64)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension()?.to_str()?.to_ascii_lowercase();
            let meta = e.metadata().ok()?; // not followed: a symlink is not a file here
            let ok = EXTS.contains(&ext.as_str()) && meta.is_file() && !e.file_name().to_string_lossy().starts_with('.');
            ok.then(|| (p.to_string_lossy().into_owned(), meta.len()))
        })
        .collect()
}

/// The recordings in `dir` ready to transcribe: new since the folder was picked, not done, and
/// finished copying. A folder seen for the first time only remembers what is already there.
pub fn scan(dir: &Path, seen: &mut Seen, settle: &mut Settle, now: Instant) -> Vec<String> {
    let files = recordings(dir);
    settle.0.retain(|p, _| files.iter().any(|(f, _)| f == p));
    if seen.folders.insert(dir.to_string_lossy().into_owned()) {
        seen.done.extend(files.into_iter().map(|(p, _)| p));
        return vec![];
    }
    files.into_iter().filter(|(p, size)| !seen.done.contains(p) && settle.steady(p, *size, now)).map(|(p, _)| p).collect()
}

/// `body` as `dir/stem.ext`, or `stem 2.ext` and so on: an existing file is never replaced.
fn write_free(dir: &Path, stem: &str, ext: &str, body: &[u8]) -> R<PathBuf> {
    use std::io::Write;
    std::fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    for n in 1..1000 {
        let path = dir.join(if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} {n}.{ext}") });
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => return f.write_all(body).map(|_| path).map_err(|e| format!("Could not write the transcript: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Could not write the transcript: {e}")),
        }
    }
    Err(format!("{stem}.{ext} and 999 more already exist"))
}

fn stem(path: &str) -> String {
    Path::new(path).file_stem().map_or_else(|| "Transcript".into(), |s| s.to_string_lossy().into_owned())
}

/// A file's transcript in an export format (meeting.rs: md, txt, json, srt, vtt, html, docx).
fn transcript(path: &str, text: &str, secs: f32, format: &str) -> R<(Vec<u8>, &'static str)> {
    meeting::export(&meeting::from_text(&stem(path), text, secs), format)
}

/// The watched folder's transcript of `path`, beside it or in its Transcripts folder.
pub fn write_transcript(s: &Settings, path: &str, text: &str, secs: f32) -> R<PathBuf> {
    let (body, ext) = transcript(path, text, secs, &s.watch_format)?;
    let dir = Path::new(path).parent().ok_or("No folder")?;
    let out = if s.watch_subfolder { dir.join(SUBFOLDER) } else { dir.to_path_buf() };
    // A shared watched folder could hold a Transcripts symlink pointing anywhere: don't follow it.
    if s.watch_subfolder && std::fs::symlink_metadata(&out).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a link to another folder, so transcripts aren't saved there", out.display()));
    }
    write_free(&out, &stem(path), ext, &body)
}

fn emit(app: &AppHandle, v: serde_json::Value) {
    let _ = app.emit_to("main", "file-job", v);
}

/// Starts the worker that transcribes the queue and the watched folder's poll.
pub fn start(app: &AppHandle) {
    let h = app.clone();
    std::thread::spawn(move || {
        loop {
            let job = {
                let mut q = lock(&QUEUE);
                loop {
                    if let Some(j) = q.pop() {
                        break j;
                    }
                    q = WAKE.wait(q).unwrap_or_else(|e| e.into_inner());
                }
            };
            run(&h, &job);
            lock(&QUEUE).done();
        }
    });
    let h = app.clone();
    std::thread::spawn(move || {
        let mut settle = Settle::default();
        loop {
            std::thread::sleep(POLL);
            poll(&h, &mut settle);
        }
    });
}

fn run(app: &AppHandle, job: &Job) {
    emit(app, json!({ "id": job.id, "path": job.path, "watched": job.watched, "state": "working" }));
    let s = crate::current_settings(app);
    let res = crate::transcribe_path(&s, &job.path).and_then(|(out, secs)| {
        crate::record_stats(app, &s, crate::stats::Source::File, &out.text, 0);
        crate::record_history(&s, &out, None);
        let saved = job.watched.then(|| write_transcript(&s, &job.path, &out.text, secs)).transpose()?;
        Ok((out, secs, saved))
    });
    if job.watched {
        let _g = lock(&STORE);
        let mut seen = Seen::load(&store());
        seen.done.insert(job.path.clone());
        if let Err(e) = seen.save(&store()) {
            eprintln!("echotype: {e}");
        }
    }
    let mut v = json!({ "id": job.id, "path": job.path, "watched": job.watched });
    match res {
        Ok((out, secs, saved)) => {
            if let (Some(v), Ok(serde_json::Value::Object(out))) = (v.as_object_mut(), serde_json::to_value(&out)) {
                v.extend(out);
            }
            v["state"] = "done".into();
            v["secs"] = secs.into();
            v["saved"] = saved.map(|p| p.to_string_lossy().into_owned()).into();
        }
        Err(e) => {
            v["state"] = "error".into();
            v["error"] = e.into();
        }
    }
    emit(app, v);
}

fn poll(app: &AppHandle, settle: &mut Settle) {
    let s = crate::current_settings(app);
    // No speech model yet: every recording would fail and be remembered as done.
    if s.watch_folder.is_empty() || crate::stt::ready(&s).is_err() {
        return;
    }
    let ready = {
        let _g = lock(&STORE);
        let mut seen = Seen::load(&store());
        let before = seen.clone();
        let ready = scan(Path::new(&s.watch_folder), &mut seen, settle, Instant::now());
        if seen != before
            && let Err(e) = seen.save(&store())
        {
            eprintln!("echotype: {e}");
        }
        ready
    };
    let mut q = lock(&QUEUE);
    for path in ready {
        if let Some(id) = q.push(&path, true) {
            emit(app, json!({ "id": id, "path": path, "watched": true, "state": "queued" }));
        }
    }
    WAKE.notify_one();
}

/// Queues dropped or picked files; each reports on "file-job" as it goes. The ones queued.
#[tauri::command]
pub async fn transcribe_files(paths: Vec<String>) -> R<Vec<serde_json::Value>> {
    let mut q = lock(&QUEUE);
    let queued = paths.iter().filter_map(|p| q.push(p, false).map(|id| json!({ "id": id, "path": p }))).collect();
    WAKE.notify_one();
    Ok(queued)
}

/// Cancel on Home: the files not started yet. Their ids.
#[tauri::command]
pub async fn cancel_files() -> R<Vec<u64>> {
    Ok(lock(&QUEUE).cancel())
}

/// A transcribed file in `format`, saved to Downloads and shown in Finder.
#[tauri::command]
pub async fn file_save(app: AppHandle, path: String, text: String, secs: f32, format: String) -> R<String> {
    crate::blocking(move || {
        use tauri_plugin_opener::OpenerExt;
        let (body, ext) = transcript(&path, &text, secs, &format)?;
        let saved = write_free(&dirs::download_dir().ok_or("No Downloads folder")?, &stem(&path), ext, &body)?;
        let _ = app.opener().reveal_item_in_dir(&saved);
        Ok(saved.to_string_lossy().into_owned())
    })
    .await
}

/// The system's picker for recordings (several at once) or for a folder; nothing on Cancel.
#[tauri::command]
pub async fn pick_files(folder: bool) -> R<Vec<String>> {
    crate::blocking(move || pick(folder)).await
}

// ponytail: AppleScript's choose file / choose folder (no dialog plugin in the app); a native
// NSOpenPanel sheet if the picker should hang off the window.
#[cfg(target_os = "macos")]
fn pick(folder: bool) -> R<Vec<String>> {
    let script = if folder {
        "activate\nPOSIX path of (choose folder with prompt \"Choose a folder to watch\")"
    } else {
        "activate\nset out to \"\"\nrepeat with f in (choose file with prompt \"Choose recordings to transcribe\" of type {\"public.audio\", \"public.movie\"} with multiple selections allowed)\nset out to out & POSIX path of f & linefeed\nend repeat\nout"
    };
    let out = std::process::Command::new("/usr/bin/osascript").args(["-e", script]).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return if err.contains("-128") { Ok(vec![]) } else { Err(err.trim().to_string()) }; // -128: Cancel
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim_end_matches('/').to_string()).filter(|l| !l.is_empty()).collect())
}

#[cfg(not(target_os = "macos"))]
fn pick(_folder: bool) -> R<Vec<String>> {
    Err("Drop the files on this window instead.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_queue_runs_files_once_in_order_and_cancel_spares_the_watched_folder() {
        let mut q = Queue::new();
        assert_eq!(q.push("a.m4a", false), Some(1));
        assert_eq!(q.push("b.wav", true), Some(2));
        assert_eq!(q.push("c.mp3", false), Some(3));
        assert_eq!(q.push("a.m4a", false), None, "already waiting");
        let a = q.pop().unwrap();
        assert_eq!((a.id, a.path.as_str()), (1, "a.m4a"));
        assert_eq!(q.push("a.m4a", false), None, "running");
        assert_eq!(q.cancel(), [3], "the dropped files still waiting");
        assert_eq!(q.pop().map(|j| j.path), Some("b.wav".into()), "the watched folder's keeps its turn");
        q.done();
        assert!(q.pop().is_none());
        assert_eq!(q.push("a.m4a", false), Some(4), "done, so it can be queued again");
    }

    #[test]
    fn a_recording_is_taken_once_its_size_holds() {
        let (mut s, t) = (Settle::default(), Instant::now());
        assert!(!s.steady("x.wav", 100, t), "first sight");
        assert!(!s.steady("x.wav", 200, t + Duration::from_secs(5)), "still growing");
        assert!(!s.steady("x.wav", 200, t + Duration::from_secs(8)), "held 3 s");
        assert!(s.steady("x.wav", 200, t + Duration::from_secs(9)), "held 4 s");
        assert!(!s.steady("empty.wav", 0, t) && !s.steady("empty.wav", 0, t + Duration::from_secs(60)), "an empty file waits");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("echotype-files-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_new_finished_recordings_are_taken_and_remembered_across_restarts() {
        let dir = temp("scan");
        std::fs::write(dir.join("old.m4a"), b"old").unwrap();
        let store = dir.join("store").join("watched-files.json");
        let (mut seen, mut settle, t) = (Seen::load(&store), Settle::default(), Instant::now());
        assert!(scan(&dir, &mut seen, &mut settle, t).is_empty(), "what was there when the folder was picked stays alone");
        for (f, body) in [("new.WAV", &b"riff"[..]), ("notes.txt", b"x"), (".hidden.mp3", b"x"), ("partial.mp3.download", b"x")] {
            std::fs::write(dir.join(f), body).unwrap();
        }
        std::fs::create_dir_all(dir.join("Transcripts")).unwrap();
        std::fs::write(dir.join("Transcripts").join("inner.wav"), b"x").unwrap();
        assert!(scan(&dir, &mut seen, &mut settle, t).is_empty(), "just appeared");
        let new = dir.join("new.WAV").to_string_lossy().into_owned();
        assert_eq!(scan(&dir, &mut seen, &mut settle, t + POLL), [new.clone()]);
        seen.done.insert(new);
        seen.save(&store).unwrap();
        let mut again = Seen::load(&store);
        assert_eq!(again, seen, "the store round-trips");
        assert!(scan(&dir, &mut again, &mut Settle::default(), t + POLL * 3).is_empty(), "a restart redoes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcripts_never_replace_a_file() {
        let dir = temp("write");
        let rec = dir.join("call.m4a");
        std::fs::write(&rec, b"audio").unwrap();
        std::fs::write(dir.join("call.txt"), b"mine").unwrap();
        let mut s = Settings::default();
        let path = rec.to_string_lossy();
        assert_eq!(write_transcript(&s, &path, "Hello there.", 2.0).unwrap(), dir.join("call 2.txt"));
        assert_eq!(std::fs::read(dir.join("call.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read_to_string(dir.join("call 2.txt")).unwrap(), "call\n\nHello there.\n");
        (s.watch_subfolder, s.watch_format) = (true, "srt".into());
        assert_eq!(write_transcript(&s, &path, "Hello there.", 2.0).unwrap(), dir.join("Transcripts").join("call.srt"));
        assert_eq!(std::fs::read(&rec).unwrap(), b"audio", "the recording is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_linked_transcripts_folder_is_not_followed() {
        let (dir, elsewhere) = (temp("link"), temp("link-target"));
        let rec = dir.join("call.m4a");
        std::fs::write(&rec, b"audio").unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("Transcripts")).unwrap();
        let s = Settings { watch_subfolder: true, ..Default::default() };
        assert!(write_transcript(&s, &rec.to_string_lossy(), "Hello.", 1.0).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        let _ = (std::fs::remove_dir_all(&dir), std::fs::remove_dir_all(&elsewhere));
    }

    /// Real model: `say` voices a clip into a temp folder, which is then watched end to end.
    /// cargo test --lib watched_folder_with_the_real_model -- --ignored --nocapture
    #[test]
    #[ignore]
    fn watched_folder_with_the_real_model() {
        let model = dirs::home_dir().unwrap().join(".cache/whisper/ggml-large-v3-turbo-q5_0.bin");
        let dir = temp("real");
        // English set, so the language detector (a second resident model) never loads.
        let mut s = Settings { advanced: true, watch_subfolder: true, watch_format: "docx".into(), languages: vec!["en".into()], ..Settings::default() };
        (s.stt.provider, s.stt.local_model, s.cleanup.enabled) = ("local".into(), model.to_string_lossy().into_owned(), false);
        let (mut seen, mut settle, t) = (Seen::default(), Settle::default(), Instant::now());
        assert!(scan(&dir, &mut seen, &mut settle, t).is_empty());
        let aiff = std::env::temp_dir().join(format!("echotype-files-say-{}.aiff", std::process::id()));
        let say = "Please send the quarterly report to the finance team by Friday.";
        assert!(std::process::Command::new("say").args(["-v", "Samantha", "-o"]).arg(&aiff).arg(say).status().unwrap().success());
        let clip = dir.join("memo.m4a");
        assert!(std::process::Command::new("ffmpeg").args(["-loglevel", "error", "-y", "-i"]).arg(&aiff).args(["-ar", "16000", "-ac", "1"]).arg(&clip).status().unwrap().success());
        let before = std::fs::read(&clip).unwrap();
        scan(&dir, &mut seen, &mut settle, t);
        let ready = scan(&dir, &mut seen, &mut settle, t + POLL);
        assert_eq!(ready, [clip.to_string_lossy().into_owned()]);
        let (out, secs) = crate::transcribe_path(&s, &ready[0]).unwrap();
        crate::stt::unload(); // ggml-metal asserts at exit while a model is resident
        println!("{secs:.1} s: {}", out.text);
        assert!(out.text.to_lowercase().contains("quarterly report"), "{}", out.text);
        let saved = write_transcript(&s, &ready[0], &out.text, secs).unwrap();
        assert_eq!(saved, dir.join("Transcripts").join("memo.docx"));
        let mut z = zip::ZipArchive::new(std::fs::File::open(&saved).unwrap()).unwrap();
        let mut doc = String::new();
        std::io::Read::read_to_string(&mut z.by_name("word/document.xml").unwrap(), &mut doc).unwrap();
        assert!(doc.contains("quarterly report"), "{doc}");
        assert_eq!(std::fs::read(&clip).unwrap(), before, "the recording is untouched");
        let _ = std::fs::remove_file(&aiff);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
