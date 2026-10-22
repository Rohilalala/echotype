//! Meeting notes. The microphone ("You") and the computer's audio (everyone else) are recorded
//! side by side, written to disk as they arrive, transcribed live in ~25 s chunks cut at pauses,
//! labelled by speaker, and merged into one timeline. When the meeting ends the computer audio is
//! re-clustered as a whole so speaker labels agree from start to finish, and the configured cleanup
//! model may write a summary. Everything stays in the app data folder under meetings/.

use crate::audio;
use crate::capture::{self, Capture, Piece, Source};
use crate::settings::{self, Settings};
use crate::speakers::{self, Clusters, Embedder, Turn};
use crate::stt::{self, Word, looped, norm};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const RATE: usize = audio::RATE as usize;
/// Live lines: a chunk is transcribed at the first pause once this much audio waits...
const MIN_CHUNK_S: usize = 4;
/// ...or, with no pause, once this much waits (whisper reads 30 s windows).
const CHUNK_S: usize = 25;
/// That one ends at the quietest 300 ms of its last LOOK_S seconds when that is a pause...
const LOOK_S: usize = 8;
/// ...else hard at CHUNK_S, and the next chunk starts OVERLAP_S earlier; each side keeps the
/// words whose middle falls on its side of the overlap's midpoint.
const OVERLAP_S: f32 = 1.0;
/// Chunks never louder than this are not sent to whisper (it invents text from silence).
const VOICE_DB: f32 = -50.0;
/// Words further apart than this start a new transcript line.
const LINE_GAP_S: f32 = 1.5;
/// A run of computer-audio words this long gets a voice fingerprint for its live speaker label.
/// (Shorter runs fingerprint poorly and would start new speakers; they keep the previous label.)
const MIN_EMBED_S: f32 = 1.5;

pub const ME: &str = "you";
pub const OTHERS: &str = "others";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f32,
    pub end: f32,
    pub speaker: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Meeting {
    pub id: String,
    pub title: String,
    pub started_ms: u64,
    pub duration_s: f32,
    /// "recording" | "processing" | "done" | "interrupted" (the app quit mid-meeting).
    pub state: String,
    pub segments: Vec<Segment>,
    /// Speaker id ("you", "others", "s1"...) → the name shown.
    pub speakers: BTreeMap<String, String>,
    /// Markdown with Summary, Decisions and Action items.
    pub summary: Option<String>,
    /// Shown above the transcript: mic-only, summary skipped, speaker models missing...
    pub notices: Vec<String>,
    /// Names heard for speakers still called "Speaker N" (name_hints), offered to the user.
    #[serde(skip_deserializing, skip_serializing_if = "BTreeMap::is_empty")]
    pub name_hints: BTreeMap<String, String>,
}

pub fn default_name(id: &str) -> String {
    match id {
        ME => "You".into(),
        OTHERS => "Others".into(),
        _ => id.strip_prefix('s').map_or_else(|| id.to_string(), |n| format!("Speaker {n}")),
    }
}

// ---------- chunking ----------

/// Where the next chunk of `pcm` (16 kHz) ends and whether it ends in a pause: at the last pause
/// after MIN_CHUNK_S, so lines show up as people pause; else once CHUNK_S waits. None until then;
/// with `flush`, whatever is left is the last chunk.
// ponytail: each chunk is a whisper call with a 30 s encoder pass, so short chunks cost more GPU
// (two tracks, a call every few seconds: ~10% of an M5); a final full-audio pass would read better.
pub fn next_cut(pcm: &[f32], flush: bool) -> Option<(usize, bool)> {
    let rest = (flush && !pcm.is_empty()).then_some((pcm.len(), true));
    if pcm.len() < MIN_CHUNK_S * RATE {
        return rest;
    }
    let full = pcm.len() >= CHUNK_S * RATE;
    let win = RATE / 20;
    let db: Vec<f32> = pcm[..pcm.len().min(CHUNK_S * RATE)].chunks(win).map(audio::dbfs).collect();
    let mut sorted = db.clone();
    sorted.sort_by(f32::total_cmp);
    // A pause: within 8 dB of the chunk's quiet floor and 15 dB under its speech.
    let pause = (sorted[sorted.len() / 10] + 8.0).max(-55.0).min(sorted[sorted.len() * 9 / 10] - 15.0);
    let span = 6; // 300 ms
    if !full {
        let first = MIN_CHUNK_S * 20 - span / 2;
        let last = (first..=db.len().saturating_sub(span)).rev().find(|&i| db[i..i + span].iter().all(|&d| d <= pause));
        return last.map(|i| ((i + span / 2) * win, true)).or(rest);
    }
    let quietest = ((CHUNK_S - LOOK_S) * 20..=db.len() - span)
        .map(|i| (i, db[i..i + span].iter().copied().fold(f32::MIN, f32::max)))
        .min_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0))); // latest of equals: longer chunks
    match quietest {
        Some((i, level)) if level <= pause => Some(((i + span / 2) * win, true)),
        _ => Some((CHUNK_S * RATE, false)),
    }
}

fn has_voice(pcm: &[f32]) -> bool {
    pcm.chunks(RATE / 20).any(|w| audio::dbfs(w) > VOICE_DB)
}

/// Words a chunk starting at `t0` s heard, moved onto the meeting clock, keeping those whose
/// middle is in [from, to): the overlap with the neighbouring chunk is heard twice, kept once.
pub fn owned(words: Vec<Word>, t0: f32, from: f32, to: f32) -> Vec<Word> {
    words
        .into_iter()
        .map(|w| Word { start: w.start + t0, end: w.end + t0, ..w })
        .filter(|w| (from..to).contains(&((w.start + w.end) / 2.0)))
        .collect()
}

/// Remote STT returns text without times: spread its words evenly over the chunk.
// ponytail: even spread; ask for verbose_json word timestamps where the provider supports them.
fn spread(text: &str, secs: f32) -> Vec<Word> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let step = secs / parts.len().max(1) as f32;
    parts.iter().enumerate().map(|(i, p)| Word { start: i as f32 * step, end: (i + 1) as f32 * step, text: p.to_string() }).collect()
}

// ---------- timeline ----------

/// A speaker's sentences: words up to . ? or ! (a sentence never splits, so word times that
/// soak up a pause cannot break one apart, and a speaker change starts a new one).
fn sentences(words: &[(Word, String)]) -> Vec<Segment> {
    let mut out: Vec<Segment> = vec![];
    let mut open = false;
    for (w, who) in words {
        match out.last_mut() {
            Some(s) if open && s.speaker == *who => {
                s.text.push(' ');
                s.text.push_str(&w.text);
                s.end = s.end.max(w.end);
            }
            _ => out.push(Segment { start: w.start, end: w.end, speaker: who.clone(), text: w.text.clone() }),
        }
        open = !w.text.ends_with(['.', '?', '!']);
    }
    out
}

/// One timeline from the mic and computer-audio words: sentences from both, ordered by time, and
/// a speaker's consecutive sentences joined into one line unless they pause longer than
/// LINE_GAP_S. Ordering sentences, not lines, keeps one side's long stretch from swallowing what
/// the other side said in between. Without headphones the mic also hears the call; a mic sentence
/// whose words mostly appear in the computer audio at the same moment is that echo and is dropped.
pub fn timeline(mic: &[(Word, String)], system: &[(Word, String)]) -> Vec<Segment> {
    let mut all: Vec<Segment> = sentences(mic)
        .into_iter()
        .filter(|m| {
            let heard: HashSet<String> = system.iter().filter(|(w, _)| w.end >= m.start - 1.0 && w.start <= m.end + 1.0).map(|(w, _)| norm(&w.text)).collect();
            let mine: Vec<String> = m.text.split_whitespace().map(norm).filter(|w| !w.is_empty()).collect();
            let echoed = mine.iter().filter(|w| heard.contains(*w)).count();
            mine.len() < 3 || echoed * 10 < mine.len() * 6
        })
        .collect();
    all.extend(sentences(system));
    all.retain(|s| !looped(&s.text));
    all.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out: Vec<Segment> = vec![];
    for s in all {
        match out.last_mut() {
            Some(l) if l.speaker == s.speaker && s.start - l.end <= LINE_GAP_S => {
                l.text.push(' ');
                l.text.push_str(&s.text);
                l.end = l.end.max(s.end);
            }
            _ => out.push(s),
        }
    }
    out
}

// ---------- speakers ----------

/// The diarization turn a word belongs to: most overlap, else the nearest. None without turns.
pub fn turn_of(turns: &[Turn], w: &Word) -> Option<i32> {
    let overlap = |t: &Turn| (t.end.min(w.end) - t.start.max(w.start)).max(0.0);
    let gap = |t: &Turn| (t.start - w.end).max(w.start - t.end).max(0.0);
    turns.iter().max_by(|a, b| overlap(a).total_cmp(&overlap(b)).then(gap(b).total_cmp(&gap(a)))).map(|t| t.speaker)
}

/// Each sentence goes to the speaker whose turns its words overlap most (the nearest turn when
/// none): whisper starts the first word after a pause up to a second early, so word by word a
/// reply's first words land on the previous speaker, and people rarely hand over mid-sentence.
pub fn sentence_speakers(words: &[Word], turns: &[Turn]) -> Option<Vec<i32>> {
    if turns.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let mut j = i;
        while j + 1 < words.len() && !words[j].text.ends_with(['.', '?', '!']) && words[j + 1].start - words[j].end <= LINE_GAP_S {
            j += 1;
        }
        let mut overlap: BTreeMap<i32, f32> = BTreeMap::new();
        for w in &words[i..=j] {
            // A word's start soaks up the silence before it; its end is reliable. Count its last 0.3 s.
            let start = w.start.max(w.end - 0.3);
            for t in turns {
                *overlap.entry(t.speaker).or_default() += (t.end.min(w.end) - t.start.max(start)).max(0.0);
            }
        }
        let best = overlap.into_iter().filter(|(_, o)| *o > 0.0).max_by(|a, b| a.1.total_cmp(&b.1)).map(|(s, _)| s);
        // In a gap between turns: the early timing makes the turn just after it the likelier owner.
        let next = turns.iter().filter(|t| t.start >= words[j].end && t.start - words[j].end <= 1.0).min_by(|a, b| a.start.total_cmp(&b.start)).map(|t| t.speaker);
        let who = best.or(next).or_else(|| turn_of(turns, &Word { start: words[i].start, end: words[j].end, text: String::new() }))?;
        out.extend(std::iter::repeat_n(who, j + 1 - i));
        i = j + 1;
    }
    Some(out)
}

