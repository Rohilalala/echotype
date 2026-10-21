//! Live capture for meeting notes: the microphone and the computer's own audio (macOS:
//! ScreenCaptureKit, Echotype's own sounds excluded; Windows: WASAPI loopback; Linux: none yet).
//! Each arrives as 16 kHz mono pieces tagged with the time since the meeting started, so the
//! two line up on one timeline.

use crate::audio::{self, StreamResampler};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The microphone: "You".
    Mic,
    /// Everything the computer plays: the other people in the call.
    System,
}

/// (where, 16 kHz mono samples, time since the meeting started when they arrived)
pub type Piece = (Source, Vec<f32>, Duration);

pub struct Capture {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// Notices for the meeting while it runs: computer audio stopped and came back, say.
    pub notes: Arc<Mutex<Vec<String>>>,
}

impl Capture {
    /// Opens the mic and, with `system`, the computer's audio. A system-audio failure is
    /// returned as a notice (the meeting continues mic-only); a mic failure is an error.
    pub fn start(device: Option<String>, system: bool, started: Instant, tx: Sender<Piece>) -> Result<(Self, Option<String>), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let notes = Arc::new(Mutex::new(vec![]));
        let mut threads = vec![mic(device, started, tx.clone(), stop.clone())?];
        let mut notice = None;
        if system {
            match system_audio(started, tx, stop.clone(), notes.clone()) {
                Ok(t) => threads.push(t),
                Err(e) => notice = Some(e),
            }
        }
        Ok((Self { stop, threads, notes }, notice))
    }

    /// Stops both streams; every captured sample has been sent when this returns.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Dropped without `stop` (the worker died): the threads still stop, unjoined.
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn mic(device: Option<String>, started: Instant, tx: Sender<Piece>, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>, String> {
    pump(Source::Mic, move |buf, lost| audio::open_stream(device, buf, lost).map(|(s, r, _)| (s, r)), started, tx, stop)
}

/// Runs one cpal input stream on its own thread, sending 16 kHz mono pieces tagged `src`.
fn pump(src: Source, open: impl FnOnce(Arc<Mutex<Vec<f32>>>, Arc<AtomicBool>) -> Result<(cpal::Stream, u32), String> + Send + 'static, started: Instant, tx: Sender<Piece>, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>, String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let t = std::thread::spawn(move || {
        let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
        let lost = Arc::new(AtomicBool::new(false));
        // The cpal stream is !Send on macOS, so it lives and dies on this thread.
        let (stream, rate) = match open(buf.clone(), lost) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let _ = ready_tx.send(Ok(()));
        let mut rs = StreamResampler::new(rate, audio::RATE);
        let mut stream = Some(stream);
        loop {
            let done = stop.load(Ordering::SeqCst);
            if done {
                stream.take(); // stop the callbacks, then read what they left
            }
            let raw = std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()));
            let mut out = rs.push(&raw);
            if done {
                out.extend(rs.finish());
            }
            if !out.is_empty() && tx.send((src, out, started.elapsed())).is_err() {
                return;
            }
            if done {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(t),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("The audio thread stopped unexpectedly".into()),
    }
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Screen Recording permission, which macOS requires for capturing other apps' audio. Other
/// systems have no such permission, so nothing is blocked there.
pub fn screen_recording_allowed() -> bool {
    #[cfg(target_os = "macos")]
    // SAFETY: no arguments, returns a BOOL.
    return unsafe { CGPreflightScreenCaptureAccess() };
    #[cfg(not(target_os = "macos"))]
    true
}

/// Shows macOS's one-time Screen Recording prompt (and lists Echotype in the pane).
pub fn request_screen_recording() {
    #[cfg(target_os = "macos")]
    // SAFETY: as above.
    unsafe {
        CGRequestScreenCaptureAccess();
    }
}

pub const NO_SCREEN_RECORDING: &str = "Only your microphone is being transcribed. To include the other people in the call, allow Echotype under Screen Recording in System Settings, then start a new meeting.";

/// Windows: WASAPI loopback of the default output. Unlike macOS it includes Echotype's own sounds.
#[cfg(target_os = "windows")]
fn system_audio(started: Instant, tx: Sender<Piece>, stop: Arc<AtomicBool>, _: Arc<Mutex<Vec<String>>>) -> Result<JoinHandle<()>, String> {
    pump(Source::System, audio::open_loopback, started, tx, stop).map_err(|e| format!("Only your microphone is being transcribed: {e}"))
}

// ponytail: Linux meetings are mic-only. Capturing the PulseAudio/PipeWire monitor source needs a
// Pulse or PipeWire client (cpal's ALSA host cannot pick the monitor for one stream only).
#[cfg(target_os = "linux")]
fn system_audio(_: Instant, _: Sender<Piece>, _: Arc<AtomicBool>, _: Arc<Mutex<Vec<String>>>) -> Result<JoinHandle<()>, String> {
    Err("Only your microphone is being transcribed: computer audio is not captured on Linux yet.".into())
}

/// The computer's audio on macOS. When macOS stops the stream mid-meeting (it did 35 s into a
/// Teams call, with nobody hearing about it), the stream is opened again every 2 s until it
/// comes back, and the meeting gets a notice saying when and why.
#[cfg(target_os = "macos")]
fn system_audio(started: Instant, tx: Sender<Piece>, stop: Arc<AtomicBool>, notes: Arc<Mutex<Vec<String>>>) -> Result<JoinHandle<()>, String> {
    if !screen_recording_allowed() {
        request_screen_recording();
        return Err(NO_SCREEN_RECORDING.into());
    }
    let first = sc_stream(started, &tx)?;
    let at = move |t: Instant| crate::meeting::clock(t.duration_since(started).as_secs_f32());
    Ok(std::thread::spawn(move || {
        let mut live = Some(first);
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
            let why = live.as_ref().and_then(|(_, died)| died.lock().unwrap_or_else(|e| e.into_inner()).take());
            let Some(why) = why else { continue };
            let lost = Instant::now();
            live = None;
            while live.is_none() && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(2));
                live = sc_stream(started, &tx).ok();
            }
            let back = if live.is_some() { format!("came back at {}", at(Instant::now())) } else { "did not come back: only your microphone was transcribed after that".into() };
            notes.lock().unwrap_or_else(|e| e.into_inner()).push(format!("Computer audio stopped at {} ({why}) and {back}.", at(lost)));
        }
        if let Some((stream, _)) = live {
            let _ = stream.stop_capture();
        }
        drop(tx);
    }))
}

