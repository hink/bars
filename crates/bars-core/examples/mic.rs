//! Records from the default microphone for N seconds and prints level stats
//! (no audio is saved). usage: cargo run --example mic [secs]
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

fn main() {
    bars_core::audio::prepare_environment();
    env_logger::init();
    let secs: f32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
    for d in bars_core::audio::list_input_devices() {
        println!("device: {}{} [{}]", d.name, if d.is_default { " (default)" } else { "" }, d.id);
    }
    let updates = Arc::new(AtomicU32::new(0));
    let u = updates.clone();
    let audio = bars_core::audio::Audio::spawn(Arc::new(move |_l| { u.fetch_add(1, Ordering::Relaxed); }), Arc::new(|e| eprintln!("microphone: {e}")));
    let t = std::time::Instant::now();
    audio.start(None, false);
    std::thread::sleep(Duration::from_secs_f32(secs));
    let rec = audio.stop().recording;
    println!(
        "captured {:.2}s of 16 kHz audio in {:.2}s wall; peak {:.3}; loudest-100ms rms {:.4}; level updates {}",
        rec.duration().as_secs_f32(), t.elapsed().as_secs_f32(), rec.peak, rec.loudest_window_rms(), updates.load(Ordering::Relaxed)
    );
}
