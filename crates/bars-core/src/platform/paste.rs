//! Pasting with proof. The text goes on the clipboard as a promise that is
//! only fulfilled when an app reads it, so a read means the paste shortcut
//! reached an app that took the text. No read within a second means it
//! landed nowhere (no text field had focus): the caller tells the user and
//! offers the text to copy. The previous clipboard comes back right after
//! the read, rather than after a guessed delay.

use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;

/// A paste that no app read within this long didn't land.
pub const READ_TIMEOUT: Duration = Duration::from_millis(1000);
/// After reporting a paste as lost, keep offering the text this much
/// longer: a slow app may still read it, or the user may click into a text
/// field and paste by hand.
pub const LATE_READ: Duration = Duration::from_millis(2500);
/// The app reads first and inserts after; give it this long before the old
/// clipboard returns.
pub const RESTORE_AFTER_READ: Duration = Duration::from_millis(500);
/// Between the pieces of a split paste.
pub const PIECE_GAP: Duration = Duration::from_millis(50);

/// A clipboard that can hand out text on demand.
pub trait LazyClipboard {
    type Saved;
    /// Every paste asks the source again (Wayland). Elsewhere the system
    /// keeps the text after the first read, so a read just before the
    /// shortcut may be the only one the paste causes.
    const ASKS_EVERY_PASTE: bool;
    /// Everything on the clipboard now, to put back afterwards.
    fn save(&mut self) -> Option<Self::Saved>;
    /// Offers `text`, produced only when an app asks for it. The receiver
    /// gets a message on every read. Kept from clipboard-history apps.
    fn offer(&mut self, text: &str) -> anyhow::Result<Receiver<()>>;
    /// Our offer is still on the clipboard (nobody copied since).
    fn still_ours(&mut self) -> bool;
    fn restore(&mut self, saved: Self::Saved);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pasted {
    /// An app read the text.
    Landed,
    /// Nothing read it in time.
    NotRead,
}

/// Pastes `pieces` one after another (`press` sends the paste shortcut),
/// waiting for each to be read. Stops at the first piece nothing read and
/// calls `on_unread` as soon as that's known.
pub fn paste_pieces<C: LazyClipboard>(
    clip: &mut C,
    pieces: &[String],
    restore: bool,
    mut press: impl FnMut() -> anyhow::Result<()>,
    on_unread: &mut dyn FnMut(),
) -> anyhow::Result<Pasted> {
    let saved = if restore { clip.save() } else { None };
    let mut outcome = Pasted::Landed;
    let result = (|| {
        for (i, piece) in pieces.iter().enumerate() {
            if i > 0 {
                thread::sleep(PIECE_GAP);
            }
            let reads = clip.offer(piece)?;
            if C::ASKS_EVERY_PASTE {
                // Reads before the shortcut (a clipboard manager that
                // ignores the hint) aren't the paste.
                while reads.try_recv().is_ok() {}
            }
            press()?;
            if reads.recv_timeout(READ_TIMEOUT).is_err() {
                outcome = Pasted::NotRead;
                on_unread();
                let _ = reads.recv_timeout(LATE_READ);
                return Ok(());
            }
        }
        thread::sleep(RESTORE_AFTER_READ);
        Ok(())
    })();
    if let Some(saved) = saved
        && clip.still_ours()
    {
        clip.restore(saved);
    }
    result.map(|()| outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{Sender, unbounded};

    #[derive(Default)]
    struct Fake {
        content: String,
        offered: Vec<String>,
        reader: Option<Sender<()>>,
        reads_ok: bool,
        restored: Option<String>,
    }

    impl LazyClipboard for Fake {
        type Saved = String;
        const ASKS_EVERY_PASTE: bool = true;
        fn save(&mut self) -> Option<String> {
            Some(self.content.clone())
        }
        fn offer(&mut self, text: &str) -> anyhow::Result<Receiver<()>> {
            self.offered.push(text.to_string());
            let (tx, rx) = unbounded();
            self.reader = Some(tx);
            Ok(rx)
        }
        fn still_ours(&mut self) -> bool {
            true
        }
        fn restore(&mut self, saved: String) {
            self.restored = Some(saved);
        }
    }

    impl Fake {
        fn press(&mut self) {
            if self.reads_ok {
                let _ = self.reader.as_ref().unwrap().send(());
            }
        }
    }

    #[test]
    fn pastes_every_piece_then_restores() {
        let fake = std::cell::RefCell::new(Fake { content: "old".into(), reads_ok: true, ..Default::default() });
        let pieces = vec!["a".to_string(), "b".to_string()];
        let mut lost = false;
        let mut clip = FakeRef(&fake);
        let out = paste_pieces(&mut clip, &pieces, true, || {
            fake.borrow_mut().press();
            Ok(())
        }, &mut || lost = true);
        assert_eq!(out.unwrap(), Pasted::Landed);
        assert!(!lost);
        assert_eq!(fake.borrow().offered, pieces);
        assert_eq!(fake.borrow().restored.as_deref(), Some("old"));
    }

    // Lets the test press keys while the clipboard is borrowed.
    struct FakeRef<'a>(&'a std::cell::RefCell<Fake>);
    impl LazyClipboard for FakeRef<'_> {
        type Saved = String;
        const ASKS_EVERY_PASTE: bool = true;
        fn save(&mut self) -> Option<String> {
            self.0.borrow_mut().save()
        }
        fn offer(&mut self, text: &str) -> anyhow::Result<Receiver<()>> {
            self.0.borrow_mut().offer(text)
        }
        fn still_ours(&mut self) -> bool {
            true
        }
        fn restore(&mut self, saved: String) {
            self.0.borrow_mut().restore(saved)
        }
    }
}
