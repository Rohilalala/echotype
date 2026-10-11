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
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.94; // fraction of the input Nyquist, small guard band
    let half = (16.0 / cutoff).ceil() as i64; // taps on each side
    let n_out = (x.len() as f64 * ratio).floor() as usize;
    (0..n_out)
        .map(|i| {
            let t = i as f64 / ratio;
            let center = t.floor() as i64;
            let lo = (center - half + 1).max(0);
            let hi = (center + half).min(x.len() as i64 - 1);
            let mut acc = 0.0;
            for k in lo..=hi {
                let d = t - k as f64;
                let w = 0.5 * (1.0 + (std::f64::consts::PI * d / half as f64).cos());
                acc += x[k as usize] as f64 * cutoff * sinc(cutoff * d) * w;
            }
            acc as f32
        })
        .collect()
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

pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices().map(|ds| ds.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
}

/// A running capture. The cpal stream lives on its own thread (it is !Send on macOS).
pub struct Recorder {
    stop: mpsc::Sender<()>,
    handle: JoinHandle<(Vec<f32>, u32)>,
}

impl Recorder {
    /// Opens `device` (or the default input) and starts recording. `on_level` gets 0..1 at ~20 Hz.
    pub fn start(device: Option<String>, on_level: impl Fn(f32) + Send + 'static) -> Result<Self, String> {
        let (stop, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let handle = std::thread::spawn(move || {
            let buf = Arc::new(Mutex::new(Vec::<f32>::with_capacity(48_000 * 30)));
            let (stream, rate) = match open_stream(device, buf.clone()) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return (vec![], RATE);
                }
            };
            let _ = ready_tx.send(Ok(()));
            let window = (rate / 20) as usize;
            while let Err(mpsc::RecvTimeoutError::Timeout) = stop_rx.recv_timeout(Duration::from_millis(50)) {
                let b = buf.lock().unwrap_or_else(|e| e.into_inner());
                let tail = &b[b.len().saturating_sub(window)..];
                on_level(level(tail));
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

/// RMS mapped from -60..0 dBFS onto 0..1 for the overlay meter.
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    ((20.0 * rms.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn open_stream(device: Option<String>, buf: Arc<Mutex<Vec<f32>>>) -> Result<(cpal::Stream, u32), String> {
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
        F::F32 => build::<f32>(&dev, &sc, buf),
        F::I16 => build::<i16>(&dev, &sc, buf),
        F::U16 => build::<u16>(&dev, &sc, buf),
        F::I32 => build::<i32>(&dev, &sc, buf),
        F::I8 => build::<i8>(&dev, &sc, buf),
        F::U8 => build::<u8>(&dev, &sc, buf),
        F::F64 => build::<f64>(&dev, &sc, buf),
        f => return Err(format!("Unsupported microphone sample format {f}")),
    }
    .map_err(|e| format!("Could not open the microphone: {e}"))?;
    stream.play().map_err(|e| format!("Could not start the microphone: {e}"))?;
    Ok((stream, rate))
}

fn build<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, buf: Arc<Mutex<Vec<f32>>>) -> Result<cpal::Stream, cpal::BuildStreamError>
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
        |e| eprintln!("echotype: audio stream error: {e}"),
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
    fn mono_mix_and_level() {
        assert_eq!(to_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(level(&[0.0; 100]), 0.0);
        assert_eq!(level(&[1.0; 100]), 1.0);
        assert!((level(&[0.031_622_8; 100]) - 0.5).abs() < 0.01); // -30 dBFS
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
