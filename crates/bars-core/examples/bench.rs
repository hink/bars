//! Downloads (if needed) and benchmarks catalog models on a WAV file.
//! usage: cargo run --release --example bench -- <wav> [model-id...]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use bars_core::models;
use transcribe_rs::TranscribeOptions;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let wav = args.next().expect("wav path");
    let ids: Vec<String> = args.collect();
    let samples = transcribe_rs::audio::read_wav_samples(std::path::Path::new(&wav))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let secs = samples.len() as f32 / 16000.0;
    println!("audio: {secs:.1}s");
    for info in models::CATALOG.iter().filter(|m| ids.is_empty() || ids.iter().any(|i| i == m.id)) {
        if !info.is_installed() {
            print!("downloading {} ... ", info.id);
            let mut last = 0;
            models::download(info, Arc::new(AtomicBool::new(false)), &mut |d, t| {
                let pct = (d * 100).checked_div(t).unwrap_or(0);
                if pct >= last + 10 { last = pct; print!("{pct}% "); use std::io::Write; let _ = std::io::stdout().flush(); }
            })?;
            println!("done");
        }
        let t0 = Instant::now();
        let mut model = bars_core::engine::load(info, &info.dir())?;
        let load = t0.elapsed();
        let _ = model.transcribe(&vec![0.0; 16000], &TranscribeOptions::default());
        let mut best = f32::MAX;
        let mut text = String::new();
        for _ in 0..3 {
            let t = Instant::now();
            let r = model.transcribe(&samples, &TranscribeOptions { language: Some("en".into()), ..Default::default() }).map_err(|e| anyhow::anyhow!("{e}"))?;
            best = best.min(t.elapsed().as_secs_f32());
            text = r.text;
        }
        println!("\n== {} ({:.0} MB)\n   load {:.2}s  infer {:.0} ms  RTF x{:.0}\n   {}", info.id, info.size_bytes() as f64 / 1e6, load.as_secs_f32(), best * 1000.0, secs / best, text.trim());
    }
    Ok(())
}
