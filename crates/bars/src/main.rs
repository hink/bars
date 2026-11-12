//! Bars: local voice dictation that types into any app.

// Windows: a GUI app, so launching it doesn't open a console window.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod autostart;
mod brand;
#[cfg(target_os = "linux")]
#[path = "ipc_dbus.rs"]
mod ipc;
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[path = "ipc_local.rs"]
mod ipc;
mod onboarding;
mod overlay;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod overlay_native;
mod settings;
mod shell;
mod state;
mod theme;
#[cfg(target_os = "linux")]
mod tray;
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[path = "tray_native.rs"]
mod tray;
mod widgets;
mod wording;

use std::borrow::Cow;

use gpui_kit::*;
use bars_core::config::Config;
use bars_core::{controller, paths};

use crate::shell::{Launch, Shell, ShellCommand};
use crate::state::AppState;

gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        House, Keyboard, AudioLines, BookA, ListClock, ShieldCheck, X, Check, Copy, Trash, Sparkles,
        ArrowRight, Search, ChevronsUpDown, CircleCheck, CircleAlert, Zap, Mic, Pencil, Plus
    ]
);

struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = brand::load_asset(path) {
            return Ok(Some(bytes));
        }
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

const USAGE: &str =
    "Usage: bars [--background | --toggle | --cancel | --paste-last | --settings | --update | --quit | --doctor | --version]";

#[cfg(target_os = "windows")]
fn single_instance_windows() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::w;
    // The handle is intentionally leaked: it marks this process as the
    // running instance until it exits.
    match unsafe { CreateMutexW(None, true, w!("Local\\xyz.hink.Bars")) } {
        Ok(_) => (unsafe { GetLastError() }) != ERROR_ALREADY_EXISTS,
        Err(_) => true,
    }
}

/// ~/Library/Logs/Bars.log (Console shows it), with the one before it kept
/// as Bars.old.log once it gets big.
#[cfg(target_os = "macos")]
fn mac_log_file() -> Option<std::fs::File> {
    let dir = std::path::PathBuf::from(std::env::var_os("HOME")?).join("Library/Logs");
    let path = dir.join("Bars.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::rename(&path, dir.join("Bars.old.log"));
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Logs to the file and, for a Bars started from a terminal, stderr.
#[cfg(target_os = "macos")]
struct Tee(std::fs::File);

#[cfg(target_os = "macos")]
impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        self.0.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn main() {
    // Where Bars started from, read before an update can replace it.
    bars_core::paths::exe();
    // Windows: still print to the terminal Bars was started from, if any
    // (`bars --doctor`, errors, logs).
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,ort=warn,naga=warn,wgpu=warn,blade_graphics=warn,transcribe_rs=warn,zbus=error,tracing::span=off,pulseaudio::client::reactor=off"));
    // macOS: an app opened from Finder has no terminal to log to.
    #[cfg(target_os = "macos")]
    if let Some(file) = mac_log_file() {
        logger.target(env_logger::Target::Pipe(Box::new(Tee(file))));
    }
    logger.init();
    // A panic on a worker thread is otherwise only printed, which an app
    // started from a launcher never shows: it goes to the log as well.
    let print_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("{info}");
        print_panic(info);
    }));
    // Still before any other thread exists (the logger starts none).
    bars_core::audio::prepare_environment();

    let arg = std::env::args().nth(1);
    let remote = match arg.as_deref() {
        None | Some("--background") => None,
        Some("--toggle") => Some("Toggle"),
        Some("--cancel") => Some("Cancel"),
        Some("--paste-last") => Some("PasteLast"),
        Some("--settings") => Some("ShowSettings"),
        Some("--update") => Some("Update"),
        Some("--quit") => Some("Quit"),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            return;
        }
        Some("-V" | "--version") => {
            println!("Bars {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--doctor") => {
            print!("{}", bars_core::platform::doctor_report());
            return;
        }
        Some(other) => {
            eprintln!("unknown argument {other}\n{USAGE}");
            std::process::exit(2);
        }
    };
    let background = arg.as_deref() == Some("--background");

    let (shell_tx, shell_rx) = async_channel::unbounded::<ShellCommand>();

    // Single instance: if Bars is already running, forward and exit.
    #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
    let _instance = match ipc::claim(shell_tx.clone()) {
        Ok(Some(instance)) => Some(instance),
        Ok(None) => {
            let method = remote.unwrap_or("ShowSettings");
            if let Err(e) = ipc::call(method) {
                eprintln!("could not reach the running Bars: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        Err(e) => {
            log::warn!("remote control unavailable ({e:#}); continuing without single-instance support");
            None
        }
    };
    // Windows: also a named mutex, so even a wedged instance can't be doubled
    // (two keyboard hooks would type everything twice).
    #[cfg(target_os = "windows")]
    if !single_instance_windows() {
        let _ = ipc::call(remote.unwrap_or("ShowSettings"));
        return;
    }
    if remote.is_some_and(|m| m != "ShowSettings") {
        eprintln!("Bars isn't running.");
        std::process::exit(1);
    }

    let config = Config::load_or_default(&paths::config_file());
    // Unbounded: the controller's state changes are never dropped (it skips
    // microphone levels itself while the UI is behind).
    let (ui_tx, ui_rx) = async_channel::unbounded();
    let handle = controller::spawn(config.clone(), ui_tx);

    let reopen_tx = shell_tx.clone();
    let app = gpui_kit::application();
    // macOS: a menu-bar app, without a Dock icon or app menu ("headless"
    // here only sets that activation policy; windows still draw).
    #[cfg(target_os = "macos")]
    let app = app.with_windowing(WindowingRequest::Headless);
    app.on_reopen(move |_| {
        let _ = reopen_tx.try_send(ShellCommand::ShowSettings(None));
    });
    app.with_assets(Assets)
        // Bars lives in the tray, so closing windows keeps it running.
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            let fonts: Vec<Cow<'static, [u8]>> = vec![
                Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf")),
                Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Medium.ttf")),
                Cow::Borrowed(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
                Cow::Borrowed(include_bytes!("../assets/fonts/Inter-Bold.ttf")),
            ];
            if let Err(e) = cx.text_system().add_fonts(fonts) {
                log::warn!("could not load bundled fonts: {e}");
            }
            theme::set_pref(config.ui.theme, cx);
            theme::apply(theme::wants_dark(config.ui.theme, cx), cx);
            autostart::sync(config.ui.launch_at_login);

            let state = cx.new(|cx| AppState::new(handle, config.clone(), cx));
            state.update(cx, |s, cx| s.refresh_devices(cx));
            state::pump(state.clone(), ui_rx, cx);

            // Keep autostart in sync with the setting.
            let mut last_autostart = config.ui.launch_at_login;
            cx.observe(&state, move |state, cx| {
                let want = state.read(cx).config.ui.launch_at_login;
                if want != last_autostart {
                    last_autostart = want;
                    autostart::sync(want);
                }
            })
            .detach();

            cx.set_global(shell::ShellSender(shell_tx.clone()));
            let shell = cx.new(|cx| Shell::new(state.clone(), shell_rx, shell_tx.clone(), cx));
            let launch = if background { Launch::Background } else { Launch::Interactive };
            shell.update(cx, |s, cx| s.startup(launch, cx));
            // Dev/test helper: start recording a shortcut (in place of the
            // first) right away.
            if std::env::var_os("BARS_DEV_CAPTURE").is_some() {
                state.update(cx, |s, cx| s.start_capture(Some(0), cx));
            }
            // Keep the shell alive for the life of the app.
            std::mem::forget(shell);
        });
}
