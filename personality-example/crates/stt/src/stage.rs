//! The stage process: events as JSON lines on stdin, `{from, text}` lines out.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use clap::Parser;

use crate::{Fake, Transcribe, process};

/// The stage's options.
#[derive(Parser, Debug)]
pub struct Args {
    /// whisper.cpp model, downloaded into --models on first use.
    #[arg(long, default_value = "base.en")]
    pub model: String,
    #[arg(long, default_value = "vesper-data/models")]
    pub models: PathBuf,
    /// Hear these words in every utterance instead of transcribing.
    #[arg(long)]
    pub fake: Option<String>,
}

async fn ensure_model(dir: &std::path::Path, name: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(format!("ggml-{name}.bin"));
    if path.exists() {
        return Ok(path);
    }
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{name}.bin");
    eprintln!("stt: downloading {url}");
    let bytes = reqwest::get(&url).await?.error_for_status()?.bytes().await?;
    tokio::fs::create_dir_all(dir).await?;
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(path)
}

/// Runs the stage on stdin/stdout until stdin closes.
pub async fn run(cli: Args) -> anyhow::Result<()> {
    let mut t: Box<dyn Transcribe> = match cli.fake {
        Some(text) => Box::new(Fake(text)),
        None => whisper(&cli).await?,
    };
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match process(&line, t.as_mut()) {
            Ok(Some(v)) => {
                let mut out = stdout.lock();
                writeln!(out, "{v}")?;
                out.flush()?;
            }
            Ok(None) => {}
            // One bad utterance shouldn't stop the stage; the node logs stderr.
            Err(e) => eprintln!("stt: {e}"),
        }
    }
    Ok(())
}

#[cfg(feature = "whisper")]
async fn whisper(cli: &Args) -> anyhow::Result<Box<dyn Transcribe>> {
    let model = ensure_model(&cli.models, &cli.model).await?;
    Ok(Box::new(crate::whisper::Whisper::load(&model)?))
}

#[cfg(not(feature = "whisper"))]
async fn whisper(_: &Args) -> anyhow::Result<Box<dyn Transcribe>> {
    let _ = ensure_model;
    anyhow::bail!("built without the whisper feature: use --fake TEXT")
}