/// One audio-only ScreenCaptureKit stream of the main display's output mix, Echotype excluded,
/// and where its stop reason lands if macOS ends it.
#[cfg(target_os = "macos")]
fn sc_stream(started: Instant, tx: &Sender<Piece>) -> Result<(screencapturekit::prelude::SCStream, Arc<Mutex<Option<String>>>), String> {
    use screencapturekit::prelude::*;
    use screencapturekit::stream::ErrorHandler;
    let fail = |e: SCError| format!("Only your microphone is being transcribed: computer audio could not start ({e}).");
    let content = SCShareableContent::get().map_err(fail)?;
    let display = content.displays().into_iter().next().ok_or("Only your microphone is being transcribed: no display to capture audio from.")?;
    let filter = SCContentFilter::create().with_display(&display).with_excluding_windows(&[]).build().map_err(fail)?;
    // Video can't be switched off, so ask for the least of it: 2x2 pixels once a second.
    let config = SCStreamConfiguration::new()
        .with_width(2)
        .with_height(2)
        .with_minimum_frame_interval(&CMTime::new(1, 1))
        .with_captures_audio(true)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(audio::RATE as i32)
        .with_channel_count(1);
    let died = Arc::new(Mutex::new(None));
    let d = died.clone();
    let mut stream = SCStream::new_with_delegate(&filter, &config, ErrorHandler::new(move |e| *d.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string()))).map_err(fail)?;
    let rs: Mutex<Option<(u32, StreamResampler)>> = Mutex::new(None);
    let audio_tx = tx.clone();
    stream
        .add_output_handler(
            move |buf: CMSampleBuffer, _| {
                let Some((pcm, rate)) = mono_f32(&buf) else { return };
                let mut g = rs.lock().unwrap_or_else(|e| e.into_inner());
                if g.as_ref().is_none_or(|(r, _)| *r != rate) {
                    *g = Some((rate, StreamResampler::new(rate, audio::RATE)));
                }
                let out = g.as_mut().expect("set above").1.push(&pcm);
                if !out.is_empty() {
                    let _ = audio_tx.send((Source::System, out, started.elapsed()));
                }
            },
            SCStreamOutputType::Audio,
        )
        .map_err(fail)?;
    // Without a screen output ScreenCaptureKit logs every dropped frame.
    stream.add_output_handler(|_: CMSampleBuffer, _| {}, SCStreamOutputType::Screen).map_err(fail)?;
    stream.start_capture().map_err(fail)?;
    Ok((stream, died))
}

/// Float or 16-bit PCM, interleaved or one buffer per channel, averaged to mono.
#[cfg(target_os = "macos")]
fn mono_f32(buf: &screencapturekit::prelude::CMSampleBuffer) -> Option<(Vec<f32>, u32)> {
    let fmt = buf.format_description()?;
    let rate = fmt.audio_sample_rate()? as u32;
    let float = fmt.audio_format_flags().is_some_and(|f| f & 1 != 0); // kAudioFormatFlagIsFloat
    let bits = fmt.audio_bits_per_channel().unwrap_or(32);
    let list = buf.audio_buffer_list().ok()?;
    let mut chans: Vec<Vec<f32>> = vec![];
    for b in &list {
        let bytes = b.data();
        let samples: Vec<f32> = match (float, bits) {
            (true, 32) => bytes.chunks_exact(4).map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect(),
            (false, 16) => bytes.chunks_exact(2).map(|c| i16::from_ne_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
            _ => return None,
        };
        chans.push(audio::to_mono(&samples, b.number_channels.max(1) as usize));
    }
    let n = chans.iter().map(Vec::len).min()?;
    let k = chans.len() as f32;
    Some(((0..n).map(|i| chans.iter().map(|c| c[i]).sum::<f32>() / k).collect(), rate))
}
