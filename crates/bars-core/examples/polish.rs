//! Polishes dictations with the default polish model (or `--model ID`,
//! another one) and times it, downloading the model if needed. One
//! dictation per line, from the arguments or stdin; `before|text|after`
//! gives the text around the cursor, which with `--ahead` is read ahead
//! (untimed), as Bars does while you talk.
//! usage: cargo run --release -p bars-core --example polish -- "um so i think we should uh ship it"
//!        cargo run --release -p bars-core --example polish -- --ahead < testdata/polish.txt
//!        cargo run --release -p bars-core --example polish -- --model spokenly-cleanup-1 --ahead < testdata/polish.txt

use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use bars_core::text::Surroundings;
use bars_core::{models, polish};

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let info = match args.iter().position(|a| a == "--model") {
        Some(i) => {
            let id = args.get(i + 1).cloned().unwrap_or_default();
            args.drain(i..(i + 2).min(args.len()));
            let m = models::polish_model(&id);
            anyhow::ensure!(m.id == id, "no polish model {id:?}");
            m
        }
        None => models::polish_model(models::DEFAULT_POLISH),
    };
    if !info.is_installed() {
        eprintln!("downloading {} ...", info.name);
        models::download(info, Arc::new(AtomicBool::new(false)), &mut |_, _| {})?;
    }
    let t0 = Instant::now();
    let mut model = polish::Model::load(&polish::model_path(info), polish::Format::of(info))?;
    let _ = model.rewrite("Hello there.", None, None)?;
    eprintln!("loaded {} in {} ms", info.name, t0.elapsed().as_millis());

    let ahead = args.first().is_some_and(|a| a == "--ahead");
    if ahead {
        args.remove(0);
    }
    let lines: Vec<String> = if args.is_empty() {
        std::io::stdin().lock().lines().map_while(Result::ok).filter(|l| !l.trim().is_empty()).collect()
    } else {
        args
    };
    let mut times = Vec::new();
    for line in lines {
        let parts: Vec<&str> = line.split('|').collect();
        let (field, text) = match parts.as_slice() {
            [before, text, after] => (Some(Surroundings { before: before.to_string(), selected: String::new(), after: after.to_string() }), *text),
            _ => (None, line.as_str()),
        };
        if ahead {
            model.prepare(None, field.as_ref())?;
        }
        let t = Instant::now();
        let out = model.rewrite(text, None, field.as_ref())?.text;
        let ms = t.elapsed().as_millis();
        times.push(ms);
        let verdict = match &out {
            Some(o) if polish::acceptable(text, o) => "ok",
            Some(_) => "REJECTED",
            None => "TIMEOUT",
        };
        println!("{ms:>5} ms  {verdict:<8}  {text}\n                   → {}", out.unwrap_or_default());
    }
    times.sort();
    if !times.is_empty() {
        println!("median {} ms, max {} ms", times[times.len() / 2], times[times.len() - 1]);
    }
    Ok(())
}
