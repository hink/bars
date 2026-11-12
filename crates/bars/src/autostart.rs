//! Launch at login. Native installs write an XDG autostart entry; inside
//! Flatpak the Background portal does it on our behalf.

use bars_core::paths;

const FILE: &str = "xyz.hink.Bars.desktop";

pub fn sync(enabled: bool) {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        native::sync(enabled);
        return;
    }
    #[allow(unreachable_code)]
    if paths::is_flatpak() {
        std::thread::spawn(move || {
            if let Err(e) = smol::block_on(request_portal(enabled)) {
                log::warn!("background portal: {e:#}");
            }
        });
        return;
    }
    let Some(dir) = dirs_autostart() else { return };
    let path = dir.join(FILE);
    // The path Bars started from: once an update has replaced the binary,
    // asking again would give "… (deleted)".
    let exe_path = bars_core::paths::exe();
    // Never register a development build running out of a cargo target dir.
    if exe_path.is_some_and(is_dev_build) {
        log::debug!("autostart: skipping for development build");
        return;
    }
    if enabled {
        let exe = exe_path
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "bars".into());
        // AppImage: point at the image, not the mounted binary.
        let exe = std::env::var("APPIMAGE").unwrap_or(exe);
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Bars\nComment=Voice dictation\nExec=\"{exe}\" --background\nIcon=xyz.hink.Bars\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n"
        );
        let _ = std::fs::create_dir_all(&dir);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str())
            && let Err(e) = std::fs::write(&path, entry)
        {
            log::warn!("autostart: {e}");
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn dirs_autostart() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .map(|c| c.join("autostart"))
}

#[cfg(target_os = "linux")]
async fn request_portal(enabled: bool) -> anyhow::Result<()> {
    use ashpd::desktop::background::Background;
    Background::request()
        .reason("Start Bars at login so dictation is always one shortcut away")
        .auto_start(enabled)
        .command(["bars", "--background"])
        .dbus_activatable(false)
        .send()
        .await?
        .response()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
async fn request_portal(_: bool) -> anyhow::Result<()> {
    Ok(())
}

/// Development builds (run from a cargo `target` dir) never register.
fn is_dev_build(exe: &std::path::Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "target")
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod native {
    pub fn sync(enabled: bool) {
        let Some(exe) = bars_core::paths::exe() else { return };
        if super::is_dev_build(exe) {
            log::debug!("autostart: skipping for development build");
            return;
        }
        if let Err(e) = set(enabled, exe) {
            log::warn!("autostart: {e:#}");
        }
    }

    /// HKCU\...\Run: per-user, no admin rights needed.
    #[cfg(target_os = "windows")]
    fn set(enabled: bool, exe: &std::path::Path) -> anyhow::Result<()> {
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
        use windows::core::{HSTRING, PCWSTR};
        let key = HSTRING::from(r"Software\Microsoft\Windows\CurrentVersion\Run");
        let name = HSTRING::from("Bars");
        unsafe {
            if enabled {
                let cmd: Vec<u16> = format!("\"{}\" --background", exe.display())
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key.as_ptr()),
                    PCWSTR(name.as_ptr()),
                    REG_SZ.0,
                    Some(cmd.as_ptr().cast()),
                    (cmd.len() * 2) as u32,
                )
                .ok()?;
            } else {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()));
            }
        }
        Ok(())
    }

    /// A per-user LaunchAgent that starts Bars at login.
    #[cfg(target_os = "macos")]
    fn set(enabled: bool, exe: &std::path::Path) -> anyhow::Result<()> {
        let dir = dirs_home().join("Library/LaunchAgents");
        let path = dir.join("xyz.hink.Bars.plist");
        if !enabled {
            let _ = std::fs::remove_file(path);
            return Ok(());
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>xyz.hink.Bars</string>
    <key>ProgramArguments</key>
    <array><string>{}</string><string>--background</string></array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#,
            exe.display()
        );
        std::fs::create_dir_all(&dir)?;
        if std::fs::read_to_string(&path).ok().as_deref() != Some(plist.as_str()) {
            std::fs::write(&path, plist)?;
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn dirs_home() -> std::path::PathBuf {
        std::env::var_os("HOME").map(Into::into).unwrap_or_default()
    }
}
