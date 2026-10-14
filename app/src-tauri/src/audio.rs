//! Microphone capture, audio-file decoding, resampling and WAV encoding.
//! Everything leaves this module as 16 kHz mono f32, which is what whisper wants.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

pub const RATE: u32 = 16_000;
/// Refuse to transcribe more than this (a 25 MB Opus file can hold many hours).
const MAX_SECONDS: usize = 60 * 60;

pub fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved.chunks(channels).map(|f| f.iter().sum::<f32>() / f.len() as f32).collect()
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) }
}

/// Band-limited (Hann-windowed sinc) resampler. Low-passes at the lower Nyquist so
/// 48 kHz mic audio does not alias into the speech band when brought down to 16 kHz.
// ponytail: direct convolution, O(n * taps) ≈ 25M MACs for 15 s at 48 kHz (~20 ms); switch to rubato's polyphase FFT if hour-long files get slow.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() || from == 0 {
        return x.to_vec();
    }
    let mut r = StreamResampler::new(from, to);
    let mut out = r.push(x);
    out.extend(r.finish());
    out
}

/// `resample` fed in pieces (meeting capture): an output sample is produced once the input on
/// both sides of it has arrived, so pieces join without seams. `push`s then `finish` give exactly
/// what `resample` gives for the whole input.
pub struct StreamResampler {
    ratio: f64,
    cutoff: f64,
    half: i64,
    /// Input not needed any more is dropped; `buf[0]` is input sample `base`.
    buf: Vec<f32>,
    base: usize,
    total: usize,
    next: usize,
}

impl StreamResampler {
    pub fn new(from: u32, to: u32) -> Self {
        let ratio = to as f64 / from.max(1) as f64;
        let cutoff = ratio.min(1.0) * 0.94; // fraction of the input Nyquist, small guard band
        Self { ratio, cutoff, half: (16.0 / cutoff).ceil() as i64, buf: vec![], base: 0, total: 0, next: 0 }
    }

    pub fn push(&mut self, x: &[f32]) -> Vec<f32> {
        if self.ratio == 1.0 {
            return x.to_vec();
        }
        self.buf.extend_from_slice(x);
        self.total += x.len();
        self.drain(false)
    }

    pub fn finish(&mut self) -> Vec<f32> {
        if self.ratio == 1.0 { vec![] } else { self.drain(true) }
    }

    fn drain(&mut self, last: bool) -> Vec<f32> {
        let (half, n) = (self.half, self.total as i64);
        let n_out = (self.total as f64 * self.ratio).floor() as usize;
        let mut out = vec![];
        while self.next < n_out {
            let t = self.next as f64 / self.ratio;
            let center = t.floor() as i64;
            if !last && center + half > n - 1 {
                break; // wait for the input to its right
            }
            let lo = (center - half + 1).max(0);
            let hi = (center + half).min(n - 1);
            let mut acc = 0.0;
            for k in lo..=hi {
                let d = t - k as f64;
                let w = 0.5 * (1.0 + (std::f64::consts::PI * d / half as f64).cos());
                acc += self.buf[k as usize - self.base] as f64 * self.cutoff * sinc(self.cutoff * d) * w;
            }
            out.push(acc as f32);
            self.next += 1;
        }
        let keep = ((self.next as f64 / self.ratio).floor() as i64 - half + 1).max(0) as usize;
        if keep > self.base {
            self.buf.drain(..keep - self.base);
            self.base = keep;
        }
        out
    }
}

/// 16-bit PCM mono WAV at 16 kHz, for remote STT uploads.
pub fn wav_bytes(pcm: &[f32]) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&RATE.to_le_bytes());
    v.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    v.extend_from_slice(&2u16.to_le_bytes()); // block align
    v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    v
}

// ---------- decoding (wav, mp3, m4a/aac, flac, ogg vorbis, ogg/webm opus) ----------

fn codecs() -> &'static symphonia::core::codecs::registry::CodecRegistry {
    static REG: std::sync::LazyLock<symphonia::core::codecs::registry::CodecRegistry> = std::sync::LazyLock::new(|| {
        let mut r = symphonia::core::codecs::registry::CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut r);
        // Symphonia has no Opus codec; WhatsApp voice notes are Ogg Opus.
        r.register_audio_decoder::<symphonia_adapter_libopus::OpusDecoder>();
        r
    });
    &REG
}

