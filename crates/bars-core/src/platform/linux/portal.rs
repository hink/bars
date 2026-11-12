//! XDG desktop portal backends. These need no special permissions, so they
//! are what a sandboxed (Flatpak) install uses until the user opts into
//! direct device access:
//!
//! * GlobalShortcuts: the desktop owns the key binding and tells us when the
//!   shortcut is pressed and released (hold-to-talk works).
//! * RemoteDesktop: lets us send key presses (paste shortcut or typed text)
//!   after a one-time consent prompt; a restore token avoids re-prompting.

use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::remote_desktop::{DeviceType, KeyState, NotifyKeyboardKeysymOptions, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode};
use crossbeam_channel::Sender;
use futures_util::StreamExt as _;

use crate::hotkey::{Chord, HotkeyEvent, Key, Mods};
use crate::paths;

const SHORTCUT_ID: &str = "dictate";

/// Converts a chord to the XDG shortcuts-spec trigger format
/// (e.g. `CTRL+LOGO+space`). Modifier-only chords (one side's modifier
/// too) get `space` appended, since desktops can't bind bare modifiers.
pub fn trigger_for(chord: &Chord) -> String {
    let (mods, key) = match chord.key.as_ref().and_then(Key::modifier) {
        Some(m) => (chord.mods | m, None),
        None => (chord.mods, chord.key.as_ref()),
    };
    let mut parts = Vec::new();
    if mods.contains(Mods::CTRL) {
        parts.push("CTRL".to_string());
    }
    if mods.contains(Mods::ALT) {
        parts.push("ALT".to_string());
    }
    if mods.contains(Mods::SHIFT) {
        parts.push("SHIFT".to_string());
    }
    if mods.contains(Mods::SUPER) {
        parts.push("LOGO".to_string());
    }
    let key = key.map(|k| keysym_name(&k.0)).unwrap_or_else(|| "space".into());
    parts.push(key);
    parts.join("+")
}

fn keysym_name(canonical: &str) -> String {
    match canonical {
        "Space" => "space",
        "Enter" => "Return",
        "Backspace" => "BackSpace",
        "Pageup" => "Prior",
        "Pagedown" => "Next",
        "Grave" => "grave",
        "Minus" => "minus",
        "Equal" => "equal",
        "Leftbracket" => "bracketleft",
        "Rightbracket" => "bracketright",
        "Backslash" => "backslash",
        "Semicolon" => "semicolon",
        "Apostrophe" => "apostrophe",
        "Comma" => "comma",
        "Period" => "period",
        "Slash" => "slash",
        "Capslock" => "Caps_Lock",
        "Scrolllock" => "Scroll_Lock",
        other if other.len() == 1 => return other.to_ascii_lowercase(),
        other => other,
    }
    .to_string()
}

enum ShortcutCmd {
    Configure,
}

pub struct PortalHotkeys {
    cmd: async_channel::Sender<ShortcutCmd>,
}

impl PortalHotkeys {
    /// Connects to the GlobalShortcuts portal and binds the dictation
    /// shortcut. Returns an error quickly if the portal isn't available.
    pub fn spawn(chord: Chord, events: Sender<HotkeyEvent>) -> anyhow::Result<Self> {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<anyhow::Result<()>>(1);
        let (cmd_tx, cmd_rx) = async_channel::unbounded::<ShortcutCmd>();
        thread::Builder::new()
            .name("bars-portal-keys".into())
            .spawn(move || {
                async_io::block_on(async move {
                    let proxy = match GlobalShortcuts::new().await {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = ready_tx.send(Err(anyhow!("GlobalShortcuts portal unavailable: {e}")));
                            return;
                        }
                    };
                    let session = match proxy.create_session(CreateSessionOptions::default()).await {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = ready_tx.send(Err(anyhow!("could not create shortcut session: {e}")));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));

                    // Binding may show a desktop dialog; don't block startup on it.
                    let trigger = trigger_for(&chord);
                    let shortcut = NewShortcut::new(SHORTCUT_ID, "Dictate (hold to talk)")
                        .preferred_trigger(trigger.as_str());
                    match proxy
                        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
                        .await
                        .and_then(|r| r.response())
                    {
                        Ok(bound) => {
                            let desc = bound
                                .shortcuts()
                                .iter()
                                .find(|s| s.id() == SHORTCUT_ID)
                                .map(|s| s.trigger_description().to_string())
                                .unwrap_or_default();
                            log::info!("portal shortcut bound: {desc:?}");
                            let _ = events.send(HotkeyEvent::Bound(desc));
                        }
                        Err(e) => {
                            log::warn!("binding portal shortcut failed: {e}");
                        }
                    }

                    let (Ok(activated), Ok(deactivated)) =
                        (proxy.receive_activated().await, proxy.receive_deactivated().await)
                    else {
                        log::error!("could not subscribe to shortcut signals");
                        return;
                    };
                    enum Ev {
                        Down,
                        Up,
                        Cmd(ShortcutCmd),
                        /// The owning `PortalHotkeys` was dropped.
                        Stop,
                    }
                    let activated = activated.filter(|a| futures_util::future::ready(a.shortcut_id() == SHORTCUT_ID)).map(|_| Ev::Down);
                    let deactivated =
                        deactivated.filter(|a| futures_util::future::ready(a.shortcut_id() == SHORTCUT_ID)).map(|_| Ev::Up);
                    let cmds = cmd_rx.map(Ev::Cmd).chain(futures_util::stream::once(async { Ev::Stop }));
                    let all = futures_util::stream::select(futures_util::stream::select(activated, deactivated), cmds);
                    let mut all = std::pin::pin!(all);
                    while let Some(ev) = all.next().await {
                        match ev {
                            Ev::Down => {
                                let _ = events.send(HotkeyEvent::Down);
                            }
                            Ev::Up => {
                                let _ = events.send(HotkeyEvent::Up);
                            }
                            Ev::Stop => {
                                let _ = session.close().await;
                                log::debug!("portal shortcut session closed");
                                return;
                            }
                            Ev::Cmd(ShortcutCmd::Configure) => {
                                if let Err(e) = proxy
                                    .configure_shortcuts(&session, None, ConfigureShortcutsOptions::default())
                                    .await
                                {
                                    log::warn!("could not open shortcut settings: {e}");
                                }
                            }
                        }
                    }
                });
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("GlobalShortcuts portal did not respond"))??;
        Ok(PortalHotkeys { cmd: cmd_tx })
    }

    /// Opens the desktop's shortcut configuration for our binding.
    pub fn configure(&self) {
        let _ = self.cmd.try_send(ShortcutCmd::Configure);
    }
}

