//! The `stt` sense stage: events `{from, audio: {"$blob": {base64, mime}}}`
//! in (JSON lines on stdin), `{from, text}` out. The audio is dropped, so
//! it never reaches the blob store or Vesper.

pub mod stage;

use base64::Engine;
use serde_json::Value;

/// Anything that turns 16 kHz mono samples into text.
pub trait Transcribe {
    fn transcribe(&mut self, samples: &[f32]) -> anyhow::Result<String>;
}

/// Always hears the same words (tests, demos without a model).
pub struct Fake(pub String);

impl Transcribe for Fake {
    fn transcribe(&mut self, _: &[f32]) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

/// A WAV file as 16 kHz mono samples.
pub fn decode(wav: &[u8]) -> anyhow::Result<Vec<f32>> {
    let mut r = hound::WavReader::new(std::io::Cursor::new(wav))?;
    let spec = r.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.map(|s| s as f32 / scale)).collect::<Result<_, _>>()?
        }
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
    };
    let ch = spec.channels as usize;
    let mono: Vec<f32> = raw.chunks(ch).map(|c| c.iter().sum::<f32>() / ch as f32).collect();
    Ok(resample(&mono, spec.sample_rate, 16_000))
}

fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let n = (x.len() as u64 * to as u64 / from as u64) as usize;
    (0..n)
        .map(|i| {
            let p = i as f64 * from as f64 / to as f64;
            let j = p as usize;
            let f = (p - j as f64) as f32;
            let a = x[j.min(x.len() - 1)];
            let b = x[(j + 1).min(x.len() - 1)];
            a + (b - a) * f
        })
        .collect()
}

/// Drops whisper's non-speech annotations ("[BLANK_AUDIO]", "(wind)").
pub fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in text.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One input line to at most one output line. Events without audio pass
/// through; silence gives nothing.
pub fn process(line: &str, t: &mut dyn Transcribe) -> anyhow::Result<Option<Value>> {
    let mut v: Value = serde_json::from_str(line)?;
    let Some(audio) = v.as_object_mut().and_then(|o| o.remove("audio")) else {
        return Ok(Some(v));
    };
    let b64 = audio.pointer("/$blob/base64").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("audio is not an inline {{\"$blob\"}}"))?;
    let wav = base64::engine::general_purpose::STANDARD.decode(b64)?;
    let text = clean(&t.transcribe(&decode(&wav)?)?);
    if text.is_empty() {
        return Ok(None);
    }
    v["text"] = text.into();
    Ok(Some(v))
}

#[cfg(feature = "whisper")]
pub mod whisper {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    pub struct Whisper(WhisperContext);

    impl Whisper {
        pub fn load(model: &std::path::Path) -> anyhow::Result<Self> {
            // whisper.cpp prints its model details on stderr; send them nowhere.
            whisper_rs::install_logging_hooks();
            let path = model.to_str().ok_or_else(|| anyhow::anyhow!("model path isn't UTF-8"))?;
            Ok(Whisper(WhisperContext::new_with_params(path, WhisperContextParameters::default())?))
        }
    }

    impl super::Transcribe for Whisper {
        fn transcribe(&mut self, samples: &[f32]) -> anyhow::Result<String> {
            let mut state = self.0.create_state()?;
            let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            p.set_language(Some("en"));
            p.set_print_progress(false);
            p.set_print_realtime(false);
            p.set_print_timestamps(false);
            p.set_print_special(false);
            p.set_suppress_blank(true);
            state.full(p, samples)?;
            let mut text = String::new();
            for seg in state.as_iter() {
                text.push_str(&seg.to_string());
            }
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wav(rate: u32, channels: u16, secs: f32) -> Vec<u8> {
        let spec = hound::WavSpec { channels, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut buf = std::io::Cursor::new(vec![]);
        let mut w = hound::WavWriter::new(&mut buf, spec).unwrap();
        for i in 0..(rate as f32 * secs) as usize * channels as usize {
            w.write_sample(((i as f32 * 0.05).sin() * 8000.0) as i16).unwrap();
        }
        w.finalize().unwrap();
        buf.into_inner()
    }

    fn event(wav: &[u8]) -> String {
        json!({"from": "alice", "audio": {"$blob": {"base64": base64::engine::general_purpose::STANDARD.encode(wav), "mime": "audio/wav"}}}).to_string()
    }

    #[test]
    fn decodes_and_resamples_to_16k_mono() {
        assert_eq!(decode(&wav(16_000, 1, 1.0)).unwrap().len(), 16_000);
        assert_eq!(decode(&wav(48_000, 2, 0.5)).unwrap().len(), 8_000);
        assert!(decode(b"not a wav").is_err());
    }

    #[test]
    fn audio_becomes_text_and_is_dropped() {
        let mut t = Fake("make me a coffee".into());
        let out = process(&event(&wav(16_000, 1, 1.0)), &mut t).unwrap().unwrap();
        assert_eq!(out, json!({"from": "alice", "text": "make me a coffee"}));
    }

    #[test]
    fn silence_and_annotations() {
        assert_eq!(clean(" [BLANK_AUDIO] "), "");
        assert_eq!(clean("(wind blowing) Hello  there [music]"), "Hello there");
        let mut t = Fake("[BLANK_AUDIO]".into());
        assert!(process(&event(&wav(16_000, 1, 0.5)), &mut t).unwrap().is_none());
    }

    #[test]
    fn other_events_pass_through_and_bad_audio_fails() {
        let mut t = Fake("x".into());
        assert_eq!(process(r#"{"tick":1}"#, &mut t).unwrap().unwrap(), json!({"tick": 1}));
        assert!(process(r#"{"audio":"blob:abc"}"#, &mut t).is_err());
    }
}
