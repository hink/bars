//! Text insertion on Windows and macOS: the clipboard plus the platform's
//! paste shortcut (Ctrl+V / Cmd+V), or typing through synthetic key events.
//! Pasted text is offered lazily, which tells whether an app took it.

#[cfg(target_os = "windows")]
use std::thread;
#[cfg(target_os = "windows")]
use std::time::Duration;

use anyhow::anyhow;
use arboard::Clipboard;
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use super::Inserted;
use super::paste::{Pasted, paste_pieces};
use crate::config::{InsertMethod, InsertionConfig};

/// Runs keyboard simulation where the platform allows it: macOS aborts the
/// app when keyboard layouts (needed to type) are looked up off the main
/// thread; elsewhere any thread works.
fn on_input_thread<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(target_os = "macos")]
    {
        super::mac::main_thread::run(f)
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
}

pub struct NativeInjector {
    enigo: Option<Enigo>,
    error: Option<String>,
    /// The window that had focus when dictation started (Windows).
    #[cfg(target_os = "windows")]
    target: Option<isize>,
    #[cfg(target_os = "macos")]
    clipboard: super::mac::paste::Pasteboard,
    #[cfg(target_os = "windows")]
    clipboard: super::win::clipboard::WinClipboard,
}

impl NativeInjector {
    pub fn new() -> Self {
        let settings = Settings {
            // The setup assistant asks for permissions; don't pop another prompt.
            open_prompt_to_get_permissions: false,
            ..Settings::default()
        };
        let (enigo, error) = match on_input_thread(|| Enigo::new(&settings)) {
            Ok(e) => (Some(e), None),
            Err(e) => {
                log::warn!("keyboard simulation unavailable: {e}");
                (None, Some(e.to_string()))
            }
        };
        NativeInjector {
            enigo,
            error,
            #[cfg(target_os = "windows")]
            target: None,
            clipboard: Default::default(),
        }
    }

    pub fn has_keyboard(&self) -> bool {
        self.enigo.is_some()
    }

    /// Remember where the user is typing, so a click on the overlay can't
    /// steal the paste (Windows activates windows on click).
    pub fn remember_target(&mut self) {
        #[cfg(target_os = "windows")]
        {
            use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
            self.target = Some(unsafe { GetForegroundWindow() }.0 as isize);
        }
    }

    #[cfg(target_os = "windows")]
    fn restore_target(&self) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Threading::GetCurrentProcessId;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
        };
        let Some(target) = self.target else { return };
        unsafe {
            let fg = GetForegroundWindow();
            let mut pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut pid));
            if pid == GetCurrentProcessId() && fg.0 as isize != target {
                let _ = SetForegroundWindow(HWND(target as *mut core::ffi::c_void));
                thread::sleep(Duration::from_millis(80));
            }
        }
    }

    pub fn copy_only(&self, text: &str) -> anyhow::Result<()> {
        Clipboard::new()?.set_text(text.to_string())?;
        Ok(())
    }

    pub fn insert(&mut self, pieces: &[String], cfg: &InsertionConfig, on_unread: &mut dyn FnMut()) -> anyhow::Result<Inserted> {
        let text = pieces.concat();
        if text.is_empty() {
            return Ok(Inserted::Sent);
        }
        #[cfg(target_os = "windows")]
        self.restore_target();
        let Some(enigo) = self.enigo.as_mut() else {
            // Still useful: leave the text on the clipboard.
            Clipboard::new()?.set_text(text)?;
            anyhow::bail!(
                "copied to clipboard ({})",
                self.error.as_deref().unwrap_or("keyboard simulation unavailable")
            );
        };
        if cfg.method == InsertMethod::Type {
            on_input_thread(|| type_text(enigo, &text))?;
            return Ok(Inserted::Sent);
        }
        #[cfg(target_os = "macos")]
        let press = super::mac::paste::press_paste;
        #[cfg(target_os = "windows")]
        let press = || {
            // The V key by its virtual-key code, which is the same on every
            // layout. Asked for as the letter "v", a layout without one
            // (Russian, Greek, Hebrew...) has no key to press, and the
            // letter would be typed instead of the paste.
            const VK_V: u32 = 0x56;
            enigo.key(Key::Control, Direction::Press).map_err(key_err)?;
            let tapped = enigo
                .key(Key::Other(VK_V), Direction::Click)
                .or_else(|_| enigo.key(Key::Unicode('v'), Direction::Click))
                .map_err(key_err);
            enigo.key(Key::Control, Direction::Release).map_err(key_err)?;
            tapped
        };
        let pasted = paste_pieces(&mut self.clipboard, pieces, cfg.restore_clipboard, press, on_unread)?;
        Ok(match pasted {
            Pasted::Landed => Inserted::Landed,
            Pasted::NotRead => Inserted::NotRead,
        })
    }
}

impl Default for NativeInjector {
    fn default() -> Self {
        Self::new()
    }
}

fn key_err(e: enigo::InputError) -> anyhow::Error {
    anyhow!("sending keys failed: {e}")
}

fn type_text(enigo: &mut Enigo, text: &str) -> anyhow::Result<()> {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            // Shift+Enter: a line break that won't "send" in chat apps.
            enigo.key(Key::Shift, Direction::Press).map_err(key_err)?;
            enigo.key(Key::Return, Direction::Click).map_err(key_err)?;
            enigo.key(Key::Shift, Direction::Release).map_err(key_err)?;
        }
        if !line.is_empty() {
            enigo.text(line).map_err(key_err)?;
        }
    }
    Ok(())
}