// ---------------------------------------------------------------------------

/// Key symbols to send (and whether each is pressed or released), and
/// where to say how it went.
type Keystrokes = (Vec<(u32, bool)>, crossbeam_channel::Sender<anyhow::Result<()>>);

pub struct PortalKeyboard {
    tx: async_channel::Sender<Keystrokes>,
}

fn token_file() -> std::path::PathBuf {
    paths::data_dir().join("remote-desktop.token")
}

impl PortalKeyboard {
    /// Starts (lazily, on first use) a RemoteDesktop session for the keyboard.
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded::<Keystrokes>();
        thread::Builder::new()
            .name("bars-portal-kbd".into())
            .spawn(move || {
                async_io::block_on(async move {
                    let mut session = None;
                    while let Ok((keys, reply)) = rx.recv().await {
                        if session.is_none() {
                            match start_session().await {
                                Ok(s) => session = Some(s),
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                    continue;
                                }
                            }
                        }
                        let (proxy, sess) = session.as_ref().expect("session started");
                        let mut result = Ok(());
                        for (sym, down) in keys {
                            let state = if down { KeyState::Pressed } else { KeyState::Released };
                            if let Err(e) = proxy
                                .notify_keyboard_keysym(sess, sym as i32, state, NotifyKeyboardKeysymOptions::default())
                                .await
                            {
                                result = Err(anyhow!("sending key failed: {e}"));
                                // The session may have been revoked; start over next time.
                                session = None;
                                break;
                            }
                            async_io::Timer::after(Duration::from_millis(2)).await;
                        }
                        let _ = reply.send(result);
                    }
                });
            })
            .expect("spawn portal keyboard thread");
        PortalKeyboard { tx }
    }

    /// Sends keysym press/release events and waits for completion.
    pub fn send(&self, keys: Vec<(u32, bool)>) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = crossbeam_channel::bounded(1);
        self.tx
            .send_blocking((keys, reply_tx))
            .map_err(|_| anyhow!("portal keyboard thread stopped"))?;
        // First use may wait on the user's consent dialog.
        reply_rx
            .recv_timeout(Duration::from_secs(120))
            .map_err(|_| anyhow!("timed out waiting for remote-control permission"))?
    }
}

impl Default for PortalKeyboard {
    fn default() -> Self {
        Self::new()
    }
}

async fn start_session() -> anyhow::Result<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>)> {
    let proxy = RemoteDesktop::new().await.context("RemoteDesktop portal unavailable")?;
    let session = proxy.create_session(CreateSessionOptions::default()).await?;
    let token = std::fs::read_to_string(token_file()).ok();
    proxy
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(ashpd::enumflags2::BitFlags::from(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?
        .response()?;
    let selected = proxy
        .start(&session, None, StartOptions::default())
        .await?
        .response()
        .context("remote-control permission was denied")?;
    if let Some(token) = selected.restore_token() {
        let _ = std::fs::write(token_file(), token);
    }
    Ok((proxy, session))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggers() {
        assert_eq!(trigger_for(&"Ctrl+Super".parse().unwrap()), "CTRL+LOGO+space");
        assert_eq!(trigger_for(&"Alt+Shift+F9".parse().unwrap()), "ALT+SHIFT+F9");
        assert_eq!(trigger_for(&"Ctrl+Grave".parse().unwrap()), "CTRL+grave");
        assert_eq!(trigger_for(&"Super+H".parse().unwrap()), "LOGO+h");
        assert_eq!(trigger_for(&"Rightalt".parse().unwrap()), "ALT+space");
        assert_eq!(trigger_for(&"F9".parse().unwrap()), "F9");
    }
}