/// After the end-of-meeting re-cluster. `clusters[i]` is word i's final cluster and `live[i]` the
/// label it had live. Clusters become s1, s2... in order of first appearance; a cluster keeps the
/// name the user gave the live label most of its words carried, so a rename made during the
/// meeting survives (each name goes to one cluster only).
pub fn relabel(clusters: &[i32], live: &[String], names: &BTreeMap<String, String>) -> (Vec<String>, BTreeMap<String, String>) {
    let mut order: Vec<i32> = vec![];
    for c in clusters {
        if !order.contains(c) {
            order.push(*c);
        }
    }
    let id = |c: i32| format!("s{}", order.iter().position(|&o| o == c).unwrap_or(0) + 1);
    let mut new_names: BTreeMap<String, String> = names.iter().filter(|(k, _)| *k == ME).map(|(k, v)| (k.clone(), v.clone())).collect();
    // (words, cluster, live label) best first, so the bigger claim on a renamed label wins.
    let mut claims: Vec<(usize, i32, &String)> = vec![];
    for &c in &order {
        let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
        for (k, l) in clusters.iter().zip(live) {
            if *k == c {
                *counts.entry(l).or_default() += 1;
            }
        }
        if let Some((l, n)) = counts.into_iter().max_by_key(|(_, n)| *n) {
            claims.push((n, c, l));
        }
    }
    claims.sort_by(|a, b| b.0.cmp(&a.0));
    let mut used = HashSet::new();
    for (_, c, l) in claims {
        let custom = names.get(l).filter(|n| **n != default_name(l));
        if let Some(n) = custom.filter(|_| used.insert(l.clone())) {
            new_names.insert(id(c), n.clone());
        }
    }
    for &c in &order {
        new_names.entry(id(c)).or_insert_with(|| default_name(&id(c)));
    }
    (clusters.iter().map(|&c| id(c)).collect(), new_names)
}

// ---------- the session: audio in, transcript out ----------

pub type Transcribe = Box<dyn FnMut(&[f32]) -> Result<Vec<Word>, String> + Send>;

/// A WAV that is valid after every append, so a crash loses nothing already written.
pub(crate) struct Wav(std::fs::File, u32);

impl Wav {
    pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        f.write_all(&audio::wav_bytes(&[]))?;
        Ok(Self(f, 0))
    }

    pub(crate) fn append(&mut self, pcm: &[f32]) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        self.0.write_all(&audio::wav_bytes(pcm)[44..])?;
        self.1 += pcm.len() as u32 * 2;
        self.0.seek(SeekFrom::Start(4))?;
        self.0.write_all(&(36 + self.1).to_le_bytes())?;
        self.0.seek(SeekFrom::Start(40))?;
        self.0.write_all(&self.1.to_le_bytes())?;
        self.0.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

/// Reads back a WAV this module wrote (16-bit mono 16 kHz, no length limit).
fn read_wav(path: &Path) -> Vec<f32> {
    std::fs::read(path).map(|b| b.get(44..).unwrap_or_default().chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect()).unwrap_or_default()
}

#[derive(Default)]
struct Track {
    /// Audio not transcribed yet; `pending[0]` is meeting sample `at`.
    pending: Vec<f32>,
    at: usize,
    /// Samples received, silence padding included.
    total: usize,
    /// Words before this time (s) belong to the previous chunk.
    owned_from: f32,
    /// Meeting-clock words and their speaker id.
    words: Vec<(Word, String)>,
    wav: Option<Wav>,
}

