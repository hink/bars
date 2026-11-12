//! Mutes other audio while the user dictates, and turns it back on after:
//! music and videos don't play over them, or into the microphone.
//!
//! Windows and Linux mute each other app's audio, so Bars' own sounds still
//! play. macOS has no public way to mute one app, so there the output
//! device is muted, once the start sound has played, and unmuted before the
//! stop sound.
//!
//! Only what Bars muted is turned back on: audio that was muted already
//! stays muted, and audio the user unmutes during a dictation isn't muted
//! again. What's muted is written down until it's back on, so if Bars
//! crashes or is killed meanwhile, its next start turns it back on.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use serde::{Deserialize, Serialize};

/// Whether muting silences Bars' own sounds too (the whole output device).
pub const MUTES_OWN_SOUNDS: bool = cfg!(target_os = "macos");

/// While muted, how often to look for audio to mute that wasn't playing
/// before (an app that starts playing, or a new output device).
const REFRESH_EVERY: Duration = Duration::from_secs(1);

/// How often, and for how long after starting, to look again for what an
/// earlier run left muted and couldn't be unmuted yet: Bars often starts
/// at login before the apps it muted do.
const RECOVER_EVERY: Duration = Duration::from_secs(15);
const RECOVER_FOR: Duration = Duration::from_secs(10 * 60);

/// Something muted, as written down in case Bars doesn't get to unmute it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Muted {
    /// The output device or app, as the platform names it.
    pub id: String,
    /// Turned down to nothing from this volume, for an output without a
    /// mute control (macOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
}

/// A platform's way of muting.
pub(crate) trait Backend {
    /// Mutes what isn't muted yet (but not Bars, nor what it muted before
    /// since the last unmute). Returns what it muted.
    fn mute(&mut self) -> Vec<Muted>;
    /// Unmutes what it muted, where it's still muted.
    fn unmute(&mut self);
    /// Unmutes what an earlier run muted and didn't unmute, where it's
    /// still muted. Returns what it couldn't get to yet (an app that isn't
    /// running), to be tried again.
    fn recover(&mut self, muted: &[Muted]) -> Vec<Muted>;
    /// Gets ready to mute: asks for any permission that takes (macOS), so
    /// the question comes when muting is turned on, not in a dictation.
    fn prepare(&mut self) {}
}

fn backend() -> Option<Box<dyn Backend>> {
    #[cfg(target_os = "macos")]
    return Some(Box::new(super::mac::mute::Output::default()));
    #[cfg(target_os = "windows")]
    return super::win::mute::Sessions::new().map(|b| Box::new(b) as Box<dyn Backend>);
    #[cfg(target_os = "linux")]
    return Some(Box::new(super::linux::mute::Streams::default()));
    #[allow(unreachable_code)]
    None
}

enum Cmd {
    Prepare,
    Mute { after: Duration },
    Restore(Sender<()>),
}

/// Mutes other audio on its own thread.
pub struct Muter {
    tx: Sender<Cmd>,
}

impl Muter {
    /// Also turns back on what a run that didn't finish left muted.
    pub fn spawn() -> Muter {
        let (tx, rx) = unbounded();
        let file = crate::paths::data_dir().join("muted.json");
        thread::Builder::new()
            .name("bars-mute".into())
            .spawn(move || run(rx, file))
            .expect("spawn mute thread");
        Muter { tx }
    }

    /// Gets ready to mute (asking macOS for permission, if it takes that).
    pub fn prepare(&self) {
        let _ = self.tx.send(Cmd::Prepare);
    }

    /// Mutes other audio, `after` this long (a sound playing first).
    pub fn mute(&self, after: Duration) {
        let _ = self.tx.send(Cmd::Mute { after });
    }

    /// Turns it back on. Returns once it's on (or after a moment), so a
    /// sound played next is heard.
    pub fn restore(&self) {
        let (tx, rx) = bounded(1);
        if self.tx.send(Cmd::Restore(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_millis(500));
        }
    }
}

