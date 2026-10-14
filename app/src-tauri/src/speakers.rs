//! Who spoke when, on this computer: the two ONNX models meeting notes need (downloaded on first
//! use like the whisper models), sherpa-onnx diarization over a whole recording, and a light
//! online clusterer that labels speakers live while the meeting runs.

use std::path::PathBuf;

pub struct SpeakerModel {
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size_mb: u64,
    pub license: &'static str,
}

/// pyannote segmentation-3.0 (MIT) finds speech turns; 3D-Speaker ERes2Net (Apache-2.0, trained on
/// VoxCeleb English) turns a turn into a voice fingerprint. Both pinned to a Hugging Face commit.
pub const MODELS: [SpeakerModel; 2] = [
    SpeakerModel {
        file: "pyannote-segmentation-3.0.onnx",
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/9403a6902bb58e3d5ae8c7e77c3422de279db2e0/model.onnx",
        sha256: "220ad67ca923bef2fa91f2390c786097bf305bceb5e261d4af67b38e938e1079",
        size_mb: 6,
        license: "MIT (pyannote/segmentation-3.0)",
    },
    // ERes2Net, not CAM++: the CAM++ VoxCeleb export scored one voice 0.67 against itself and
    // 0.94 against a different one on the test meeting; ERes2Net kept voices apart (same 0.85-0.90,
    // different <= 0.57).
    SpeakerModel {
        file: "3dspeaker-eres2net-en-voxceleb.onnx",
        url: "https://huggingface.co/csukuangfj/speaker-embedding-models/resolve/0743f301363dec56491a490f6d6cbc9d67f9a3bf/3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx",
        sha256: "c59158379255ad66e161679cca6af8d52d51e389e3224ab7d7a7baae295c2db5",
        size_mb: 26,
        license: "Apache-2.0 (3D-Speaker, iic/speech_eres2net_sv_en_voxceleb_16k)",
    },
];

pub fn path(m: &SpeakerModel) -> PathBuf {
    crate::stt::models_dir().join("speakers").join(m.file)
}

/// Fetches whichever model is missing. `progress(file, done, total)`.
pub fn ensure(mut progress: impl FnMut(&str, u64, Option<u64>)) -> Result<(), String> {
    for m in MODELS.iter().filter(|m| !path(m).is_file()) {
        crate::stt::fetch(m.url, &path(m), m.sha256, |d, t| progress(m.file, d, t))?;
    }
    Ok(())
}

/// Cosine similarity a voice must reach to join a speaker live (against the speaker's running
/// mean, so stricter than the final pass, which needs every pair of a speaker's turns over 0.5).
// ponytail: thresholds from synthetic voices; tune both on recorded meetings.
const SAME_SPEAKER_LIVE: f32 = 0.6;
const SAME_SPEAKER_FINAL: f32 = 0.5;

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let n = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    dot / (n(a) * n(b))
}

/// Live speaker labels: each voice fingerprint joins the closest speaker so far (running mean)
/// or starts a new one. Indices count from 0 in order of first appearance.
#[derive(Default)]
pub struct Clusters {
    centroids: Vec<(Vec<f32>, f32)>,
}

impl Clusters {
    pub fn assign(&mut self, e: &[f32]) -> usize {
        let best = self.centroids.iter().enumerate().map(|(i, (c, _))| (i, cosine(c, e))).max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((i, sim)) if sim >= SAME_SPEAKER_LIVE => {
                let (c, n) = &mut self.centroids[i];
                c.iter_mut().zip(e).for_each(|(c, x)| *c = (*c * *n + x) / (*n + 1.0));
                *n += 1.0;
                i
            }
            _ => {
                self.centroids.push((e.to_vec(), 1.0));
                self.centroids.len() - 1
            }
        }
    }
}

/// A speech turn found by diarization: seconds, and a cluster number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Turn {
    pub start: f32,
    pub end: f32,
    pub speaker: i32,
}

#[cfg(target_os = "macos")]
mod engine {
    use super::*;

    pub struct Embedder(sherpa_rs::speaker_id::EmbeddingExtractor);

    impl Embedder {
        pub fn new() -> Result<Self, String> {
            let cfg = sherpa_rs::speaker_id::ExtractorConfig { model: path(&MODELS[1]).to_string_lossy().into_owned(), ..Default::default() };
            sherpa_rs::speaker_id::EmbeddingExtractor::new(cfg).map(Self).map_err(|e| format!("Could not load the speaker model: {e}"))
        }

        pub fn embed(&mut self, pcm: &[f32]) -> Option<Vec<f32>> {
            self.0.compute_speaker_embedding(pcm.to_vec(), crate::audio::RATE).ok()
        }
    }

    /// Diarizes a whole 16 kHz recording. `speakers`: how many there are, when known.
    /// Threads for the final pass: it runs once, after the meeting, while nothing else is transcribing.
    // ponytail: fixed 4 threads; scale with core count if hour-long meetings take too long to finish.
    const THREADS: i32 = 4;

