//! `vesper-stt [--model NAME] [--models DIR] [--fake TEXT]`: the stt stage.

use clap::Parser;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    vesper_stt::stage::run(vesper_stt::stage::Args::parse()).await
}