/// Decodes any supported audio file to 16 kHz mono. `ext` is a hint ("opus", "m4a"...).
pub fn decode(bytes: Vec<u8>, ext: Option<&str>) -> Result<Vec<f32>, String> {
    use symphonia::core::errors::Error;
    use symphonia::core::formats::TrackType;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;

    let mut hint = Hint::new();
    if let Some(e) = ext {
        hint.with_extension(&e.to_ascii_lowercase());
    }
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut reader = symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .map_err(|e| format!("Unsupported or unreadable audio file: {e}"))?;
    let track = reader.default_track(TrackType::Audio).ok_or("The file has no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("The file has no audio track")?.clone();
    let mut decoder = codecs()
        .make_audio_decoder(&params, &Default::default())
        .map_err(|e| format!("Unsupported audio codec: {e}"))?;

    let mut rate = params.sample_rate.unwrap_or(RATE);
    let mut mono = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("Could not read the audio file: {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                rate = buf.spec().rate();
                let ch = buf.spec().channels().count().max(1);
                buf.copy_to_vec_interleaved(&mut tmp);
                mono.extend(to_mono(&tmp, ch));
                if mono.len() > MAX_SECONDS * rate as usize {
                    return Err("Audio is longer than 60 minutes".into());
                }
            }
            Err(Error::DecodeError(_)) => continue, // skip a corrupt packet, keep the rest
            Err(e) => return Err(format!("Could not decode the audio: {e}")),
        }
    }
    if mono.is_empty() {
        return Err("The file contains no audio".into());
    }
    Ok(resample(&mono, rate, RATE))
}

// ---------- microphone ----------

#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct InputDevice {
    pub name: String,
    /// AirPods and other Bluetooth mics switch to a narrowband codec that hurts quiet speech.
    pub bluetooth: bool,
    /// The system default input, what "System default" records from.
    pub default: bool,
}

pub fn input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.name().ok());
    #[cfg(target_os = "macos")]
    let transports = mac::bluetooth_names();
    let names: Vec<String> = host.input_devices().map(|ds| ds.filter_map(|d| d.name().ok()).collect()).unwrap_or_default();
    names
        .into_iter()
        .map(|name| {
            #[cfg(target_os = "macos")]
            let bluetooth = transports.as_ref().map_or_else(|| bluetooth_by_name(&name), |t| t.contains(&name));
            #[cfg(not(target_os = "macos"))]
            let bluetooth = bluetooth_by_name(&name);
            InputDevice { default: default.as_deref() == Some(name.as_str()), name, bluetooth }
        })
        .collect()
}

/// Fallback when the OS can't say how a device is connected.
fn bluetooth_by_name(name: &str) -> bool {
    let n = name.to_lowercase();
    ["airpods", "bluetooth", "beats", "buds"].iter().any(|w| n.contains(w))
}

#[cfg(target_os = "macos")]
mod mac {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(id: u32, addr: *const Address, qsize: u32, q: *const c_void, size: *mut u32) -> i32;
        fn AudioObjectGetPropertyData(id: u32, addr: *const Address, qsize: u32, q: *const c_void, size: *mut u32, data: *mut c_void) -> i32;
    }

    const fn cc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SYSTEM: u32 = 1;
    const GLOBAL: u32 = cc(b"glob");

    fn addr(selector: &[u8; 4]) -> Address {
        Address { selector: cc(selector), scope: GLOBAL, element: 0 }
    }

    /// Names of every audio device whose CoreAudio transport is Bluetooth or Bluetooth LE.
    /// None when CoreAudio can't be read; callers fall back to the device name.
    pub fn bluetooth_names() -> Option<Vec<String>> {
        // SAFETY: each call gets a valid address and an out-buffer of exactly the size it reports or expects.
        unsafe {
            let devs = addr(b"dev#");
            let mut size = 0u32;
            if AudioObjectGetPropertyDataSize(SYSTEM, &devs, 0, std::ptr::null(), &mut size) != 0 {
                return None;
            }
            let mut ids = vec![0u32; size as usize / 4];
            if AudioObjectGetPropertyData(SYSTEM, &devs, 0, std::ptr::null(), &mut size, ids.as_mut_ptr().cast()) != 0 {
                return None;
            }
            let mut out = vec![];
            for id in ids {
                let (mut transport, mut tsize) = (0u32, 4u32);
                if AudioObjectGetPropertyData(id, &addr(b"tran"), 0, std::ptr::null(), &mut tsize, (&mut transport as *mut u32).cast()) != 0 {
                    continue;
                }
                if transport != cc(b"blue") && transport != cc(b"blea") {
                    continue;
                }
                let (mut name, mut nsize): (CFStringRef, u32) = (std::ptr::null(), std::mem::size_of::<CFStringRef>() as u32);
                if AudioObjectGetPropertyData(id, &addr(b"lnam"), 0, std::ptr::null(), &mut nsize, (&mut name as *mut CFStringRef).cast()) == 0 && !name.is_null() {
                    out.push(CFString::wrap_under_create_rule(name).to_string());
                }
            }
            Some(out)
        }
    }
}

