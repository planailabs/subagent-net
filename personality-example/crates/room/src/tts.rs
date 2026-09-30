//! Her voice: Piper (a CLI; there is no Rust binding) or a silent stand-in
//! for tests and machines without Piper. Either way the result is a WAV
//! and a loudness envelope for lip sync.

use std::io::Cursor;
use std::path::{Path, PathBuf};

/// One envelope value per this many seconds.
pub const FRAME: f64 = 0.04;
pub const DEFAULT_VOICE: &str = "en_GB-jenny_dioco-medium";

#[derive(Clone, Debug)]
pub enum Tts {
    /// No audio; the mouth still moves, about 0.3 s per word.
    Silent,
    Piper { bin: String, model: PathBuf },
}

pub struct Speech {
    pub wav: Vec<u8>,
    /// Loudness per 40 ms, 0..1.
    pub envelope: Vec<f32>,
    pub secs: f64,
}

impl Tts {
    pub async fn speak(&self, text: &str) -> anyhow::Result<Speech> {
        match self {
            Tts::Silent => {
                let words = text.split_whitespace().count().max(1);
                let secs = 0.3 * words as f64;
                let rate = 16_000;
                let samples = vec![0i16; (secs * rate as f64) as usize];
                let n = (secs / FRAME).ceil() as usize;
                // A syllable-ish rhythm so the mouth moves.
                let envelope = (0..n).map(|i| if i % 5 == 4 { 0.1 } else { 0.35 + 0.3 * ((i as f32) * 1.7).sin().abs() }).collect();
                Ok(Speech { wav: wav(&samples, rate)?, envelope, secs })
            }
            Tts::Piper { bin, model } => {
                use tokio::io::AsyncWriteExt;
                let out = std::env::temp_dir().join(format!("vesper-say-{}.wav", uuid::Uuid::new_v4()));
                let mut child = tokio::process::Command::new(bin)
                    .arg("-m")
                    .arg(model)
                    .arg("-f")
                    .arg(&out)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| anyhow::anyhow!("starting {bin}: {e}"))?;
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(text.replace('\n', " ").as_bytes()).await?;
                drop(stdin);
                let res = child.wait_with_output().await?;
                let bytes = tokio::fs::read(&out).await;
                let _ = tokio::fs::remove_file(&out).await;
                anyhow::ensure!(res.status.success(), "piper failed: {}", String::from_utf8_lossy(&res.stderr).trim());
                analyse(bytes?)
            }
        }
    }
}

fn wav(samples: &[i16], rate: u32) -> anyhow::Result<Vec<u8>> {
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut buf = Cursor::new(vec![]);
    let mut w = hound::WavWriter::new(&mut buf, spec)?;
    for s in samples {
        w.write_sample(*s)?;
    }
    w.finalize()?;
    Ok(buf.into_inner())
}

/// PCM s16le mono bytes as a WAV file.
pub fn pcm_to_wav(pcm: &[u8], rate: u32) -> anyhow::Result<Vec<u8>> {
    let samples: Vec<i16> = pcm.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    wav(&samples, rate)
}

/// Duration and loudness envelope of a 16-bit WAV.
pub fn analyse(wav: Vec<u8>) -> anyhow::Result<Speech> {
    let mut r = hound::WavReader::new(Cursor::new(&wav))?;
    let spec = r.spec();
    anyhow::ensure!(spec.bits_per_sample == 16, "expected 16-bit audio");
    let samples: Vec<i16> = r.samples::<i16>().collect::<Result<_, _>>()?;
    let ch = spec.channels as usize;
    let frame = ((spec.sample_rate as f64 * FRAME) as usize * ch).max(1);
    let rms: Vec<f32> = samples
        .chunks(frame)
        .map(|c| (c.iter().map(|&s| (s as f32 / 32768.0).powi(2)).sum::<f32>() / c.len() as f32).sqrt())
        .collect();
    let peak = rms.iter().cloned().fold(0.0f32, f32::max).max(1e-4);
    let secs = samples.len() as f64 / ch as f64 / spec.sample_rate as f64;
    Ok(Speech { envelope: rms.iter().map(|v| (v / peak).min(1.0)).collect(), secs, wav })
}

/// Downloads a Piper voice (model and config) into `dir` unless it's there.
pub async fn ensure_voice(dir: &Path, voice: &str) -> anyhow::Result<PathBuf> {
    let model = dir.join(format!("{voice}.onnx"));
    if model.exists() && model.with_extension("onnx.json").exists() {
        return Ok(model);
    }
    // en_GB-jenny_dioco-medium -> en/en_GB/jenny_dioco/medium/
    let parts: Vec<&str> = voice.split('-').collect();
    anyhow::ensure!(parts.len() == 3, "voice names look like en_GB-jenny_dioco-medium");
    let lang = parts[0].split('_').next().unwrap();
    let base = format!("https://huggingface.co/rhasspy/piper-voices/resolve/main/{lang}/{}/{}/{}/{voice}", parts[0], parts[1], parts[2]);
    tokio::fs::create_dir_all(dir).await?;
    for ext in ["onnx.json", "onnx"] {
        let url = format!("{base}.{ext}");
        tracing::info!("downloading {url}");
        let bytes = reqwest::get(&url).await?.error_for_status()?.bytes().await?;
        let path = dir.join(format!("{voice}.{ext}"));
        let tmp = path.with_extension("part");
        tokio::fs::write(&tmp, &bytes).await?;
        tokio::fs::rename(&tmp, &path).await?;
    }
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn silent_speech_has_a_moving_mouth() {
        let s = Tts::Silent.speak("hello there you").await.unwrap();
        assert!((s.secs - 0.9).abs() < 1e-9);
        assert_eq!(s.envelope.len(), 23);
        assert!(s.envelope.iter().any(|&v| v > 0.3));
        assert!((analyse(s.wav).unwrap().secs - 0.9).abs() < 1e-3);
    }

    #[test]
    fn envelope_follows_loudness() {
        let rate = 16_000;
        let mut pcm = vec![];
        for i in 0..rate {
            // Loud first half, quiet second half.
            let amp = if i < rate / 2 { 16000.0 } else { 1600.0 };
            let s = (amp * (i as f32 * 0.1).sin()) as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        let s = analyse(pcm_to_wav(&pcm, rate).unwrap()).unwrap();
        assert!((s.secs - 1.0).abs() < 1e-6);
        assert_eq!(s.envelope.len(), 25);
        assert!(s.envelope[2] > 0.95 && s.envelope[20] < 0.15, "{:?}", s.envelope);
    }

    #[tokio::test]
    #[ignore = "needs piper on PATH and downloads a voice"]
    async fn piper_speaks() {
        let dir = std::env::temp_dir().join("vesper-voices");
        let model = ensure_voice(&dir, DEFAULT_VOICE).await.unwrap();
        let s = Tts::Piper { bin: "piper".into(), model }.speak("Hello, I'm Vesper.").await.unwrap();
        assert!(s.secs > 0.5 && s.envelope.iter().any(|&v| v > 0.9));
    }
}