    /// Diarizes a whole 16 kHz recording. `speakers`: how many there are, when known.
    /// Called through sherpa-onnx's C API because sherpa-rs pins its threads to 1.
    pub fn diarize(pcm: Vec<f32>, speakers: Option<i32>) -> Result<Vec<Turn>, String> {
        use sherpa_rs::sherpa_rs_sys as sys;
        use std::ffi::CString;
        let c = |p: PathBuf| CString::new(p.to_string_lossy().into_owned()).map_err(|e| e.to_string());
        let (seg, emb, cpu) = (c(path(&MODELS[0]))?, c(path(&MODELS[1]))?, CString::new("cpu").expect("no NUL"));
        let cfg = sys::SherpaOnnxOfflineSpeakerDiarizationConfig {
            segmentation: sys::SherpaOnnxOfflineSpeakerSegmentationModelConfig {
                pyannote: sys::SherpaOnnxOfflineSpeakerSegmentationPyannoteModelConfig { model: seg.as_ptr() },
                num_threads: THREADS,
                debug: 0,
                provider: cpu.as_ptr(),
            },
            embedding: sys::SherpaOnnxSpeakerEmbeddingExtractorConfig { model: emb.as_ptr(), num_threads: THREADS, debug: 0, provider: cpu.as_ptr() },
            clustering: sys::SherpaOnnxFastClusteringConfig {
                num_clusters: speakers.unwrap_or(-1), // -1: decide by threshold
                threshold: 1.0 - SAME_SPEAKER_FINAL,  // sherpa's threshold is a cosine distance
            },
            min_duration_on: 0.3,
            min_duration_off: 0.5,
        };
        // SAFETY: every pointer in `cfg` outlives the calls; results are freed with their own destroy calls.
        unsafe {
            let sd = sys::SherpaOnnxCreateOfflineSpeakerDiarization(&cfg);
            if sd.is_null() {
                return Err("Could not load the speaker models".into());
            }
            let result = sys::SherpaOnnxOfflineSpeakerDiarizationProcess(sd, pcm.as_ptr(), pcm.len() as i32);
            if result.is_null() {
                sys::SherpaOnnxDestroyOfflineSpeakerDiarization(sd);
                return Err("Speaker detection failed".into());
            }
            let n = sys::SherpaOnnxOfflineSpeakerDiarizationResultGetNumSegments(result).max(0) as usize;
            let segs = sys::SherpaOnnxOfflineSpeakerDiarizationResultSortByStartTime(result);
            let turns = if segs.is_null() { vec![] } else { std::slice::from_raw_parts(segs, n).iter().map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker }).collect() };
            if !segs.is_null() {
                sys::SherpaOnnxOfflineSpeakerDiarizationDestroySegment(segs);
            }
            sys::SherpaOnnxOfflineSpeakerDiarizationDestroyResult(result);
            sys::SherpaOnnxDestroyOfflineSpeakerDiarization(sd);
            Ok(turns)
        }
    }
}

#[cfg(target_os = "macos")]
pub use engine::{Embedder, diarize};

// ponytail: sherpa-onnx is linked on macOS only (the only place computer audio is captured);
// Windows/Linux meetings are mic-only, so they never need it. Link it there when they capture output audio.
#[cfg(not(target_os = "macos"))]
pub struct Embedder;

#[cfg(not(target_os = "macos"))]
impl Embedder {
    pub fn new() -> Result<Self, String> {
        Err("Speaker detection is macOS-only for now".into())
    }
    pub fn embed(&mut self, _: &[f32]) -> Option<Vec<f32>> {
        None
    }
}

#[cfg(not(target_os = "macos"))]
pub fn diarize(_: Vec<f32>, _: Option<i32>) -> Result<Vec<Turn>, String> {
    Err("Speaker detection is macOS-only for now".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_clusters_join_close_voices_and_split_far_ones() {
        let mut c = Clusters::default();
        assert_eq!(c.assign(&[1.0, 0.0, 0.0]), 0);
        assert_eq!(c.assign(&[0.9, 0.1, 0.0]), 0, "close to speaker 0");
        assert_eq!(c.assign(&[0.0, 1.0, 0.0]), 1, "orthogonal voice is new");
        assert_eq!(c.assign(&[0.0, 0.0, 1.0]), 2);
        assert_eq!(c.assign(&[0.05, 0.95, 0.0]), 1);
    }

    /// Prints how alike each pair of known turns sounds to the fingerprint model:
    /// `ECHOTYPE_SPEAKER_FIXTURE=<dir with meeting.wav and truth.json> cargo test turn_similarity -- --ignored --nocapture`
    #[test]
    #[ignore = "needs ECHOTYPE_SPEAKER_FIXTURE and the downloaded speaker models"]
    fn turn_similarity() {
        let dir = PathBuf::from(std::env::var("ECHOTYPE_SPEAKER_FIXTURE").unwrap());
        let pcm = crate::audio::decode(std::fs::read(dir.join("meeting.wav")).unwrap(), Some("wav")).unwrap();
        let truth: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(dir.join("truth.json")).unwrap()).unwrap();
        let mut e = Embedder::new().unwrap();
        let embs: Vec<Vec<f32>> = truth.iter().map(|t| {
            let (a, b) = ((t["start"].as_f64().unwrap() * 16_000.0) as usize, (t["end"].as_f64().unwrap() * 16_000.0) as usize);
            e.embed(&pcm[a..b.min(pcm.len())]).unwrap()
        }).collect();
        for (i, a) in embs.iter().enumerate() {
            let row: Vec<String> = embs.iter().map(|b| format!("{:5.2}", cosine(a, b))).collect();
            println!("{:9} {}", truth[i]["speaker"].as_str().unwrap(), row.join(" "));
        }
    }

    #[test]
    fn models_are_pinned_and_checksummed() {
        for m in &MODELS {
            assert!(m.url.contains("/resolve/") && !m.url.contains("/main/"), "{} must be pinned to a commit", m.file);
            assert_eq!(m.sha256.len(), 64);
            assert!(!m.license.is_empty());
        }
    }
}