pub struct Session {
    pub meeting: Arc<Mutex<Meeting>>,
    dir: Option<PathBuf>,
    mic: Track,
    system: Track,
    transcribe: Transcribe,
    /// Voice fingerprints for live labels; None labels every other voice "Others".
    embed: Option<Embedder>,
    clusters: Clusters,
    pub stt_ms: u128,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Session {
    /// `dir`: where the WAVs and meeting.json go (None for the CLI).
    pub fn new(meeting: Meeting, dir: Option<PathBuf>, transcribe: Transcribe, embed: Option<Embedder>) -> Result<Self, String> {
        let mut s = Self { meeting: Arc::new(Mutex::new(meeting)), dir, mic: Track::default(), system: Track::default(), transcribe, embed, clusters: Clusters::default(), stt_ms: 0 };
        if let Some(d) = &s.dir {
            let wav = |n: &str| Wav::create(&d.join(n)).map(Some).map_err(|e| format!("Could not create the meeting's audio file: {e}"));
            s.mic.wav = wav("audio-mic.wav")?;
            s.system.wav = wav("audio-system.wav")?;
        }
        Ok(s)
    }

    /// Takes captured audio. `at`: when its last sample arrived, on the meeting clock; a stream
    /// that started late or went quiet (ScreenCaptureKit sends nothing while nothing plays) is
    /// padded with silence so both stay on one clock. Returns whether the transcript changed.
    pub fn push(&mut self, src: Source, pcm: &[f32], at: Option<Duration>) -> bool {
        let t = self.track(src);
        let behind = at.map_or(0.0, |a| a.as_secs_f32() - (t.total + pcm.len()) as f32 / RATE as f32);
        if behind > 0.5 {
            let pad = vec![0.0; (behind * RATE as f32) as usize];
            Self::append(t, &pad);
        }
        Self::append(t, pcm);
        let mut changed = false;
        while let Some((cut, pause)) = next_cut(&self.track(src).pending, false) {
            self.chunk(src, cut, pause);
            changed = true;
        }
        changed
    }

    fn append(t: &mut Track, pcm: &[f32]) {
        t.pending.extend_from_slice(pcm);
        t.total += pcm.len();
        if let Some(w) = &mut t.wav
            && let Err(e) = w.append(pcm)
        {
            eprintln!("echotype: meeting audio not saved: {e}");
        }
    }

    fn track(&mut self, src: Source) -> &mut Track {
        if src == Source::Mic { &mut self.mic } else { &mut self.system }
    }

    /// Transcribes the next `cut` samples. A failed transcription (a provider timeout, say) costs
    /// only that chunk's words, with a notice; the meeting and its audio carry on.
    fn chunk(&mut self, src: Source, cut: usize, pause: bool) {
        let t = if src == Source::Mic { &self.mic } else { &self.system };
        let t0 = t.at as f32 / RATE as f32;
        let end = t0 + cut as f32 / RATE as f32;
        let to = if pause { end } else { end - OVERLAP_S / 2.0 };
        let audio = t.pending[..cut].to_vec();
        let from = t.owned_from;
        let words = if has_voice(&audio) {
            let started = Instant::now();
            let heard = (self.transcribe)(&audio);
            self.stt_ms += started.elapsed().as_millis();
            match heard {
                Ok(w) => owned(w, t0, from, to),
                Err(e) => {
                    let mut m = lock(&self.meeting);
                    if !m.notices.iter().any(|n| n.ends_with(&e)) {
                        m.notices.push(format!("{}–{} was not transcribed (the audio is saved): {e}", clock(from), clock(to)));
                    }
                    vec![]
                }
            }
        } else {
            vec![]
        };
        let labelled = match src {
            Source::Mic => words.into_iter().map(|w| (w, ME.to_string())).collect(),
            Source::System => self.label_live(words, &audio, t0),
        };
        let t = self.track(src);
        t.words.extend(labelled);
        let keep_from = if pause { cut } else { cut - (OVERLAP_S * RATE as f32) as usize };
        t.pending.drain(..keep_from);
        t.at += keep_from;
        t.owned_from = to;
        self.rebuild();
    }

    /// Live speaker labels: one fingerprint per run of words without a long pause.
    fn label_live(&mut self, words: Vec<Word>, audio: &[f32], t0: f32) -> Vec<(Word, String)> {
        let Some(embed) = &mut self.embed else {
            return words.into_iter().map(|w| (w, OTHERS.to_string())).collect();
        };
        let mut last = self.system.words.last().map_or_else(|| "s1".to_string(), |(_, s)| s.clone());
        let mut out = vec![];
        let mut i = 0;
        while i < words.len() {
            let mut j = i + 1;
            while j < words.len() && words[j].start - words[j - 1].end < 0.6 {
                j += 1;
            }
            let (a, b) = (words[i].start, words[j - 1].end);
            if b - a >= MIN_EMBED_S {
                let hi = (((b - t0) * RATE as f32) as usize).min(audio.len());
                let lo = (((a - t0).max(0.0) * RATE as f32) as usize).min(hi);
                if let Some(e) = embed.embed(&audio[lo..hi]) {
                    last = format!("s{}", self.clusters.assign(&e) + 1);
                }
            }
            out.extend(words[i..j].iter().map(|w| (w.clone(), last.clone())));
            i = j;
        }
        out
    }

    fn rebuild(&mut self) {
        let segments = timeline(&self.mic.words, &self.system.words);
        let mut m = lock(&self.meeting);
        for s in &segments {
            m.speakers.entry(s.speaker.clone()).or_insert_with(|| default_name(&s.speaker));
        }
        m.segments = segments;
        m.duration_s = self.mic.total.max(self.system.total) as f32 / RATE as f32;
        if let Some(d) = &self.dir {
            save(d, &m);
        }
    }

    /// Transcribes what is left, then re-clusters the whole computer-audio track (`diarize` gets
    /// all of it) and re-labels every word by the speaker turn it falls in.
    pub fn finish(&mut self, diarize: Option<&mut dyn FnMut(Vec<f32>) -> Result<Vec<Turn>, String>>, system_audio: Option<Vec<f32>>) -> Result<(), String> {
        for src in [Source::Mic, Source::System] {
            while let Some((cut, _)) = next_cut(&self.track(src).pending, true) {
                self.chunk(src, cut, true);
            }
        }
        let audio = system_audio.or_else(|| self.dir.as_ref().map(|d| read_wav(&d.join("audio-system.wav"))));
        if let (Some(diarize), Some(audio)) = (diarize, audio)
            && !self.system.words.is_empty()
        {
            let turns = diarize(audio)?;
            let words: Vec<Word> = self.system.words.iter().map(|(w, _)| w.clone()).collect();
            if let Some(clusters) = sentence_speakers(&words, &turns) {
                let live: Vec<String> = self.system.words.iter().map(|(_, s)| s.clone()).collect();
                let mut m = lock(&self.meeting);
                let (ids, names) = relabel(&clusters, &live, &m.speakers);
                m.speakers = names;
                drop(m);
                for ((_, s), id) in self.system.words.iter_mut().zip(ids) {
                    *s = id;
                }
            }
        }
        self.rebuild();
        Ok(())
    }
}

// ---------- notes and export ----------

/// "Speaker: text" lines, what the summary model reads.
pub fn transcript_lines(m: &Meeting) -> String {
    for_notes(m, "")
}

/// transcript_lines with You under the user's own name (`me`, Settings → display name), so the
/// notes say "(Aditya) Send the questions" rather than "(You)".
fn for_notes(m: &Meeting, me: &str) -> String {
    let me = me.trim();
    let line = |s: &Segment| format!("{}: {}", if s.speaker == ME && !me.is_empty() { me } else { name(m, &s.speaker) }, s.text);
    m.segments.iter().map(line).collect::<Vec<_>>().join("\n")
}

fn name<'a>(m: &'a Meeting, id: &'a str) -> &'a str {
    m.speakers.get(id).map_or(id, String::as_str)
}

/// About 12k tokens, roughly 80 minutes of talk; the rest of a longer meeting is not summarised.
// ponytail: one call with a cap; summarise in parts and merge if long meetings need full notes.
pub const SUMMARY_MAX_CHARS: usize = 48_000;

/// The notes, or None when nobody said anything (the model is then never asked: with nothing to
/// go on it would only make something up). `llm(transcript)`: Echotype Cloud's /v1/summary, which
/// holds the prompt.
pub fn summarize(m: &Meeting, me: &str, llm: impl FnOnce(&str) -> Result<String, String>) -> Result<Option<String>, String> {
    let mut text = for_notes(m, me);
    if !m.segments.iter().any(|s| crate::cleanup::has_speech(&s.text)) {
        return Ok(None);
    }
    if text.len() > SUMMARY_MAX_CHARS {
        let mut cut = SUMMARY_MAX_CHARS;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(text[..cut].rfind('\n').unwrap_or(cut));
    }
    let notes = llm(&text)?;
    Ok((!notes.trim().is_empty()).then(|| notes.trim().to_string()))
}

pub fn clock(secs: f32) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

pub fn markdown(m: &Meeting) -> String {
    let mut out = format!("# {}\n\n{}\n", m.title, clock(m.duration_s));
    if let Some(s) = &m.summary {
        out.push_str(&format!("\n{s}\n"));
    }
    out.push_str("\n## Transcript\n\n");
    for s in &m.segments {
        match name(m, &s.speaker) {
            "" => out.push_str(&format!("{}\n\n", s.text)),
            who => out.push_str(&format!("**{who}** ({}): {}\n\n", clock(s.start), s.text)),
        }
    }
    out.trim_end().to_string() + "\n"
}

/// "Name: " before a line, or nothing for a transcribed file (from_text), which has no speakers.
fn said(m: &Meeting, s: &Segment) -> String {
    match name(m, &s.speaker) {
        "" => s.text.clone(),
        who => format!("{who}: {}", s.text),
    }
}

pub fn plain(m: &Meeting) -> String {
    let lines: Vec<String> = m.segments.iter().map(|s| if s.speaker.is_empty() { s.text.clone() } else { format!("[{}] {}", clock(s.start), said(m, s)) }).collect();
    format!("{}\n\n{}\n", m.title, lines.join("\n"))
}

/// "00:01:02,345" (SRT, `sep` ',') or "00:01:02.345" (WebVTT, '.').
fn timecode(secs: f32, sep: char) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}{sep}{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The transcript as subtitles, one cue a line, "Name: text".
pub fn srt(m: &Meeting) -> String {
    let cues: Vec<String> = m.segments.iter().enumerate().map(|(i, s)| format!("{}\n{} --> {}\n{}\n", i + 1, timecode(s.start, ','), timecode(s.end, ','), said(m, s))).collect();
    cues.join("\n")
}

/// WebVTT with voice tags, which players and agents read as speakers.
pub fn vtt(m: &Meeting) -> String {
    let cue = |s: &Segment| match name(m, &s.speaker) {
        "" => s.text.clone(),
        who => format!("<v {who}>{}", s.text),
    };
    let cues: Vec<String> = m.segments.iter().map(|s| format!("{} --> {}\n{}\n", timecode(s.start, '.'), timecode(s.end, '.'), cue(s))).collect();
    format!("WEBVTT\n\n{}", cues.join("\n"))
}

/// Everything an agent needs, speaker names resolved.
pub fn json(m: &Meeting) -> String {
    let segments: Vec<_> = m.segments.iter().map(|s| serde_json::json!({ "start": s.start, "end": s.end, "speaker": name(m, &s.speaker), "text": s.text })).collect();
    let v = serde_json::json!({ "title": m.title, "started_ms": m.started_ms, "duration_s": m.duration_s, "summary": m.summary, "segments": segments });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

/// Text made safe inside XML and HTML, attributes included.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The summary's lines without their Markdown marks, and whether each is a heading.
// ponytail: headings and list marks only; bold, links and tables stay as typed.
fn summary_lines(m: &Meeting) -> Vec<(bool, String)> {
    let lines = m.summary.as_deref().unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty());
    lines.map(|l| if l.starts_with('#') { (true, l.trim_start_matches('#').trim().to_string()) } else { (false, l.to_string()) }).collect()
}

/// A page that opens in any browser and pastes into Docs or Notion with its headings.
pub fn html(m: &Meeting) -> String {
    let mut out = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{t}</title>\n<style>body{{font:16px/1.55 -apple-system,system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem}}time{{color:#777;font-size:.85em;margin-left:.4em}}</style></head>\n<body>\n<h1>{t}</h1>\n<p>{}</p>\n",
        clock(m.duration_s),
        t = esc(&m.title)
    );
    for (heading, l) in summary_lines(m) {
        out.push_str(&if heading { format!("<h2>{}</h2>\n", esc(&l)) } else { format!("<p>{}</p>\n", esc(&l)) });
    }
    out.push_str("<h2>Transcript</h2>\n");
    for s in &m.segments {
        match name(m, &s.speaker) {
            "" => out.push_str(&format!("<p>{}</p>\n", esc(&s.text))),
            who => out.push_str(&format!("<p><b>{}</b><time>{}</time><br>{}</p>\n", esc(who), clock(s.start), esc(&s.text))),
        }
    }
    out + "</body></html>\n"
}

/// A Word document: the three parts Word needs, no styles part (runs carry bold and size).
pub fn docx(m: &Meeting) -> Result<Vec<u8>, String> {
    use std::io::Write;
    // (text, bold, half-points or 0 for the default size)
    let p = |runs: &[(&str, bool, u32)]| {
        let runs: String = runs
            .iter()
            .map(|(t, b, sz)| {
                let props = format!("{}{}", if *b { "<w:b/>" } else { "" }, if *sz > 0 { format!("<w:sz w:val=\"{sz}\"/>") } else { String::new() });
                format!("<w:r><w:rPr>{props}</w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc(t))
            })
            .collect();
        format!("<w:p>{runs}</w:p>")
    };
    let mut body = p(&[(&m.title, true, 36)]) + &p(&[(&clock(m.duration_s), false, 0)]);
    for (heading, l) in summary_lines(m) {
        body += &p(&[(&l, heading, if heading { 28 } else { 0 })]);
    }
    body += &p(&[("Transcript", true, 28)]);
    for s in &m.segments {
        body += &match name(m, &s.speaker) {
            "" => p(&[(&s.text, false, 0)]),
            who => p(&[(who, true, 0), (&format!("  {}  ", clock(s.start)), false, 18), (&s.text, false, 0)]),
        };
    }
    let parts = [
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#.to_string()),
        ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_string()),
        ("word/document.xml", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#)),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (path, xml) in parts {
        zip.start_file(path, opts).map_err(|e| e.to_string())?;
        zip.write_all(xml.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(zip.finish().map_err(|e| e.to_string())?.into_inner())
}

/// The meeting in `format` (md, txt, json, srt, vtt, html, docx) and its file extension. Exports are free.
pub fn export(m: &Meeting, format: &str) -> Result<(Vec<u8>, &'static str), String> {
    Ok(match format {
        "md" => (markdown(m).into_bytes(), "md"),
        "txt" => (plain(m).into_bytes(), "txt"),
        "json" => (json(m).into_bytes(), "json"),
        "srt" => (srt(m).into_bytes(), "srt"),
        "vtt" => (vtt(m).into_bytes(), "vtt"),
        "html" => (html(m).into_bytes(), "html"),
        "docx" => (docx(m)?, "docx"),
        other => return Err(format!("Unknown export format '{other}'")),
    })
}

/// A transcribed file (`text`, `secs` long) as a meeting with no speakers, one line a sentence,
/// so it exports in every meeting format.
// ponytail: cue times spread evenly by word over the file; whisper's word times
// (stt::transcribe_words) would make subtitles exact through long pauses.
pub fn from_text(title: &str, text: &str, secs: f32) -> Meeting {
    let words: Vec<(Word, String)> = spread(text, secs).into_iter().map(|w| (w, String::new())).collect();
    Meeting { title: title.into(), duration_s: secs, state: "done".into(), segments: sentences(&words), ..Default::default() }
}

// ---------- names ----------

/// Capitalised words that open sentences or follow "I'm" without being anyone's name.
const NOT_NAMES: &[&str] = &[
    "I", "So", "Yeah", "Yes", "No", "Okay", "Ok", "Sure", "Sir", "Madam", "Guys", "Everyone", "All", "Team", "Bro", "Dude", "Man", "God", "Sorry", "Fine", "Good",
    "Great", "Here", "There", "Just", "Not", "Going", "Done", "Back", "Ready", "Also", "Well", "Hello", "Hi", "Hey", "Thanks", "Thank", "Right", "Cool", "Perfect", "The",
    "This", "That", "It", "We", "You", "He", "She", "They", "My", "Your", "Mr", "Mrs", "Ms", "Dr", "Doctor", "Today", "Monday", "Tuesday", "Wednesday", "Thursday",
    "Friday", "Saturday", "Sunday", "Again", "Now", "Then", "Sounds", "Exactly", "Absolutely", "Totally", "Really", "Very", "Everybody", "Folks", "Mate", "Boss",
];

/// A name at words[i]: one capitalised word of letters, not a sentence opener.
fn name_at(words: &[&str], i: usize) -> Option<String> {
    let w = words.get(i)?.trim_matches(|c: char| !c.is_alphanumeric());
    let mut cs = w.chars();
    let ok = w.len() >= 2 && cs.next().is_some_and(char::is_uppercase) && cs.all(char::is_lowercase) && !NOT_NAMES.contains(&w);
    ok.then(|| w.to_string())
}

/// Names for the speakers, from what was said: "I'm Priya" or "my name is Priya" names who said
/// it; "Thanks, Priya" names the last other voice; "Priya, can you…" the next one. Suggestions
/// only (the user confirms each), never for You, and each name goes to one speaker.
// ponytail: English phrasings only; a calendar invite's attendees would catch the rest.
pub fn name_hints(m: &Meeting) -> BTreeMap<String, String> {
    let mut votes: BTreeMap<(String, String), u32> = BTreeMap::new();
    let mut vote = |speaker: Option<&str>, name: Option<String>, w: u32| {
        if let (Some(sp), Some(n)) = (speaker.filter(|s| *s != ME), name) {
            *votes.entry((sp.to_string(), n)).or_default() += w;
        }
    };
    let other = |i: usize, back: bool| -> Option<&str> {
        let me = &m.segments[i].speaker;
        let found = if back { m.segments[..i].iter().rev().find(|s| &s.speaker != me) } else { m.segments[i + 1..].iter().find(|s| &s.speaker != me) };
        found.map(|s| s.speaker.as_str())
    };
    for (i, seg) in m.segments.iter().enumerate() {
        let words: Vec<&str> = seg.text.split_whitespace().collect();
        let low: Vec<String> = words.iter().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').to_lowercase()).collect();
        let at = |j: usize, s: &str| low.get(j).is_some_and(|w| w == s);
        for j in 0..low.len() {
            if at(j, "i'm") || (at(j, "i") && at(j + 1, "am")) {
                vote(Some(&seg.speaker), name_at(&words, j + if at(j, "i") { 2 } else { 1 }), 3);
            }
            if at(j, "my") && at(j + 1, "name") && at(j + 2, "is") {
                vote(Some(&seg.speaker), name_at(&words, j + 3), 3);
            }
            if at(j, "thanks") || (at(j, "thank") && at(j + 1, "you")) {
                vote(other(i, true), name_at(&words, j + if at(j, "thanks") { 1 } else { 2 }), 1);
            }
        }
        // "Priya, can you…": a name opening the line, then a comma and a question.
        let asks = ["can", "could", "what", "how", "do", "would", "will", "are", "did", "is"];
        if words.first().is_some_and(|w| w.ends_with(',')) && low.get(1).is_some_and(|w| asks.contains(&w.as_str())) {
            vote(other(i, false), name_at(&words, 0), 1);
        }
    }
    let mut best: Vec<((String, String), u32)> = votes.into_iter().collect();
    best.sort_by(|a, b| b.1.cmp(&a.1));
    let mut out = BTreeMap::new();
    let mut taken = HashSet::new();
    for ((speaker, name), _) in best {
        if !out.contains_key(&speaker) && taken.insert(name.clone()) {
            out.insert(speaker, name);
        }
    }
    out
}

// ---------- storage ----------

pub fn meetings_dir() -> PathBuf {
    settings::data_dir().join("meetings")
}

fn save(dir: &Path, m: &Meeting) {
    let tmp = dir.join(format!("meeting.json.{:?}.tmp", std::thread::current().id()).replace(['(', ')'], ""));
    let res = serde_json::to_vec_pretty(m).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string())).and_then(|_| std::fs::rename(&tmp, dir.join("meeting.json")).map_err(|e| e.to_string()));
    if let Err(e) = res {
        eprintln!("echotype: could not save meeting notes: {e}");
    }
}

/// The folder of meeting `id`; ids are folder names Echotype made, nothing else is accepted.
fn dir_of(id: &str) -> Result<PathBuf, String> {
    let ok = !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let d = meetings_dir().join(id);
    if ok && d.join("meeting.json").is_file() { Ok(d) } else { Err("That meeting no longer exists".into()) }
}

fn live_id() -> Option<String> {
    lock(&LIVE).as_ref().map(|l| l.id.clone())
}

pub fn load(id: &str) -> Result<Meeting, String> {
    let bytes = std::fs::read(dir_of(id)?.join("meeting.json")).map_err(|e| e.to_string())?;
    let mut m: Meeting = serde_json::from_slice(&bytes).map_err(|e| format!("The meeting file is damaged: {e}"))?;
    if matches!(m.state.as_str(), "recording" | "processing") && live_id().as_deref() != Some(id) {
        m.state = "interrupted".into();
    }
    Ok(m)
}

/// Newest first, without transcripts.
pub fn list() -> Vec<Meeting> {
    let mut out: Vec<Meeting> = std::fs::read_dir(meetings_dir())
        .map(|rd| rd.flatten().filter_map(|e| load(&e.file_name().to_string_lossy()).ok()).collect())
        .unwrap_or_default();
    for m in &mut out {
        m.segments.clear();
        m.summary = None;
    }
    out.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
    out
}

/// Renames a speaker everywhere in the meeting: every line, and the summary's mentions of them.
pub fn rename(m: &mut Meeting, speaker: &str, new: &str) -> Result<(), String> {
    let new = new.trim();
    if new.is_empty() || new.chars().count() > 60 {
        return Err("Names can be 1 to 60 characters".into());
    }
    let old = std::mem::replace(m.speakers.get_mut(speaker).ok_or("No such speaker in this meeting")?, new.to_string());
    // "You" and "Others" are also ordinary words, so the summary keeps them.
    if let Some(s) = m.summary.as_mut().filter(|_| speaker != ME && speaker != OTHERS) {
        *s = crate::cleanup::apply_replacements(s, &[settings::Replacement { from: old, to: new.to_string() }]);
    }
    Ok(())
}

pub fn rename_speaker(id: &str, speaker: &str, new: &str) -> Result<Meeting, String> {
    let apply = |m: &mut Meeting| rename(m, speaker, new);
    // The live meeting's copy is the one its worker keeps saving.
    let shared = lock(&LIVE).as_ref().filter(|l| l.id == id).map(|l| l.meeting.clone());
    if let Some(shared) = shared {
        let mut m = lock(&shared);
        apply(&mut m)?;
        save(&dir_of(id)?, &m);
        return Ok(m.clone());
    }
    let mut m = load(id)?;
    apply(&mut m)?;
    save(&dir_of(id)?, &m);
    Ok(m)
}

pub fn delete(id: &str) -> Result<(), String> {
    if live_id().as_deref() == Some(id) {
        return Err("Stop this meeting before deleting it".into());
    }
    std::fs::remove_dir_all(dir_of(id)?).map_err(|e| format!("Could not delete the meeting: {e}"))
}

// ---------- old audio ----------

const AUDIO: [&str; 2] = ["audio-mic.wav", "audio-system.wav"];

/// Whether meeting `m`'s audio is due to go: `days` (0 = never) after the meeting ended. A meeting
/// still recording or being written up is never touched.
pub fn audio_expired(m: &Meeting, days: u32, now_ms: u64) -> bool {
    let ended = m.started_ms + (m.duration_s.max(0.0) * 1000.0) as u64;
    days > 0 && !matches!(m.state.as_str(), "recording" | "processing") && now_ms.saturating_sub(ended) > u64::from(days) * 86_400_000
}

/// Deletes the WAVs of meetings that ended more than `days` ago (about 52 MB a track an hour) and
/// says so on the meeting. meeting.json stays: the transcript, speakers and summary are kept.
pub fn prune_audio(days: u32) {
    let Ok(rd) = std::fs::read_dir(meetings_dir()) else { return };
    let now = now_ms();
    for e in rd.flatten() {
        let dir = e.path();
        if !AUDIO.iter().any(|f| dir.join(f).is_file()) {
            continue; // already pruned: skip reading its transcript
        }
        let Ok(mut m) = load(&e.file_name().to_string_lossy()) else { continue };
        if !audio_expired(&m, days, now) {
            continue;
        }
        for f in AUDIO {
            let _ = std::fs::remove_file(dir.join(f));
        }
        if AUDIO.iter().any(|f| dir.join(f).is_file()) {
            eprintln!("echotype: could not delete the old audio of meeting {}", m.id);
            continue;
        }
        m.notices.push(format!("The audio was deleted {days} days after the meeting to save space (Settings → Meetings). The transcript and notes stay."));
        save(&dir, &m);
    }
}

// ---------- search ----------

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Snippet {
    pub start: f32,
    pub text: String,
}

#[derive(Serialize, Debug)]
pub struct Hit {
    pub id: String,
    pub title: String,
    pub started_ms: u64,
    pub snippets: Vec<Snippet>,
}

const SNIPPET_CHARS: usize = 160;

/// Lowercase, one char for one char, so a match's char position is also its place in the original.
fn fold(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// Up to 3 lines of `m` that hold the most `words` (folded), in time order, or None unless every
/// word is somewhere in its title, transcript or summary.
pub fn search_meeting(m: &Meeting, words: &[String]) -> Option<Vec<Snippet>> {
    let lines: Vec<String> = m.segments.iter().map(|s| fold(&s.text)).collect();
    let other = [fold(&m.title), fold(m.summary.as_deref().unwrap_or_default())];
    if words.is_empty() || !words.iter().all(|w| other.iter().chain(&lines).any(|t| t.contains(w.as_str()))) {
        return None;
    }
    let mut best: Vec<(usize, usize)> = lines.iter().enumerate().map(|(i, l)| (words.iter().filter(|w| l.contains(w.as_str())).count(), i)).filter(|(n, _)| *n > 0).collect();
    best.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    best.truncate(3);
    best.sort_by_key(|&(_, i)| i);
    Some(best.into_iter().map(|(_, i)| snippet(&m.segments[i], &lines[i], words)).collect())
}

/// The line, or SNIPPET_CHARS of it from a little before its first match.
fn snippet(s: &Segment, low: &str, words: &[String]) -> Snippet {
    let n = s.text.chars().count();
    if n <= SNIPPET_CHARS {
        return Snippet { start: s.start, text: s.text.clone() };
    }
    let first = words.iter().filter_map(|w| low.find(w.as_str())).min().unwrap_or(0);
    let from = low[..first].chars().count().saturating_sub(40).min(n - SNIPPET_CHARS);
    let mut text: String = s.text.chars().skip(from).take(SNIPPET_CHARS).collect();
    if from > 0 {
        text = format!("…{}", text.trim_start());
    }
    if from + SNIPPET_CHARS < n {
        text = format!("{}…", text.trim_end());
    }
    Snippet { start: s.start, text }
}

/// Meetings where every word of `query` appears (any case), newest first.
// ponytail: reads every meeting.json per query (hundreds are fine); keep an index if thousands get slow.
pub fn search(query: &str) -> Vec<Hit> {
    let words: Vec<String> = query.split_whitespace().map(fold).collect();
    if words.is_empty() {
        return vec![];
    }
    let hit = |m: Meeting| search_meeting(&m, &words).map(|snippets| Hit { id: m.id, title: m.title, started_ms: m.started_ms, snippets });
    let mut out: Vec<Hit> = std::fs::read_dir(meetings_dir()).map(|rd| rd.flatten().filter_map(|e| load(&e.file_name().to_string_lossy()).ok().and_then(hit)).collect()).unwrap_or_default();
    out.sort_by(|a, b| b.started_ms.cmp(&a.started_ms));
    out
}

/// UTC "2026-10-14-090512" for folder names (civil-from-days, Howard Hinnant).
fn stamp(ms: u64) -> String {
    let secs = ms / 1000;
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}-{:02}{:02}{:02}", secs / 3600 % 24, secs / 60 % 60, secs % 60)
}

fn slug(title: &str) -> String {
    let s: String = title.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    s.split('-').filter(|p| !p.is_empty()).take(6).collect::<Vec<_>>().join("-")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

// ---------- transcription for these settings ----------

fn transcriber(s: &Settings) -> Result<Transcribe, String> {
    // Meetings are transcribed in the language spoken; the auto transform (cleanup.rs) does not apply.
    let s = s.clone();
    stt::ready(&s)?;
    let prompt = stt::prompt(&s);
    if s.stt.provider == "local" {
        let model = stt::pick_local_model(&s)?;
        let langs = stt::languages(&s);
        Ok(Box::new(move |pcm| Ok(latin(&s, stt::transcribe_words(&model, pcm, &langs, &prompt, speakers::vad().as_deref())?))))
    } else {
        Ok(Box::new(move |pcm| Ok(latin(&s, spread(&stt::transcribe_remote(&s, pcm, &prompt)?, pcm.len() as f32 / RATE as f32)))))
    }
}

/// Each word in English letters when the user wants them (translit), a capital after . ? !
// ponytail: a chunk starts with a capital even mid-sentence; carry the last word across chunks if it shows.
fn latin(s: &Settings, mut words: Vec<Word>) -> Vec<Word> {
    if crate::translit::active(s) {
        let mut cap = true;
        for w in &mut words {
            w.text = crate::translit::convert(&w.text, cap);
            cap = w.text.ends_with(['.', '?', '!']);
        }
    }
    words
}

/// Loads the fingerprint model, downloading both speaker models first when missing.
fn embedder(mut progress: impl FnMut(&str, u64, Option<u64>)) -> Result<Embedder, String> {
    if !SPEAKERS {
        return Embedder::new(); // says why; nothing to download for it
    }
    speakers::ensure(&mut progress)?;
    Embedder::new()
}

// ---------- the live meeting (app) ----------

struct Live {
    id: String,
    started_ms: u64,
    started: Instant,
    /// Some while recording; taken by stop.
    capture: Option<Capture>,
    stopped: Arc<AtomicBool>,
    meeting: Arc<Mutex<Meeting>>,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);
static TRAY_ITEM: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>> = Mutex::new(None);

/// The tray's "Start meeting notes" item, relabelled while a meeting runs.
pub fn set_tray_item(item: tauri::menu::MenuItem<tauri::Wry>) {
    *lock(&TRAY_ITEM) = Some(item);
}

pub fn recording() -> bool {
    lock(&LIVE).as_ref().is_some_and(|l| l.capture.is_some())
}

/// The recording meeting's id.
pub fn recording_id() -> Option<String> {
    lock(&LIVE).as_ref().filter(|l| l.capture.is_some()).map(|l| l.id.clone())
}

/// "Start notes · Zoom call" while a detected call is on offer (calls.rs).
pub fn menu_label() -> String {
    match crate::calls::offered_title() {
        _ if recording() => "Stop meeting notes".into(),
        Some(title) => format!("Start notes · {title}"),
        None => "Start meeting notes".into(),
    }
}

#[derive(Serialize, Clone)]
struct Event<'a> {
    id: &'a str,
    state: &'a str,
}

/// `meeting` → every window: the Meetings page refetches, and the pill can show its own state.
// TODO(pill): the overlay could show "Meeting · 12:34" from this event; until then the menu bar
// title (tray) carries the meeting state, so the pill code stays untouched here.
fn emit(app: &AppHandle, id: &str, state: &str) {
    let _ = app.emit("meeting", Event { id, state });
}

/// Tray and pill menu: start, or stop the running meeting.
/// Runs off the main thread: starting waits on ScreenCaptureKit and the permission check.
pub fn toggle(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let res = if recording() { stop(&app) } else { crate::calls::start_offered(&app) };
        if let Err(e) = res {
            crate::emit_state(&app, crate::DictationState { state: "error", error: Some(e), ..Default::default() });
        }
    });
}

/// Set while a start is opening the streams, so LIVE is not held across that (the tray reads it).
static STARTING: AtomicBool = AtomicBool::new(false);

pub fn start(app: &AppHandle, title: Option<String>) -> Result<Meeting, String> {
    if crate::notices::blocked() {
        return Err("This version of Echotype is retired. Update it in Account to take meeting notes.".into());
    }
    if let Some(l) = lock(&LIVE).as_ref() {
        return Err(if l.capture.is_some() { "A meeting is already being transcribed" } else { "Still writing up the last meeting. Try again in a moment." }.into());
    }
    if STARTING.swap(true, Ordering::SeqCst) {
        return Err("A meeting is already starting".into());
    }
    let res = open(app, title);
    STARTING.store(false, Ordering::SeqCst);
    res
}

fn open(app: &AppHandle, title: Option<String>) -> Result<Meeting, String> {
    let s = crate::current_settings(app);
    let transcribe = transcriber(&s)?;
    let started_ms = now_ms();
    let title = title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Meeting".into());
    let id = format!("{}-{}", stamp(started_ms), slug(&title));
    let dir = meetings_dir().join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create the meeting folder: {e}"))?;
    let mut m = Meeting { id: id.clone(), title, started_ms, state: "recording".into(), ..Default::default() };
    m.speakers.insert(ME.into(), default_name(ME));
    save(&dir, &m);
    let started = Instant::now();
    let (tx, rx) = mpsc::channel::<Piece>();
    let opened = Session::new(m.clone(), Some(dir.clone()), transcribe, None).and_then(|session| Ok((session, Capture::start(s.input_device.clone(), true, started, tx)?)));
    let (session, (capture, notice)) = match opened {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let system = notice.is_none();
    m.notices.extend(notice);
    *lock(&session.meeting) = m.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let notes = capture.notes.clone();
    *lock(&LIVE) = Some(Live { id: id.clone(), started_ms, started, capture: Some(capture), stopped: stopped.clone(), meeting: session.meeting.clone() });
    let app2 = app.clone();
    std::thread::spawn(move || run(app2, session, rx, stopped, system, notes));
    tick(app);
    crate::emit_state(app, crate::hint("Meeting notes on. Let everyone know this meeting is being transcribed.", None));
    emit(app, &id, "recording");
    show_window(app, &id);
    crate::telemetry::meeting(&s, false);
    Ok(m)
}

pub fn stop(app: &AppHandle) -> Result<(), String> {
    let (capture, stopped) = {
        let mut live = lock(&LIVE);
        let l = live.as_mut().ok_or("No meeting is being transcribed")?;
        (l.capture.take().ok_or("No meeting is being transcribed")?, l.stopped.clone())
    };
    capture.stop(); // joins both streams: every sample has been sent
    stopped.store(true, Ordering::SeqCst);
    refresh_tray(app, None);
    Ok(())
}

/// Speaker detection (sherpa-onnx) is linked on macOS only; elsewhere other voices are "Others".
const SPEAKERS: bool = cfg!(target_os = "macos");

/// For the Meetings page: the live meeting (after a reload), whether computer audio can be
/// captured on this OS and is allowed, whether voices are told apart, and what the first meeting
/// will download.
pub fn status() -> serde_json::Value {
    let live = lock(&LIVE).as_ref().map(|l| serde_json::json!({ "id": l.id, "started_ms": l.started_ms, "recording": l.capture.is_some() }));
    let download_mb: u64 = speakers::MODELS.iter().filter(|m| SPEAKERS && !speakers::path(m).is_file()).map(|m| m.size_mb).sum();
    serde_json::json!({
        "live": live,
        "computer_audio": cfg!(any(target_os = "macos", target_os = "windows")),
        "speakers": SPEAKERS,
        "screen_recording": capture::screen_recording_allowed(),
        "download_mb": download_mb,
        "summaries": crate::settings::CLOUD,
    })
}

pub(crate) fn refresh_tray(app: &AppHandle, elapsed: Option<Duration>) {
    let title = elapsed.map(|e| format!("Meeting · {}", clock(e.as_secs_f32())));
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_title(title.as_deref());
        let _ = tray.set_tooltip(Some(title.as_deref().unwrap_or("Echotype")));
    }
    let item = lock(&TRAY_ITEM).clone(); // set_text waits on the main thread: never hold the lock across it
    if let Some(item) = item {
        let _ = item.set_text(menu_label());
    }
}

/// Menu bar title "Meeting · 12:34", once a second while recording.
fn tick(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            let elapsed = lock(&LIVE).as_ref().filter(|l| l.capture.is_some()).map(|l| l.started.elapsed());
            let Some(e) = elapsed else { break };
            refresh_tray(&app, Some(e));
            std::thread::sleep(Duration::from_secs(1));
        }
        refresh_tray(&app, None);
    });
}