// ---------- level statistics ----------

/// Windows are the recorder callback's 50 ms; 20 make a second.
const WINDOWS_PER_SEC: usize = 20;
/// Below this a window counts as silence for the muted-mic check.
pub const MUTED_DB: f32 = -70.0;
const FLOOR_MIN_DB: f32 = -90.0;
/// The meter spans this many dB above the running floor.
const METER_RANGE_DB: f32 = 30.0;

/// dBFS of a window, not clamped (digital zero reads -120).
pub fn dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    20.0 * rms.max(1e-6).log10()
}

/// What the overlay meter gets each window.
#[derive(serde::Serialize, Debug, Clone, Copy, Default, PartialEq)]
pub struct Level {
    /// 0..1 above the room's noise floor, so whispers visibly move it.
    pub meter: f32,
    /// Every window so far (at least 2 s) was below -70 dBFS: "Is your mic muted?".
    pub muted: bool,
}

#[derive(serde::Serialize, Debug, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Spoken softly but clearly above the room: "Quiet voice".
    Quiet,
    /// Barely above the room: "Too quiet, move the mic closer".
    TooQuiet,
}

/// Live meter and per-clip stats over 50 ms windows, all in dBFS.
#[derive(Default)]
pub struct LevelStats {
    recent: std::collections::VecDeque<f32>,
    clip: Vec<f32>,
    windows: usize,
    skip: u32,
    heard: bool,
    muted_seen: bool,
    level: Level,
}

impl LevelStats {
    /// Ignore the next `n` windows: Echotype's own start/lock sound must not count as the room or as speech.
    pub fn exclude(&mut self, n: u32) {
        self.skip = self.skip.max(n);
    }

    pub fn push(&mut self, db: f32) -> Level {
        self.windows += 1;
        if self.skip > 0 {
            self.skip -= 1;
            return self.level;
        }
        // Running floor: the quietest window of the last 3 s, never below -90 dBFS.
        self.recent.push_back(db);
        if self.recent.len() > 3 * WINDOWS_PER_SEC {
            self.recent.pop_front();
        }
        let floor = self.recent.iter().copied().fold(f32::INFINITY, f32::min).max(FLOOR_MIN_DB);
        self.clip.push(db);
        if db >= MUTED_DB {
            self.heard = true;
        }
        let muted = !self.heard && self.windows >= 2 * WINDOWS_PER_SEC;
        self.muted_seen |= muted;
        self.level = Level { meter: ((db - floor) / METER_RANGE_DB).clamp(0.0, 1.0), muted };
        self.level
    }

    /// The whole clip stayed under -70 dBFS: skip STT and say "No audio. Check your mic".
    pub fn silent(&self) -> bool {
        !self.heard && self.windows >= 2 * WINDOWS_PER_SEC
    }

    /// The muted notice showed at some point in this recording.
    pub fn muted_seen(&self) -> bool {
        self.muted_seen
    }

