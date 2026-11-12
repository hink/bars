//! Transcribes a WAV file whole and in the pieces Bars cuts a long
//! dictation into while you talk, to compare the two and see where the
//! cuts fall. The model must be installed (see the bench example).
//! `--rate N` records it at N Hz first, like a microphone (often 48000).
//! usage: cargo run --release -p bars-core --example pieces -- [--rate N] <wav> [model-id]

use std::time::Duration;

use bars_core::engine::{Engine, EngineEvent, EngineStatus, Transcript};
use bars_core::models;
use bars_core::segment::Segmenter;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let rate = match args.iter().position(|a| a == "--rate") {
        Some(i) => {
            args.remove(i);
            args.remove(i).parse().expect("--rate N")
        }
        None => bars_core::audio::SAMPLE_RATE,
    };
    let mut args = args.into_iter();
    let wav = args.next().expect("wav path");
    let id = args.next();
    let samples = transcribe_rs::audio::read_wav_samples(std::path::Path::new(&wav)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let samples = bars_core::audio::resample(&samples, bars_core::audio::SAMPLE_RATE, rate);
    let secs = |n: usize| n as f32 / rate as f32;
    let info = models::CATALOG
        .iter()
        .find(|m| id.as_deref().map_or(m.is_installed(), |id| m.id == id))
        .expect("an installed model");

    let (tx, rx) = crossbeam_channel::unbounded();
    let engine = Engine::spawn(tx);
    engine.select(info.id, 0);
    let next = || -> anyhow::Result<Transcript> {
        loop {
            match rx.recv_timeout(Duration::from_secs(300))? {
                EngineEvent::Done(t) => return Ok(t),
                EngineEvent::Failed { error, .. } => anyhow::bail!(error),
                EngineEvent::Status(EngineStatus::Failed { error, .. } | EngineStatus::Missing(error)) => anyhow::bail!(error),
                _ => {}
            }
        }
    };

    // Fed as the controller does, a few hundred ms at a time.
    let mut segmenter = Segmenter::new(rate);
    let mut pieces = Vec::new();
    for chunk in samples.chunks(rate as usize * 3 / 10) {
        let mut next = segmenter.push(chunk);
        while let Some(piece) = next {
            pieces.push(piece);
            next = segmenter.push(&[]);
        }
    }
    pieces.push(segmenter.last(&samples));

    println!("audio: {:.1}s, {} in {} pieces", secs(samples.len()), info.id, pieces.len());
    let mut at = 0;
    let mut joined = Vec::new();
    let mut last_ms = 0;
    for (i, piece) in pieces.into_iter().enumerate() {
        let len = piece.keep.len();
        engine.transcribe_piece(i as u64, piece, rate, None);
        let t = next()?;
        println!("  {:>5.1}–{:<5.1}s  {:>4} ms  {}", secs(at), secs(at + len), t.infer_ms, t.text);
        at += len;
        last_ms = t.infer_ms;
        if !t.text.is_empty() {
            joined.push(t.text);
        }
    }
    engine.transcribe(100, samples, rate, None);
    let whole = next()?;
    println!("\nwhole ({} ms after you stop):\n  {}", whole.infer_ms, whole.text);
    let joined = joined.join(" ");
    println!("\nin pieces ({last_ms} ms after you stop):\n  {joined}");
    println!("\n{}", if joined == whole.text { "the same" } else { "different" });
    Ok(())
}