/// The worker: owns the session, so audio, transcription and saving happen in order.
fn run(app: AppHandle, mut session: Session, rx: mpsc::Receiver<Piece>, stopped: Arc<AtomicBool>, system: bool, notes: Arc<Mutex<Vec<String>>>) {
    /// However the worker ends, even by a panic, LIVE is cleared so a new meeting can start (the
    /// saved state then reads "interrupted") and the capture threads are told to stop.
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            lock(&LIVE).take();
            stt::free_vad(); // its long-meeting state goes; dictation reloads the detector
        }
    }
    let _done = Done;
    let id = lock(&session.meeting).id.clone();
    let note = |session: &Session, n: String| lock(&session.meeting).notices.push(n);
    // The first meeting downloads the speaker models; that must not hold up live transcription.
    let (models_tx, models) = mpsc::channel();
    if system {
        std::thread::spawn(move || models_tx.send(embedder(|_, _, _| {})));
    }
    let mut waiting = system;
    let take_models = |session: &mut Session, r: Result<Embedder, String>| {
        match r {
            Ok(e) => session.embed = Some(e),
            Err(e) => note(session, format!("Other voices are labelled Others: the speaker models are not available ({e})")),
        }
    };
    let capture_notes = |session: &Session| {
        let new: Vec<String> = lock(&notes).drain(..).collect();
        lock(&session.meeting).notices.extend(new);
    };
    loop {
        if let Ok(r) = models.try_recv() {
            waiting = false;
            take_models(&mut session, r);
        }
        capture_notes(&session);
        let piece = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(p) => p,
            Err(mpsc::RecvTimeoutError::Timeout) if stopped.load(Ordering::SeqCst) => match rx.try_recv() {
                Ok(p) => p,
                Err(_) => break,
            },
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if session.push(piece.0, &piece.1, Some(piece.2)) {
            emit(&app, &id, "recording");
        }
    }
    capture_notes(&session); // the capture threads have ended: their last notices are in
    lock(&session.meeting).state = "processing".into();
    emit(&app, &id, "processing");
    if waiting {
        match models.recv_timeout(Duration::from_secs(120)) {
            Ok(r) => take_models(&mut session, r),
            Err(_) => note(&session, "Speakers were not told apart: the speaker models are still downloading".into()),
        }
    }
    let mut diarize = |pcm: Vec<f32>| speakers::diarize(pcm, None);
    let can_diarize = session.embed.is_some();
    if let Err(e) = session.finish(can_diarize.then_some(&mut diarize as _), None) {
        note(&session, format!("Speakers were not re-checked at the end: {e}"));
    }
    {
        let mut m = lock(&session.meeting);
        m.state = "done".into();
        if let Some(d) = &session.dir {
            save(d, &m);
        }
    }
    drop(_done);
    crate::telemetry::meeting(&crate::current_settings(&app), true);
    emit(&app, &id, "done");
}