fn run(rx: Receiver<Cmd>, file: PathBuf) {
    match backend() {
        Some(backend) => run_with(rx, &file, backend, RECOVER_EVERY),
        None => {
            for cmd in rx {
                if let Cmd::Restore(done) = cmd {
                    let _ = done.send(());
                }
            }
        }
    }
}

fn run_with(rx: Receiver<Cmd>, file: &Path, mut backend: Box<dyn Backend>, recover_every: Duration) {
    // What an earlier run left muted. Whatever can't be unmuted yet stays
    // written down (with what this run mutes) and is tried again: for a
    // while now, and the next time Bars starts.
    let mut left = read(file).unwrap_or_default();
    let started = Instant::now();
    if !left.is_empty() {
        log::info!("unmuting what an earlier run left muted: {left:?}");
        left = backend.recover(&left);
        note(file, &[], &left);
    }
    let mut retry = (!left.is_empty()).then(|| Instant::now() + recover_every);
    let mut muted: Vec<Muted> = Vec::new();
    // While muting: when to next mute what plays.
    let mut due: Option<Instant> = None;
    loop {
        let cmd = match [due, retry].into_iter().flatten().min() {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match cmd {
            Ok(Cmd::Prepare) => {
                if due.is_none() {
                    backend.prepare();
                }
            }
            Ok(Cmd::Mute { after }) => {
                due = Some(Instant::now() + after);
            }
            Ok(Cmd::Restore(done)) => {
                if due.take().is_some() {
                    backend.unmute();
                    if !muted.is_empty() {
                        log::info!("unmuted other audio");
                        muted.clear();
                        note(file, &muted, &left);
                    }
                }
                let _ = done.send(());
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if due.is_some_and(|at| now >= at) {
                    let new = backend.mute();
                    if !new.is_empty() {
                        log::info!("muted {new:?}");
                        muted.extend(new);
                        note(file, &muted, &left);
                    }
                    due = Some(now + REFRESH_EVERY);
                }
                if retry.is_some_and(|at| now >= at) {
                    // Not while a dictation has other audio muted: what
                    // comes back on would play over it.
                    if due.is_none() {
                        let still = backend.recover(&left);
                        if still != left {
                            log::info!("unmuted what an earlier run left muted; still to go: {still:?}");
                            left = still;
                            note(file, &muted, &left);
                        }
                    }
                    retry = (!left.is_empty() && started.elapsed() < RECOVER_FOR).then(|| now + recover_every);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if due.is_some() {
                    backend.unmute();
                    note(file, &[], &left);
                }
                return;
            }
        }
    }
}

fn read(file: &Path) -> Option<Vec<Muted>> {
    let text = std::fs::read_to_string(file).ok()?;
    serde_json::from_str(&text).map_err(|e| log::warn!("{}: {e}", file.display())).ok()
}

/// Writes down what's muted now and what an earlier run left muted, or
/// that nothing is.
fn note(file: &Path, muted: &[Muted], left: &[Muted]) {
    if muted.is_empty() && left.is_empty() {
        let _ = std::fs::remove_file(file);
    } else {
        write(file, &[muted, left].concat());
    }
}

fn write(file: &Path, muted: &[Muted]) {
    let result = serde_json::to_string(muted).map_err(std::io::Error::other).and_then(|json| {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, json)
    });
    if let Err(e) = result {
        log::warn!("couldn't note what's muted in {}: {e}", file.display());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Two apps, one muted already; records what happens to them.
    #[derive(Clone, Default)]
    struct Fake {
        apps: Arc<Mutex<Vec<(String, bool)>>>,
        ours: Arc<Mutex<Vec<String>>>,
    }

    impl Backend for Fake {
        fn mute(&mut self) -> Vec<Muted> {
            let mut apps = self.apps.lock().unwrap();
            let mut ours = self.ours.lock().unwrap();
            let mut new = Vec::new();
            for (id, muted) in apps.iter_mut() {
                if !*muted && !ours.contains(id) {
                    *muted = true;
                    ours.push(id.clone());
                    new.push(Muted { id: id.clone(), volume: None });
                }
            }
            new
        }

        fn unmute(&mut self) {
            let mut apps = self.apps.lock().unwrap();
            for id in self.ours.lock().unwrap().drain(..) {
                if let Some(app) = apps.iter_mut().find(|(a, _)| *a == id) {
                    app.1 = false;
                }
            }
        }

        fn recover(&mut self, muted: &[Muted]) -> Vec<Muted> {
            let mut apps = self.apps.lock().unwrap();
            for app in apps.iter_mut().filter(|(a, _)| muted.iter().any(|m| m.id == *a)) {
                app.1 = false;
            }
            muted.iter().filter(|m| !apps.iter().any(|(a, _)| *a == m.id)).cloned().collect()
        }
    }

    fn state(fake: &Fake) -> Vec<(String, bool)> {
        fake.apps.lock().unwrap().clone()
    }

    #[test]
    fn mutes_what_plays_and_unmutes_only_that() {
        let fake = Fake::default();
        *fake.apps.lock().unwrap() = vec![("music".into(), false), ("muted already".into(), true)];
        let file = std::env::temp_dir().join(format!("bars-mute-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&file);
        let (tx, rx) = unbounded();
        let backend = fake.clone();
        let worker = thread::spawn({
            let file = file.clone();
            move || run_with(rx, &file, Box::new(backend), RECOVER_EVERY)
        });
        let muter = Muter { tx };

        muter.mute(Duration::from_millis(50));
        thread::sleep(Duration::from_millis(20));
        assert!(!state(&fake)[0].1, "not before the sound has played");
        thread::sleep(Duration::from_millis(100));
        assert_eq!(state(&fake), [("music".into(), true), ("muted already".into(), true)]);
        assert_eq!(read(&file), Some(vec![Muted { id: "music".into(), volume: None }]));
        // An app that starts playing meanwhile is muted too.
        fake.apps.lock().unwrap().push(("video".into(), false));
        thread::sleep(REFRESH_EVERY + Duration::from_millis(100));
        assert!(state(&fake)[2].1);

        muter.restore();
        assert_eq!(state(&fake), [("music".into(), false), ("muted already".into(), true), ("video".into(), false)]);
        assert_eq!(read(&file), None, "nothing left to unmute");
        // Restoring with nothing muted does nothing.
        muter.restore();
        drop(muter);
        worker.join().unwrap();
    }

    #[test]
    fn a_run_that_didnt_finish_is_undone() {
        let fake = Fake::default();
        *fake.apps.lock().unwrap() = vec![("music".into(), true), ("muted by the user".into(), true)];
        let file = std::env::temp_dir().join(format!("bars-mute-crash-{}.json", std::process::id()));
        write(&file, &[Muted { id: "music".into(), volume: None }]);
        let (tx, rx) = unbounded::<Cmd>();
        drop(tx);
        run_with(rx, &file, Box::new(fake.clone()), RECOVER_EVERY);
        assert_eq!(state(&fake), [("music".into(), false), ("muted by the user".into(), true)]);
        assert!(!file.exists());
    }

    #[test]
    fn an_app_that_isnt_running_yet_is_unmuted_once_it_is() {
        let fake = Fake::default();
        *fake.apps.lock().unwrap() = vec![("music".into(), true)];
        let file = std::env::temp_dir().join(format!("bars-mute-later-{}.json", std::process::id()));
        let later = Muted { id: "browser".into(), volume: None };
        write(&file, &[Muted { id: "music".into(), volume: None }, later.clone()]);
        let (tx, rx) = unbounded::<Cmd>();
        let worker = thread::spawn({
            let (file, backend) = (file.clone(), fake.clone());
            move || run_with(rx, &file, Box::new(backend), Duration::from_millis(50))
        });
        thread::sleep(Duration::from_millis(120));
        // What's running is unmuted; the rest stays written down.
        assert_eq!(state(&fake), [("music".into(), false)]);
        assert_eq!(read(&file), Some(vec![later]));
        // The browser starts, still muted from before.
        fake.apps.lock().unwrap().push(("browser".into(), true));
        thread::sleep(Duration::from_millis(150));
        assert_eq!(state(&fake)[1], ("browser".into(), false));
        assert!(!file.exists());
        drop(tx);
        worker.join().unwrap();
    }
}
