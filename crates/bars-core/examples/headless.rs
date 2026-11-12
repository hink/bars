//! Runs the controller without a UI and prints its events.
//! usage: cargo run --example headless [seconds]

use std::time::{Duration, Instant};

use bars_core::config::Config;
use bars_core::controller::{self, Event};

fn main() {
    bars_core::audio::prepare_environment();
    env_logger::init();
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4);
    let cfg = Config::load_or_default(&bars_core::paths::config_file());
    let (tx, rx) = async_channel::unbounded();
    let _handle = controller::spawn(cfg, tx);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        match rx.recv_blocking() {
            Ok(Event::Level(_)) => {}
            Ok(Event::History { entries, stats }) => println!("{:>6} ms  History({} entries, {stats:?})", start.elapsed().as_millis(), entries.len()),
            Ok(Event::Config(_)) => println!("{:>6} ms  Config", start.elapsed().as_millis()),
            Ok(ev) => println!("{:>6} ms  {ev:?}", start.elapsed().as_millis()),
            Err(_) => break,
        }
    }
}