// ---------- commands ----------

type R<T> = Result<T, String>;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> R<T> + Send + 'static) -> R<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn meeting_start(app: AppHandle, title: Option<String>) -> R<Meeting> {
    blocking(move || start(&app, title)).await
}

#[tauri::command]
pub async fn meeting_stop(app: AppHandle) -> R<()> {
    blocking(move || stop(&app)).await
}

#[tauri::command]
pub async fn meeting_status() -> R<serde_json::Value> {
    Ok(status())
}

#[tauri::command]
pub async fn meeting_list() -> R<Vec<Meeting>> {
    blocking(|| Ok(list())).await
}

#[tauri::command]
pub async fn meeting_get(id: String) -> R<Meeting> {
    blocking(move || {
        let mut m = load(&id)?;
        let unnamed = |sp: &String| m.speakers.get(sp).is_none_or(|n| *n == default_name(sp));
        m.name_hints = name_hints(&m).into_iter().filter(|(sp, _)| unnamed(sp)).collect();
        Ok(m)
    })
    .await
}

#[tauri::command]
pub async fn meeting_search(query: String) -> R<Vec<Hit>> {
    blocking(move || Ok(search(&query))).await
}

#[tauri::command]
pub async fn meeting_rename_speaker(id: String, speaker: String, name: String) -> R<Meeting> {
    blocking(move || rename_speaker(&id, &speaker, &name)).await
}