    /// (floor, speech level) of the clip: 10th and 95th percentile window, floor clamped at -90.
    pub fn floor_and_speech(&self) -> Option<(f32, f32)> {
        if self.clip.is_empty() {
            return None;
        }
        let mut v = self.clip.clone();
        v.sort_by(f32::total_cmp);
        let pct = |p: usize| v[(v.len() - 1) * p / 100];
        Some((pct(10).max(FLOOR_MIN_DB), pct(95)))
    }

    /// Label for clips of 1 s or longer. Thresholds measured on TTS whispers (whisper-mode.md):
    // ponytail: re-check -40/-50 dBFS and 12/10 dB SNR on owner-recorded whispers (ECHOTYPE_WHISPER_FIXTURES test).
    pub fn quality(&self) -> Option<Quality> {
        if self.windows < WINDOWS_PER_SEC {
            return None;
        }
        let (floor, speech) = self.floor_and_speech()?;
        let snr = speech - floor;
        if speech < -50.0 && snr < 10.0 {
            Some(Quality::TooQuiet)
        } else if speech < -40.0 && snr >= 12.0 {
            Some(Quality::Quiet)
        } else {
            None
        }
    }
}

/// 50 ms window dBFS of a whole clip, the same windows the recorder reports live.
#[cfg(test)]
pub fn windows_db(pcm: &[f32], rate: u32) -> Vec<f32> {
    pcm.chunks((rate as usize / WINDOWS_PER_SEC).max(1)).map(dbfs).collect()
}

/// A running capture. The cpal stream lives on its own thread (it is !Send on macOS).
pub struct Recorder {
    stop: mpsc::Sender<()>,
    handle: JoinHandle<(Vec<f32>, u32)>,
}

impl Recorder {
    /// Opens `device` (or the default input) and starts recording. Every 50 ms `on_window` gets the
    /// dBFS of the last 50 ms (unclamped) and whether the device has gone away (unplugged, Bluetooth dropped).
    pub fn start(device: Option<String>, mut on_window: impl FnMut(f32, bool) + Send + 'static) -> Result<Self, String> {
        let (stop, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let handle = std::thread::spawn(move || {
            let buf = Arc::new(Mutex::new(Vec::<f32>::with_capacity(48_000 * 30)));
            let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (stream, rate) = match open_stream(device, buf.clone(), lost.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return (vec![], RATE);
                }
            };
            let _ = ready_tx.send(Ok(()));
            let window = (rate as usize / WINDOWS_PER_SEC).max(1);
            while let Err(mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(Duration::from_millis(50)) {
                let db = {
                    let b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    dbfs(&b[b.len().saturating_sub(window)..])
                };
                on_window(db, lost.load(std::sync::atomic::Ordering::SeqCst));
            }
            drop(stream);
            let samples = std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()));
            (samples, rate)
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { stop, handle }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("The audio thread stopped unexpectedly".into()),
        }
    }

    /// Stops and returns the recording as 16 kHz mono.
    pub fn stop(self) -> Vec<f32> {
        let _ = self.stop.send(());
        let (samples, rate) = self.handle.join().unwrap_or_default();
        resample(&samples, rate, RATE)
    }
}

pub(crate) fn open_stream(device: Option<String>, buf: Arc<Mutex<Vec<f32>>>, lost: Arc<std::sync::atomic::AtomicBool>) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let dev = device
        .and_then(|name| host.input_devices().ok()?.find(|d| d.name().ok().as_deref() == Some(name.as_str())))
        .or_else(|| host.default_input_device())
        .ok_or("No microphone found")?;
    let cfg = dev.default_input_config().map_err(|e| format!("Microphone unavailable: {e}"))?;
    let rate = cfg.sample_rate().0;
    let sc: cpal::StreamConfig = cfg.clone().into();
    use cpal::SampleFormat as F;
    let stream = match cfg.sample_format() {
        F::F32 => build::<f32>(&dev, &sc, buf, lost),
        F::I16 => build::<i16>(&dev, &sc, buf, lost),
        F::U16 => build::<u16>(&dev, &sc, buf, lost),
        F::I32 => build::<i32>(&dev, &sc, buf, lost),
        F::I8 => build::<i8>(&dev, &sc, buf, lost),
        F::U8 => build::<u8>(&dev, &sc, buf, lost),
        F::F64 => build::<f64>(&dev, &sc, buf, lost),
        f => return Err(format!("Unsupported microphone sample format {f}")),
    }
    .map_err(|e| format!("Could not open the microphone: {e}"))?;
    stream.play().map_err(|e| format!("Could not start the microphone: {e}"))?;
    Ok((stream, rate))
}

