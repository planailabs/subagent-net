//! The `vesper-stt` binary as a stage: JSON lines in, JSON lines out.

use std::io::Write;
use std::process::{Command, Stdio};

use base64::Engine;
use serde_json::{Value, json};

fn event(wav: &[u8]) -> String {
    json!({"from": "alice", "audio": {"$blob": {"base64": base64::engine::general_purpose::STANDARD.encode(wav), "mime": "audio/wav"}}}).to_string()
}

fn run(args: &[&str], input: &str) -> Vec<Value> {
    let mut c = Command::new(env!("CARGO_BIN_EXE_vesper-stt")).args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = c.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
fn fake_stage_turns_utterances_into_text() {
    let wav = vesper_room::tts::pcm_to_wav(&[0u8; 32000], 16_000).unwrap();
    let input = format!("{}\nnot json\n{}\n", event(&wav), json!({"tick": 3}));
    assert_eq!(run(&["--fake", "make me a coffee"], &input), [json!({"from": "alice", "text": "make me a coffee"}), json!({"tick": 3})]);
}

/// Piper says it, whisper hears it.
#[tokio::test]
#[ignore = "needs piper on PATH; downloads a voice and a whisper model"]
async fn whisper_hears_piper() {
    use vesper_room::tts::{DEFAULT_VOICE, Tts, ensure_voice};
    let data = std::env::temp_dir().join("vesper-stt-test");
    let model = ensure_voice(&data.join("voices"), DEFAULT_VOICE).await.unwrap();
    let speech = Tts::Piper { bin: "piper".into(), model }.speak("Could you make me a coffee, please?").await.unwrap();
    let models = data.join("models");
    let out = run(&["--models", models.to_str().unwrap()], &format!("{}\n", event(&speech.wav)));
    let text = out[0]["text"].as_str().unwrap().to_lowercase();
    assert!(text.contains("coffee"), "{text}");
}