#[tauri::command]
pub async fn meeting_delete(id: String) -> R<()> {
    blocking(move || delete(&id)).await
}

/// The meeting as Markdown ("md") or plain text ("txt"), for Copy.
#[tauri::command]
pub async fn meeting_export(id: String, format: String) -> R<String> {
    blocking(move || export(&load(&id)?, &format).and_then(|(body, _)| String::from_utf8(body).map_err(|_| format!("{format} is not text")))).await
}

/// Echotype Pro: an answer about the meeting so far (or all of it), from Echotype Cloud.
#[tauri::command]
pub async fn meeting_ask(app: AppHandle, id: String, question: String) -> R<String> {
    let me = crate::current_settings(&app).display_name;
    blocking(move || {
        if !crate::settings::CLOUD {
            return Err("Asking about a meeting is coming with Echotype Pro.".into());
        }
        let m = load(&id)?;
        if !m.segments.iter().any(|s| crate::cleanup::has_speech(&s.text)) {
            return Err("Nothing has been said yet.".into());
        }
        // The latest part of a long meeting: "what did I miss?" is about the last few minutes.
        let text = for_notes(&m, &me);
        let mut from = text.len().saturating_sub(SUMMARY_MAX_CHARS);
        while !text.is_char_boundary(from) {
            from += 1;
        }
        crate::cloud::ask(&text[from..], question.trim())
    })
    .await
}

/// Opens the meeting window, or brings it forward on meeting `id`.
#[tauri::command]
pub async fn meeting_window(app: AppHandle, id: String) -> R<()> {
    show_window(&app, &id);
    Ok(())
}