fn build<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, buf: Arc<Mutex<Vec<f32>>>, lost: Arc<std::sync::atomic::AtomicBool>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let ch = cfg.channels.max(1) as usize;
    dev.build_input_stream(
        cfg,
        move |data: &[T], _| {
            let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
            for frame in data.chunks(ch) {
                b.push(frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / ch as f32);
            }
        },
        move |e| {
            eprintln!("echotype: audio stream error: {e}");
            if matches!(e, cpal::StreamError::DeviceNotAvailable) {
                lost.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize).map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        let mid = &x[x.len() / 10..x.len() * 9 / 10]; // skip edge transients
        (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt()
    }

    #[test]
    fn resample_keeps_speech_band_and_removes_aliases() {
        let pass = resample(&sine(440.0, 48_000, 1.0), 48_000, 16_000);
        assert_eq!(pass.len(), 16_000);
        assert!((rms(&pass) - 0.7071).abs() < 0.02, "440 Hz rms {}", rms(&pass));
        // 12 kHz cannot exist at 16 kHz; a naive decimator would fold it to 4 kHz.
        let stop = resample(&sine(12_000.0, 48_000, 1.0), 48_000, 16_000);
        assert!(rms(&stop) < 0.02, "12 kHz leaked: rms {}", rms(&stop));
        // Odd ratio and upsampling.
        assert_eq!(resample(&sine(440.0, 44_100, 1.0), 44_100, 16_000).len(), 16_000);
        let up = resample(&sine(440.0, 8_000, 1.0), 8_000, 16_000);
        assert!((rms(&up) - 0.7071).abs() < 0.02);
    }

    #[test]
    fn resampling_in_pieces_matches_resampling_the_whole() {
        let x = sine(440.0, 44_100, 1.3);
        let whole = resample(&x, 44_100, 16_000);
        let mut r = StreamResampler::new(44_100, 16_000);
        let mut pieces: Vec<f32> = x.chunks(1_237).flat_map(|c| r.push(c)).collect();
        pieces.extend(r.finish());
        assert_eq!(pieces.len(), whole.len());
        assert!(pieces.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 1e-6));
        assert_eq!(StreamResampler::new(16_000, 16_000).push(&[0.5, 0.25]), vec![0.5, 0.25]);
    }

    #[test]
    fn mono_mix_and_unclamped_dbfs() {
        assert_eq!(to_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert!(dbfs(&[1.0; 100]).abs() < 0.01);
        assert!((dbfs(&[0.031_622_8; 100]) + 30.0).abs() < 0.01);
        // The old meter clamped at -60; a -80 dBFS window must read about -80.
        assert!((dbfs(&noise(0.000_1 * 2f32.sqrt(), 800)) + 80.0).abs() < 1.5, "{}", dbfs(&noise(0.000_1 * 2f32.sqrt(), 800)));
        assert_eq!(dbfs(&[0.0; 800]), -120.0);
        assert_eq!(dbfs(&[]), -120.0);
    }

    /// Deterministic white-ish noise with peak `amp` (rms ≈ amp / √3).
    fn noise(amp: f32, n: usize) -> Vec<f32> {
        let mut x = 12345u32;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((x >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp * 3f32.sqrt() / 2f32.sqrt()
            })
            .collect()
    }

    fn at_db(db: f32) -> f32 {
        10f32.powf(db / 20.0) * 2f32.sqrt() // sine peak for that rms
    }

    fn stats(pcm: &[f32]) -> (LevelStats, Vec<Level>) {
        let mut st = LevelStats::default();
        let levels = windows_db(pcm, RATE).into_iter().map(|db| st.push(db)).collect();
        (st, levels)
    }

    /// `secs` of room noise at `room` dBFS with a 440 Hz "voice" at `voice` dBFS from 0.5 s to the end minus 0.5 s.
    fn clip(room: f32, voice: Option<f32>, secs: f32) -> Vec<f32> {
        let n = (RATE as f32 * secs) as usize;
        let mut pcm = noise(10f32.powf(room / 20.0) * 2f32.sqrt(), n);
        if let Some(v) = voice {
            let tone = sine(440.0, RATE, secs);
            for (i, s) in pcm.iter_mut().enumerate().take(n - RATE as usize / 2).skip(RATE as usize / 2) {
                *s += tone[i] * at_db(v) / 2f32.sqrt();
            }
        }
        pcm
    }

    #[test]
    fn quality_labels_quiet_voice_too_quiet_and_nothing() {
        assert_eq!(stats(&clip(-70.0, Some(-45.0), 3.0)).0.quality(), Some(Quality::Quiet), "whisper well above a quiet room");
        assert_eq!(stats(&clip(-58.0, Some(-54.0), 3.0)).0.quality(), Some(Quality::TooQuiet), "voice barely above the room");
        assert_eq!(stats(&clip(-60.0, Some(-20.0), 3.0)).0.quality(), None, "normal speech");
        assert_eq!(stats(&clip(-45.0, None, 3.0)).0.quality(), None, "room noise only");
        assert_eq!(stats(&clip(-70.0, Some(-45.0), 0.8)).0.quality(), None, "labels need 1 s");
    }

    #[test]
    fn meter_is_relative_to_the_room_and_the_floor_stops_at_minus_90() {
        let (_, levels) = stats(&clip(-70.0, Some(-45.0), 3.0));
        assert!(levels[5].meter < 0.2, "room alone {}", levels[5].meter);
        assert!(levels[30].meter > 0.6, "a whisper 25 dB over the room fills most of the meter: {}", levels[30].meter);
        let mut st = LevelStats::default();
        st.push(-120.0);
        assert!((st.push(-60.0).meter - 1.0).abs() < 1e-6, "floor clamps at -90, so -60 is 30 dB up");
    }

    #[test]
    fn muted_mic_is_flagged_after_two_seconds_and_cleared_by_sound() {
        for quiet in [vec![0.0f32; RATE as usize * 3], noise(at_db(-80.0), RATE as usize * 3)] {
            let (st, levels) = stats(&quiet);
            assert!(!levels[38].muted && levels[39].muted && levels.last().unwrap().muted);
            assert!(st.silent());
        }
        let mut pcm = vec![0.0f32; RATE as usize * 3];
        pcm.extend(clip(-60.0, Some(-20.0), 1.0));
        let (st, levels) = stats(&pcm);
        assert!(levels[50].muted && !levels.last().unwrap().muted && !st.silent());
        let (st, levels) = stats(&clip(-60.0, Some(-20.0), 3.0));
        assert!(levels.iter().all(|l| !l.muted) && !st.silent());
    }

    #[test]
    fn bluetooth_name_fallback() {
        assert!(bluetooth_by_name("Aditya's AirPods Pro") && bluetooth_by_name("Jabra Bluetooth Headset"));
        assert!(!bluetooth_by_name("MacBook Pro Microphone") && !bluetooth_by_name("Shure MV7"));
    }

    #[test]
    fn excluded_windows_dont_count_as_the_room_or_as_speech() {
        let mut st = LevelStats::default();
        st.exclude(4);
        for _ in 0..4 {
            st.push(-6.0); // Echotype's own start sound
        }
        for _ in 0..40 {
            st.push(-120.0);
        }
        assert!(st.silent(), "a loud start sound must not hide a muted mic");
        assert_eq!(st.floor_and_speech(), Some((-90.0, -120.0)));
    }

    /// Echotype's start, stop and lock sounds: original, synthesized here, written once to
    /// app/src-tauri/sounds and bundled. `cargo test generate_sounds -- --ignored` rewrites them.
    #[test]
    #[ignore = "writes the bundled sound files"]
    fn generate_sounds() {
        // Two soft notes per sound: (frequency Hz, start ms, length ms).
        let tone = |notes: &[(f32, f32, f32)], total_ms: f32| -> Vec<f32> {
            let n = (RATE as f32 * total_ms / 1000.0) as usize;
            let mut out = vec![0.0f32; n];
            for &(f, at, len) in notes {
                let (s0, len_n) = ((RATE as f32 * at / 1000.0) as usize, (RATE as f32 * len / 1000.0) as usize);
                for i in 0..len_n.min(n - s0) {
                    let t = i as f32 / RATE as f32;
                    let attack = (i as f32 / (RATE as f32 * 0.004)).min(1.0);
                    let decay = (-t * 38.0).exp();
                    let x = (2.0 * std::f32::consts::PI * f * t).sin() + 0.18 * (4.0 * std::f32::consts::PI * f * t).sin();
                    out[s0 + i] += 0.22 * attack * decay * x;
                }
            }
            out
        };
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sounds");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, pcm) in [
            ("start", tone(&[(784.0, 0.0, 60.0), (1175.0, 45.0, 65.0)], 110.0)),
            ("stop", tone(&[(1175.0, 0.0, 60.0), (784.0, 45.0, 65.0)], 110.0)),
            ("lock", tone(&[(1047.0, 0.0, 50.0), (1568.0, 40.0, 50.0), (2093.0, 75.0, 40.0)], 115.0)),
        ] {
            assert!(pcm.len() <= RATE as usize * 120 / 1000, "{name} must be 120 ms or shorter");
            std::fs::write(dir.join(format!("{name}.wav")), wav_bytes(&pcm)).unwrap();
        }
    }

    #[test]
    fn bundled_sounds_are_short_wavs() {
        for name in ["start", "stop", "lock"] {
            let bytes = std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sounds").join(format!("{name}.wav"))).unwrap();
            let pcm = decode(bytes, Some("wav")).unwrap();
            assert!(pcm.len() <= RATE as usize * 120 / 1000, "{name}: {} samples", pcm.len());
            assert!(pcm.iter().any(|s| s.abs() > 0.05), "{name} is silent");
        }
    }

    /// Prints floor, speech level and SNR for owner-recorded clips (kept out of the repo):
    /// `ECHOTYPE_WHISPER_FIXTURES=~/whispers cargo test whisper_fixtures -- --ignored --nocapture`
    #[test]
    #[ignore = "needs ECHOTYPE_WHISPER_FIXTURES, a folder of recorded .wav clips"]
    fn whisper_fixtures() {
        let dir = std::env::var("ECHOTYPE_WHISPER_FIXTURES").expect("set ECHOTYPE_WHISPER_FIXTURES");
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let Ok(pcm) = decode(std::fs::read(e.path()).unwrap(), e.path().extension().and_then(|x| x.to_str())) else { continue };
            let (st, _) = stats(&pcm);
            let (floor, speech) = st.floor_and_speech().unwrap_or_default();
            println!("{:40} floor {floor:6.1}  speech {speech:6.1}  snr {:5.1}  label {:?}", e.file_name().to_string_lossy(), speech - floor, st.quality());
        }
    }

    #[test]
    fn wav_round_trips_through_the_decoder() {
        let pcm = sine(440.0, RATE, 0.5).iter().map(|s| s * 0.5).collect::<Vec<_>>();
        let wav = wav_bytes(&pcm);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + pcm.len() * 2);
        let back = decode(wav, Some("wav")).unwrap();
        assert_eq!(back.len(), pcm.len());
        assert!(back.iter().zip(&pcm).all(|(a, b)| (a - b).abs() < 1e-3));
    }

    #[test]
    fn stereo_44k_wav_decodes_to_16k_mono() {
        // Hand-built 44.1 kHz stereo 16-bit WAV, 0.5 s.
        let frames = 22_050usize;
        let mut v = Vec::new();
        let data_len = (frames * 4) as u32;
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        for x in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), 44_100u32.to_le_bytes().to_vec(), (44_100u32 * 4).to_le_bytes().to_vec(), 4u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] {
            v.extend(x);
        }
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in sine(440.0, 44_100, 0.5) {
            let x = ((s * 0.5) * 32767.0) as i16;
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&x.to_le_bytes());
        }
        let back = decode(v, None).unwrap();
        assert_eq!(back.len(), 8_000);
        assert!((rms(&back) - 0.3536).abs() < 0.02);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode(b"definitely not audio".to_vec(), Some("mp3")).is_err());
        assert!(decode(vec![], None).is_err());
    }
}