/// The meeting window (like Wispr Flow's): the live transcript, Stop, Summary and Ask beside the
/// call, opened when meeting notes start. It shows meeting `id`; a second call switches it.
pub fn show_window(app: &AppHandle, id: &str) {
    if let Some(w) = app.get_webview_window("meeting") {
        let _ = app.emit_to("meeting", "meeting-show", id);
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    let url = WebviewUrl::App(format!("meeting.html?id={id}").into());
    match WebviewWindowBuilder::new(app, "meeting", url).title("Meeting notes").inner_size(440.0, 720.0).min_inner_size(340.0, 420.0).build() {
        Ok(_) => crate::apply_theme(app, &crate::current_settings(app)),
        Err(e) => eprintln!("echotype: meeting window: {e}"),
    }
}

/// Echotype Pro: the finished meeting's notes, written on request by Echotype Cloud and saved.
#[tauri::command]
pub async fn meeting_summarize(app: AppHandle, id: String) -> R<Meeting> {
    let me = crate::current_settings(&app).display_name;
    blocking(move || {
        if !crate::settings::CLOUD {
            return Err("Meeting summaries are coming with Echotype Pro.".into());
        }
        let dir = dir_of(&id)?;
        let mut m = load(&id)?;
        if m.state != "done" {
            return Err("The meeting is still being written up.".into());
        }
        let Some(notes) = summarize(&m, &me, crate::cloud::summary)? else {
            return Err("Nothing was said, so there is nothing to summarise.".into());
        };
        if transcript_lines(&m).len() > SUMMARY_MAX_CHARS && !m.notices.iter().any(|n| n.contains("covers its first part")) {
            m.notices.push("The meeting is long, so the summary covers its first part only".into());
        }
        m.summary = Some(notes);
        save(&dir, &m);
        Ok(m)
    })
    .await
}

/// Writes the export to Downloads and shows it in Finder.
#[tauri::command]
pub async fn meeting_save(app: AppHandle, id: String, format: String) -> R<String> {
    blocking(move || {
        use tauri_plugin_opener::OpenerExt;
        let m = load(&id)?;
        let (body, ext) = export(&m, &format)?;
        let dir = dirs::download_dir().ok_or("No Downloads folder")?;
        let path = dir.join(format!("{}-{}.{ext}", slug(&m.title), &stamp(m.started_ms)[..10]));
        std::fs::write(&path, body).map_err(|e| format!("Could not save: {e}"))?;
        let _ = app.opener().reveal_item_in_dir(&path);
        Ok(path.to_string_lossy().into_owned())
    })
    .await
}

// ---------- CLI ----------

/// `echotype --meeting <audio file> [--mic <audio file>] [--speakers N] [--model <ggml .bin>]`: the
/// meeting pipeline on a recording (computer audio, plus the microphone track when given, as a
/// saved meeting's audio-system.wav and audio-mic.wav), printed as a speaker-labelled transcript.
pub fn cli(args: &[String]) -> i32 {
    let mut s = Settings::load();
    let (mut file, mut mic, mut n) = (None, None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--meeting" => file = it.next().cloned(),
            "--mic" => mic = it.next().cloned(),
            "--speakers" => n = it.next().and_then(|v| v.parse::<i32>().ok()).filter(|v| *v > 0),
            "--model" => {
                if let Some(m) = it.next() {
                    s.stt.provider = "local".into();
                    s.stt.local_model = std::fs::canonicalize(m).map_or(m.clone(), |p| p.to_string_lossy().into_owned());
                    s.advanced = true; // a model named here is a manual pick
                }
            }
            other => {
                eprintln!("echotype: unknown argument '{other}'");
                file = None;
                break;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("usage: echotype --meeting <audio file> [--mic <audio file>] [--speakers N] [--model <path to ggml .bin>]");
        return 2;
    };
    stt::quiet_whisper_logs();
    let code = match cli_run(&s, &file, mic.as_deref(), n) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    stt::unload();
    code
}

fn cli_run(s: &Settings, file: &str, mic: Option<&str>, n: Option<i32>) -> Result<String, String> {
    let read = |f: &str| audio::decode(std::fs::read(f).map_err(|e| format!("Could not read {f}: {e}"))?, Path::new(f).extension().and_then(|e| e.to_str()));
    let pcm = read(file)?;
    let mic = mic.map(read).transpose()?.unwrap_or_default();
    let secs = pcm.len() as f32 / RATE as f32;
    stt::warm_up(s); // dictation's model, loaded as in the app, so "after" shows only what the meeting freed
    let before = rss_mb();
    for m in speakers::MODELS.iter().filter(|m| !speakers::path(m).is_file()) {
        eprintln!("downloading {} ({} MB, {})", m.file, m.size_mb, m.license);
    }
    let embed = embedder(|_, _, _| {}).map_err(|e| format!("speaker models: {e}"))?;
    let m = Meeting { title: Path::new(file).file_stem().map_or_else(|| "Meeting".into(), |f| f.to_string_lossy().into_owned()), ..Default::default() };
    let mut session = Session::new(m, None, transcriber(s)?, Some(embed))?;
    let t = Instant::now();
    // One second of each track in turn, as the live capture delivers them.
    for i in (0..pcm.len().max(mic.len())).step_by(RATE) {
        for (src, track) in [(Source::System, &pcm), (Source::Mic, &mic)] {
            if let Some(piece) = track.get(i..(i + RATE).min(track.len())).filter(|p| !p.is_empty()) {
                session.push(src, piece, None);
            }
        }
    }
    let mut diarize_ms = 0;
    let mut diarize = |p: Vec<f32>| {
        let t = Instant::now();
        let r = speakers::diarize(p, n);
        diarize_ms = t.elapsed().as_millis();
        r
    };
    session.finish(Some(&mut diarize), Some(pcm))?;
    let total = t.elapsed().as_secs_f32();
    let (stt_ms, out) = (session.stt_ms, plain(&lock(&session.meeting)).trim_end().to_string());
    drop(session); // the speaker models go with it
    stt::free_vad();
    eprintln!(
        "audio {secs:.1} s, transcription {:.1} s, final diarization {:.1} s, total {total:.1} s, real-time factor {:.3}, RSS (footprint) before {} ({}) MB, peak {} MB, after {} ({}) MB",
        stt_ms as f32 / 1000.0,
        diarize_ms as f32 / 1000.0,
        total / secs.max(0.001),
        before.0,
        before.1,
        peak_rss_mb(),
        rss_mb().0,
        rss_mb().1
    );
    Ok(out)
}

/// (resident, physical footprint) right now, MB. Footprint leaves out freed memory the allocator
/// has not handed back yet, which resident still counts.
fn rss_mb() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    // SAFETY: proc_pid_rusage fills a zeroed rusage_info_v2 for this process.
    unsafe {
        let mut ri: libc::rusage_info_v2 = std::mem::zeroed();
        if libc::proc_pid_rusage(std::process::id() as i32, libc::RUSAGE_INFO_V2, &mut ri as *mut _ as *mut _) == 0 {
            return (ri.ri_resident_size >> 20, ri.ri_phys_footprint >> 20);
        }
    }
    (0, 0)
}

fn peak_rss_mb() -> u64 {
    #[cfg(target_os = "macos")]
    // SAFETY: getrusage fills a zeroed rusage; ru_maxrss is bytes on macOS.
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        return u.ru_maxrss as u64 >> 20;
    }
    #[allow(unreachable_code)]
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: f32, end: f32, text: &str) -> Word {
        Word { start, end, text: text.into() }
    }

    fn tone(secs: f32, db: f32) -> Vec<f32> {
        let a = 10f32.powf(db / 20.0) * 2f32.sqrt();
        (0..(secs * RATE as f32) as usize).map(|i| a * (i as f32 * 0.1).sin()).collect()
    }

    #[test]
    fn chunks_end_at_the_first_pause_after_4_s_else_at_25_s() {
        assert_eq!(next_cut(&tone(10.0, -20.0), false), None, "no pause yet: wait");
        assert_eq!(next_cut(&tone(10.0, -20.0), true), Some((10 * RATE, true)), "flush takes the rest");
        assert_eq!(next_cut(&tone(3.0, -20.0), true), Some((3 * RATE, true)));
        assert_eq!(next_cut(&[], true), None);
        // A live line: 5 s of speech, a 0.6 s pause, more speech: cut in the pause.
        let mut pcm = tone(5.0, -20.0);
        pcm.extend(vec![0.0; RATE * 6 / 10]);
        pcm.extend(tone(1.0, -20.0));
        let (cut, pause) = next_cut(&pcm, false).unwrap();
        assert!(pause && cut > 5 * RATE && cut < 5 * RATE + RATE * 6 / 10, "cut at {cut}");
        // A pause before 4 s is too early for a line of its own.
        let mut early = tone(2.0, -20.0);
        early.extend(vec![0.0; RATE * 6 / 10]);
        early.extend(tone(3.0, -20.0));
        assert_eq!(next_cut(&early, false), None);
        // Speech with a 0.6 s pause at 20 s, all of it waiting (25 s+): cut in the pause.
        let mut pcm = tone(20.0, -20.0);
        pcm.extend(vec![0.0; RATE * 6 / 10]);
        pcm.extend(tone(10.0, -20.0));
        let (cut, pause) = next_cut(&pcm, false).unwrap();
        assert!(pause && cut > 20 * RATE && cut < 20 * RATE + RATE * 6 / 10, "cut at {cut}");
        // No pause anywhere: hard cut at 25 s, overlap handled by the session.
        assert_eq!(next_cut(&tone(30.0, -20.0), false), Some((25 * RATE, false)));
        // A pause before the look-back window does not count.
        let mut early = tone(5.0, -20.0);
        early.extend(vec![0.0; RATE]);
        early.extend(tone(25.0, -20.0));
        assert_eq!(next_cut(&early, false), Some((25 * RATE, false)));
    }

    #[test]
    fn overlapping_chunks_keep_each_word_once() {
        // Chunk A covered 0..25 s (hard cut, owns up to 24.5); chunk B starts at 24 s.
        let a = owned(vec![w(23.0, 23.6, "fine"), w(24.1, 24.4, "so"), w(24.6, 25.0, "the")], 0.0, 0.0, 24.5);
        let b = owned(vec![w(0.1, 0.4, "so"), w(0.6, 1.0, "the"), w(1.1, 1.5, "plan")], 24.0, 24.5, 49.5);
        let all: Vec<&str> = a.iter().chain(&b).map(|w| w.text.as_str()).collect();
        assert_eq!(all, ["fine", "so", "the", "plan"]);
        assert_eq!(b[0].start, 24.6, "moved onto the meeting clock");
    }

    #[test]
    fn session_chunks_live_and_flushes_the_rest_without_repeats() {
        // A fake recognizer that "hears" one word per second of loud audio, at its real time.
        let fake: Transcribe = Box::new(|pcm: &[f32]| {
            Ok((0..pcm.len() / RATE).filter(|i| audio::dbfs(&pcm[i * RATE..(i + 1) * RATE]) > -40.0).map(|i| w(i as f32 + 0.2, i as f32 + 0.8, "x")).collect())
        });
        let mut s = Session::new(Meeting::default(), None, fake, None).unwrap();
        let pcm = tone(61.0, -20.0); // no pauses: two hard cuts, then the flush
        let mut changed = 0;
        for p in pcm.chunks(RATE / 4) {
            changed += s.push(Source::System, p, None) as usize;
        }
        assert_eq!(changed, 2, "two chunks transcribed while the meeting ran");
        s.finish(None, None).unwrap();
        let starts: Vec<f32> = s.system.words.iter().map(|(w, _)| w.start).collect();
        assert_eq!(starts.len(), 61, "every second heard exactly once: {starts:?}");
        assert!(starts.windows(2).all(|p| p[1] > p[0]));
        assert!(s.system.words.iter().all(|(_, who)| who == OTHERS), "no speaker models: Others");
    }

    #[test]
    fn a_failed_chunk_costs_its_words_only_and_says_so() {
        let mut calls = 0;
        let flaky: Transcribe = Box::new(move |_: &[f32]| {
            calls += 1;
            if calls == 1 { Err("Speech-to-text failed (429): slow down".into()) } else { Ok(vec![w(0.2, 0.8, "later")]) }
        });
        let mut s = Session::new(Meeting::default(), None, flaky, None).unwrap();
        for p in tone(55.0, -20.0).chunks(RATE) {
            s.push(Source::Mic, p, None);
        }
        s.finish(None, None).unwrap();
        let m = lock(&s.meeting).clone();
        assert_eq!(m.notices, ["00:00–00:24 was not transcribed (the audio is saved): Speech-to-text failed (429): slow down"]);
        assert_eq!(s.mic.words.len(), 2, "the chunks after the failure were transcribed");
        assert!(s.mic.pending.is_empty());
    }

    #[test]
    fn a_late_or_paused_stream_is_padded_onto_the_meeting_clock() {
        let none: Transcribe = Box::new(|_| Ok(vec![]));
        let mut s = Session::new(Meeting::default(), None, none, None).unwrap();
        s.push(Source::System, &[0.1; RATE], Some(Duration::from_secs(3)));
        assert_eq!(s.system.total, 3 * RATE, "started 2 s late");
        s.push(Source::System, &[0.1; RATE / 4], Some(Duration::from_millis(3400)));
        assert_eq!(s.system.total, 3 * RATE + RATE / 4, "small jitter is not padded");
    }

    #[test]
    fn two_streams_merge_into_one_timeline_and_echo_is_dropped() {
        let mic = vec![
            (w(0.0, 0.5, "Morning"), ME.to_string()),
            (w(0.6, 1.0, "all."), ME.to_string()),
            // The call playing through the speakers, heard by the mic too.
            (w(5.0, 5.4, "let's"), ME.to_string()),
            (w(5.5, 5.8, "ship"), ME.to_string()),
            (w(5.9, 6.3, "Friday."), ME.to_string()),
            (w(9.0, 9.5, "Agreed."), ME.to_string()),
        ];
        let sys = vec![
            (w(2.0, 2.4, "Hi"), "s1".to_string()),
            (w(5.0, 5.4, "Let's"), "s2".to_string()),
            (w(5.5, 5.8, "ship"), "s2".to_string()),
            (w(5.9, 6.3, "Friday."), "s2".to_string()),
            (w(6.4, 6.8, "OK?"), "s1".to_string()),
        ];
        let t = timeline(&mic, &sys);
        let got: Vec<(&str, &str)> = t.iter().map(|s| (s.speaker.as_str(), s.text.as_str())).collect();
        assert_eq!(got, [("you", "Morning all."), ("s1", "Hi"), ("s2", "Let's ship Friday."), ("s1", "OK?"), ("you", "Agreed.")]);
        // Sentences join into a line, split on a long pause, and never break inside: a word whose
        // time soaked up a pause stays in its sentence.
        let me = |v: &[(f32, f32, &str)]| v.iter().map(|&(a, b, t)| (w(a, b, t), ME.to_string())).collect::<Vec<_>>();
        let t = timeline(&me(&[(0.0, 1.0, "One."), (1.5, 2.0, "Two."), (4.0, 5.0, "Three"), (9.0, 9.5, "four.")]), &[]);
        let got: Vec<&str> = t.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(got, ["One. Two.", "Three four."]);
        // A long stretch of the mic does not swallow what the call said in between.
        let t = timeline(&me(&[(0.0, 1.0, "Yes."), (1.2, 2.0, "Sure."), (2.2, 3.0, "Okay.")]), &[(w(1.1, 1.9, "Ready?"), "s1".to_string())]);
        let got: Vec<&str> = t.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(got, ["Yes.", "Ready?", "Sure. Okay."]);
        // A whisper loop is dropped; real repetition is kept.
        let t = timeline(&me(&[(0.0, 4.0, "I'm going to go to the top of the top of the top of the top."), (5.0, 6.0, "Works, works, works.")]), &[]);
        assert_eq!(t.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["Works, works, works."]);
    }

    #[test]
    fn words_go_to_the_turn_they_overlap_most_else_the_nearest() {
        let turns = [Turn { start: 0.0, end: 2.0, speaker: 0 }, Turn { start: 2.0, end: 5.0, speaker: 1 }];
        assert_eq!(turn_of(&turns, &w(1.5, 2.8, "x")), Some(1));
        assert_eq!(turn_of(&turns, &w(0.1, 0.5, "x")), Some(0));
        assert_eq!(turn_of(&turns, &w(7.0, 7.5, "x")), Some(1), "after the last turn: nearest");
        assert_eq!(turn_of(&[], &w(1.0, 2.0, "x")), None);
    }

    #[test]
    fn a_sentence_stays_with_one_speaker_despite_early_word_times() {
        let turns = [Turn { start: 0.0, end: 3.0, speaker: 0 }, Turn { start: 3.6, end: 8.0, speaker: 1 }];
        // "That's" is timed 0.6 s early, inside speaker 0's turn; the rest of its sentence is speaker 1's.
        let words = [w(0.5, 1.5, "Thanks."), w(1.6, 2.9, "Bye."), w(2.8, 3.3, "That's"), w(3.7, 4.2, "great"), w(4.3, 5.0, "news."), w(9.0, 9.5, "Later.")];
        assert_eq!(sentence_speakers(&words, &turns), Some(vec![0, 0, 1, 1, 1, 1]), "the last sentence has no overlap: nearest turn");
        // A one-word reply timed into the gap before its turn goes to that turn, not the one before.
        assert_eq!(sentence_speakers(&[w(3.05, 3.5, "Agreed.")], &turns), Some(vec![1]));
        assert_eq!(sentence_speakers(&words, &[]), None);
    }

    #[test]
    fn reclustering_renumbers_by_first_appearance_and_keeps_renames() {
        let names: BTreeMap<String, String> = [("you", "You"), ("s1", "Speaker 1"), ("s2", "Priya"), ("s3", "Speaker 3")].into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        // Live labels were noisy (s3 was really Priya); the final pass found two voices, 7 then 4.
        let clusters = [7, 7, 4, 4, 4, 7, 4];
        let live: Vec<String> = ["s1", "s1", "s2", "s3", "s2", "s1", "s2"].map(String::from).to_vec();
        let (ids, out) = relabel(&clusters, &live, &names);
        assert_eq!(ids, ["s1", "s1", "s2", "s2", "s2", "s1", "s2"]);
        assert_eq!(out.get("s2").map(String::as_str), Some("Priya"), "the rename follows the voice");
        assert_eq!(out.get("s1").map(String::as_str), Some("Speaker 1"));
        assert_eq!(out.get("you").map(String::as_str), Some("You"));
        assert!(!out.contains_key("s3"));
        // Two clusters claiming one renamed label: the bigger one keeps the name.
        let (_, out) = relabel(&[1, 2, 2], &["s2".into(), "s2".into(), "s2".into()], &names);
        assert_eq!((out["s1"].as_str(), out["s2"].as_str()), ("Speaker 1", "Priya"));
    }

    fn sample() -> Meeting {
        let mut m = Meeting { title: "Launch sync".into(), duration_s: 754.0, ..Default::default() };
        m.speakers = [("you", "You"), ("s1", "Priya")].into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        m.segments = vec![
            Segment { start: 2.0, end: 4.0, speaker: "you".into(), text: "Shall we ship Friday?".into() },
            Segment { start: 65.0, end: 68.0, speaker: "s1".into(), text: "Yes, I'll send the deck.".into() },
        ];
        m
    }

    #[test]
    fn names_come_from_introductions_thanks_and_questions() {
        let seg = |sp: &str, t: &str| Segment { start: 0.0, end: 1.0, speaker: sp.into(), text: t.into() };
        let mut m = Meeting::default();
        m.segments = vec![
            seg("you", "Hi everyone, I'm Aditya."),
            seg("s1", "Hey, this is the plan. I'm Priya from design."),
            seg("s2", "Sounds good to me."),
            seg("you", "Thanks, Rahul. Okay, so we ship Friday."),
            seg("you", "Meera, can you send the deck?"),
            seg("s3", "Sure, I'm sorry it's late. I'm going to send it today."),
        ];
        let hints = name_hints(&m);
        assert_eq!(hints.get("s1").map(String::as_str), Some("Priya"), "an introduction names who said it");
        assert_eq!(hints.get("s2").map(String::as_str), Some("Rahul"), "thanks names the last other voice");
        assert_eq!(hints.get("s3").map(String::as_str), Some("Meera"), "a question by name goes to the next voice");
        assert!(!hints.contains_key("you"), "You is never renamed: {hints:?}");
        assert!(!hints.values().any(|n| n == "Sorry" || n == "Going"), "lowercase and filler words are not names");
    }

    #[test]
    fn exports_for_agents_cover_subtitles_and_json() {
        let m = sample();
        assert_eq!(srt(&m), "1\n00:00:02,000 --> 00:00:04,000\nYou: Shall we ship Friday?\n\n2\n00:01:05,000 --> 00:01:08,000\nPriya: Yes, I'll send the deck.\n");
        assert!(vtt(&m).starts_with("WEBVTT\n\n00:00:02.000 --> 00:00:04.000\n<v You>Shall we ship Friday?\n"));
        let v: serde_json::Value = serde_json::from_str(&json(&m)).unwrap();
        assert_eq!((v["segments"][1]["speaker"].as_str(), v["title"].as_str()), (Some("Priya"), Some("Launch sync")));
        for f in ["md", "txt", "json", "srt", "vtt"] {
            assert!(export(&m, f).is_ok(), "{f}");
        }
        assert!(export(&m, "pdf").is_err());
    }

    #[test]
    fn docx_is_a_zip_with_the_parts_word_needs_and_escapes_text() {
        let mut m = sample();
        m.title = "Q&A <draft> \"v2\" it's".into();
        m.summary = Some("## Decisions\n- Ship <Friday> & tell Priya".into());
        let (bytes, ext) = export(&m, "docx").unwrap();
        assert_eq!(ext, "docx");
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut names: Vec<&str> = z.file_names().collect();
        names.sort();
        assert_eq!(names, ["[Content_Types].xml", "_rels/.rels", "word/document.xml"]);
        let mut doc = String::new();
        std::io::Read::read_to_string(&mut z.by_name("word/document.xml").unwrap(), &mut doc).unwrap();
        assert!(doc.contains("Q&amp;A &lt;draft&gt; &quot;v2&quot; it&apos;s"), "{doc}");
        assert!(doc.contains(">Decisions<") && doc.contains("- Ship &lt;Friday&gt; &amp; tell Priya"));
        assert!(doc.contains(">Priya<") && doc.contains("Yes, I&apos;ll send the deck."));
        assert!(!doc.contains("<draft>") && !doc.contains("<Friday>"));
    }

    #[test]
    fn html_escapes_and_keeps_speakers() {
        let mut m = sample();
        m.segments[0].text = "<script>alert(1)</script>".into();
        let page = html(&m);
        assert!(page.starts_with("<!doctype html>") && page.contains("<h1>Launch sync</h1>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;") && !page.contains("<script>"));
        assert!(page.contains("<b>Priya</b><time>01:05</time><br>Yes, I&apos;ll send the deck."));
    }

    #[test]
    fn a_transcribed_file_exports_as_sentences_without_speakers() {
        let m = from_text("voice note", "Send the deck. Call Priya? Done", 6.0);
        assert_eq!(m.segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["Send the deck.", "Call Priya?", "Done"]);
        assert_eq!(srt(&m), "1\n00:00:00,000 --> 00:00:03,000\nSend the deck.\n\n2\n00:00:03,000 --> 00:00:05,000\nCall Priya?\n\n3\n00:00:05,000 --> 00:00:06,000\nDone\n");
        assert!(vtt(&m).contains("00:00:00.000 --> 00:00:03.000\nSend the deck.\n"), "no voice tag without a speaker");
        assert_eq!(plain(&m), "voice note\n\nSend the deck.\nCall Priya?\nDone\n");
        assert!(markdown(&m).ends_with("## Transcript\n\nSend the deck.\n\nCall Priya?\n\nDone\n"));
        assert!(from_text("silence", "", 3.0).segments.is_empty());
    }

    #[test]
    fn a_rename_reaches_every_line_and_the_summary_but_never_rewrites_you() {
        let mut m = sample();
        m.speakers.insert("s2".into(), "Speaker 2".into());
        m.summary = Some("- Speaker 2: send the deck. Speaker 20 agreed. You agreed.".into());
        rename(&mut m, "s2", "  Arjun ").unwrap();
        assert_eq!(m.speakers["s2"], "Arjun");
        assert_eq!(m.summary.as_deref(), Some("- Arjun: send the deck. Speaker 20 agreed. You agreed."), "whole names only");
        rename(&mut m, "you", "Aditya").unwrap();
        assert!(plain(&m).contains("Aditya: Shall we ship Friday?"));
        assert_eq!(m.summary.as_deref(), Some("- Arjun: send the deck. Speaker 20 agreed. You agreed."));
        assert!(rename(&mut m, "s9", "X").is_err() && rename(&mut m, "s1", " ").is_err());
    }

    #[test]
    fn markdown_and_text_exports_use_the_names() {
        let mut m = sample();
        assert_eq!(markdown(&m), "# Launch sync\n\n12:34\n\n## Transcript\n\n**You** (00:02): Shall we ship Friday?\n\n**Priya** (01:05): Yes, I'll send the deck.\n");
        m.summary = Some("## Summary\nShip on Friday.".into());
        assert!(markdown(&m).contains("12:34\n\n## Summary\nShip on Friday.\n\n## Transcript"));
        assert_eq!(plain(&m), "Launch sync\n\n[00:02] You: Shall we ship Friday?\n[01:05] Priya: Yes, I'll send the deck.\n");
        assert_eq!(clock(3725.0), "1:02:05");
    }

    #[test]
    fn summary_never_asks_the_model_about_an_empty_meeting() {
        let mut m = sample();
        let seen = std::cell::RefCell::new(String::new());
        let out = summarize(&m, "", |text| {
            *seen.borrow_mut() = text.to_string();
            Ok(" ## Summary\nShip Friday. ".into())
        });
        assert_eq!(out.unwrap().as_deref(), Some("## Summary\nShip Friday."));
        assert_eq!(seen.into_inner(), "You: Shall we ship Friday?\nPriya: Yes, I'll send the deck.");
        assert_eq!(for_notes(&m, "Aditya"), "Aditya: Shall we ship Friday?\nPriya: Yes, I'll send the deck.", "the notes use the user's name");
        let mut long = sample();
        long.segments = vec![Segment { start: 0.0, end: 1.0, speaker: "you".into(), text: "word ".repeat(9_000) }; 3];
        summarize(&long, "", |text| {
            assert!(text.len() <= SUMMARY_MAX_CHARS && text.ends_with("word "), "cut at a line: {}", text.len());
            Ok(String::new())
        })
        .unwrap();
        for empty in [vec![], vec![Segment { start: 0.0, end: 1.0, speaker: "you".into(), text: " ... ".into() }]] {
            m.segments = empty;
            assert_eq!(summarize(&m, "", |_| panic!("asked the model about nothing")).unwrap(), None);
        }
    }

    #[test]
    fn meeting_ids_cannot_leave_the_meetings_folder() {
        for bad in ["", "../x", "a/b", "..", "x.json", &"a".repeat(81)] {
            assert!(dir_of(bad).is_err(), "{bad}");
        }
        assert_eq!(stamp(1_700_000_000_000), "2023-11-14-221320");
        assert_eq!(stamp(0), "1970-01-01-000000");
        assert_eq!(slug("Launch sync: Q4 / pricing!"), "launch-sync-q4-pricing");
        assert_eq!(default_name("s12"), "Speaker 12");
    }

    #[test]
    fn old_audio_goes_only_when_the_meeting_ended_long_enough_ago() {
        const DAY: u64 = 86_400_000;
        let now = 1000 * DAY;
        let m = |state: &str, started_days_ago: u64, hours: f32| Meeting { state: state.into(), started_ms: now - started_days_ago * DAY, duration_s: hours * 3600.0, ..Default::default() };
        assert!(audio_expired(&m("done", 31, 1.0), 30, now));
        assert!(audio_expired(&m("interrupted", 8, 1.0), 7, now), "the app quit mid-meeting: it still ended");
        assert!(!audio_expired(&m("done", 29, 1.0), 30, now));
        assert!(!audio_expired(&m("done", 31, 48.0), 30, now), "counted from the end, not the start");
        assert!(!audio_expired(&m("done", 400, 1.0), 0, now), "0 keeps audio forever");
        for busy in ["recording", "processing"] {
            assert!(!audio_expired(&m(busy, 400, 1.0), 7, now), "{busy} is never touched");
        }
    }

    #[test]
    fn search_needs_every_word_in_any_case_and_shows_the_best_lines() {
        let q = |m: &Meeting, q: &str| search_meeting(m, &q.split_whitespace().map(fold).collect::<Vec<_>>());
        let texts = |r: Option<Vec<Snippet>>| r.map(|v| v.into_iter().map(|s| (s.start, s.text)).collect::<Vec<_>>());
        let mut m = sample();
        m.summary = Some("## Decisions\n- Ship on Friday.".into());
        assert_eq!(texts(q(&m, "FRIDAY ship")), Some(vec![(2.0, "Shall we ship Friday?".into())]));
        assert_eq!(texts(q(&m, "deck yes")), Some(vec![(65.0, "Yes, I'll send the deck.".into())]));
        assert_eq!(q(&m, "deck tuesday"), None, "every word must match");
        assert_eq!(q(&m, "   "), None);
        assert_eq!(q(&m, "launch decisions"), Some(vec![]), "title and summary count; no line to show");
        // Three lines at most, those with the most words first, shown in time order.
        m.segments = (0..6).map(|i| Segment { start: i as f32, end: i as f32 + 1.0, speaker: "you".into(), text: if i % 2 == 0 { "ship it" } else { "ship it friday" }.into() }).collect();
        assert_eq!(q(&m, "ship friday").unwrap().iter().map(|s| s.start).collect::<Vec<_>>(), vec![1.0, 3.0, 5.0]);
        // A long line is cut around the match, at char boundaries.
        let long = format!("{} Größe ÉCOLE budget {}", "é".repeat(300), "ü".repeat(300));
        m.segments = vec![Segment { start: 9.0, end: 10.0, speaker: "s1".into(), text: long }];
        let s = &q(&m, "école BUDGET größe").unwrap()[0];
        assert!(s.text.starts_with('…') && s.text.ends_with('…') && s.text.contains("Größe ÉCOLE budget"), "{}", s.text);
        assert!(s.text.chars().count() <= SNIPPET_CHARS + 2);
    }

    #[test]
    fn remote_text_is_spread_over_the_chunk() {
        let ws = spread("one two three four", 8.0);
        assert_eq!((ws[1].start, ws[3].end), (2.0, 8.0));
        assert!(spread("", 5.0).is_empty());
    }

    #[test]
    fn rolling_wav_is_valid_after_every_append() {
        let dir = std::env::temp_dir().join(format!("echotype-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.wav");
        let mut wav = Wav::create(&p).unwrap();
        wav.append(&[0.5; 800]).unwrap();
        assert_eq!(audio::decode(std::fs::read(&p).unwrap(), Some("wav")).unwrap().len(), 800);
        wav.append(&[-0.25; 1600]).unwrap();
        let back = read_wav(&p);
        assert_eq!(back.len(), 2400);
        assert!((back[2000] + 0.25).abs() < 1e-3);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
