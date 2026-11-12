//! Updates. Bars only tells the user that a newer version is published and
//! installs it when they ask. macOS and Windows replace themselves through
//! cargo-packager's updater. On Linux the release tarball's `bin/bars`
//! replaces this program, or the Flatpak reinstalls itself from the
//! release's bundle, in builds the release workflow made for that
//! (`BARS_PACKAGE`). Other copies (built from source, a distro package)
//! open the release page instead.
//!
//! What's published is read from a small manifest on GitHub Releases,
//! `latest.json`, attached to every release by the release workflow. A
//! draft release is never "latest", so publishing a release is what offers
//! it. Downloads are checked against Bars' own signing key (minisign), not
//! the code signature.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, anyhow};
use semver::Version;
use serde::Deserialize;

/// The newest published release's manifest.
const MANIFEST: &str = "https://github.com/hink/bars/releases/latest/download/latest.json";
/// Where to download by hand.
pub const RELEASE_PAGE: &str = "https://github.com/hink/bars/releases/latest";
/// Public half of the key updates are signed with: the contents of the
/// `.pub` file from `cargo packager signer generate` (minisign key
/// EF7F31A8EDF9A6B4). Empty: updates can't be verified, so none are
/// installed (the release page opens instead).
const PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEVGN0YzMUE4RURGOUE2QjQKUldTMHB2bnRxREYvN3dYcThOUVZqTjVvK2pOYUhBczQ0d2tQNjlFTU1lekNGUUpFYS8wd0MyM0sK";
/// Linux: the release file this build updates itself from, "tarball" or
/// "flatpak", set by the release workflow when it builds it.
#[cfg(target_os = "linux")]
const PACKAGE: Option<&str> = option_env!("BARS_PACKAGE");
#[cfg(not(target_os = "linux"))]
const PACKAGE: Option<&str> = None;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes: Option<String>,
    /// Linux: the file this build updates from, if the release has one for
    /// this architecture.
    file: Option<ReleaseFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct ReleaseFile {
    url: String,
    signature: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    notes: Option<String>,
    /// Linux files by architecture ("x86_64"), then kind ("tarball",
    /// "flatpak"). Not under "platforms", where the macOS and Windows
    /// updater rejects a format it doesn't know.
    #[serde(default)]
    linux: HashMap<String, HashMap<String, ReleaseFile>>,
}

/// `BARS_DEV_UPDATE_MANIFEST` points at another manifest, for testing.
fn manifest_url() -> String {
    std::env::var("BARS_DEV_UPDATE_MANIFEST").unwrap_or_else(|_| MANIFEST.into())
}

/// The latest release, if it's newer than this build.
pub fn check() -> anyhow::Result<Option<Release>> {
    let mut resp = crate::models::http_agent()
        .get(&manifest_url())
        .config()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .call()
        .context("fetching the release manifest")?;
    let body = resp.body_mut().read_to_string().context("reading the release manifest")?;
    let manifest: Manifest = serde_json::from_str(&body).context("reading the release manifest")?;
    Ok(newer(env!("CARGO_PKG_VERSION"), PACKAGE, manifest))
}

/// The manifest's release if it's newer than `current`, with the file a
/// build of the `package` kind updates from.
fn newer(current: &str, package: Option<&str>, mut manifest: Manifest) -> Option<Release> {
    let latest = Version::parse(manifest.version.trim_start_matches('v')).ok()?;
    let current = Version::parse(current).ok()?;
    (latest > current).then(|| Release {
        version: latest.to_string(),
        notes: manifest.notes.filter(|n| !n.trim().is_empty()),
        file: package.and_then(|kind| manifest.linux.remove(std::env::consts::ARCH)?.remove(kind)),
    })
}

impl Release {
    /// Whether this copy can install this release itself; if not, updating
    /// opens the release page.
    pub fn installable(&self) -> bool {
        can_install() && (cfg!(not(target_os = "linux")) || self.file.is_some())
    }
}

/// Whether this copy can install updates itself: an installed app with a
/// key to verify them.
fn can_install() -> bool {
    !PUBLIC_KEY.is_empty() && installed_app()
}

#[cfg(target_os = "macos")]
fn installed_app() -> bool {
    // .../Bars.app/Contents/MacOS/bars
    crate::paths::exe().is_some_and(|exe| {
        exe.parent().is_some_and(|dir| dir.ends_with("Contents/MacOS"))
            && exe.ancestors().nth(3).is_some_and(|app| app.extension().is_some_and(|e| e == "app"))
    })
}

#[cfg(target_os = "windows")]
fn installed_app() -> bool {
    // The installer puts its uninstaller next to bars.exe.
    crate::paths::exe().is_some_and(|exe| exe.with_file_name("uninstall.exe").exists())
}

/// Linux: a build made for updating itself, installed where it can: the
/// tarball's binary in a folder Bars can write to, or the Flatpak installed
/// for this user and allowed to run `flatpak` outside its sandbox. Worked
/// out once.
#[cfg(target_os = "linux")]
fn installed_app() -> bool {
    static INSTALLED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| match PACKAGE {
        Some("tarball") => !crate::paths::is_flatpak() && crate::paths::exe().is_some_and(replaceable),
        Some("flatpak") => {
            // As the real path, which app-path is (/var/home on Silverblue,
            // say, where /home is a link to it).
            let home = dirs::home_dir().and_then(|h| h.canonicalize().ok()).unwrap_or_default();
            std::fs::read_to_string("/.flatpak-info").is_ok_and(|info| can_reinstall(&info, &home))
        }
        _ => false,
    });
    *INSTALLED
}

/// The binary is still where Bars started from, in a folder it can write
/// to (not /usr/bin, say).
#[cfg(target_os = "linux")]
fn replaceable(exe: &Path) -> bool {
    !exe.to_string_lossy().ends_with(" (deleted)")
        && exe.parent().is_some_and(|dir| nix::unistd::access(dir, nix::unistd::AccessFlags::W_OK).is_ok())
}

/// From the sandbox's `/.flatpak-info`: the Flatpak is installed for this
/// user (under `home`), not for everyone, which would take an admin's
/// password, and may talk to Flatpak to run `flatpak` on the host.
#[cfg(target_os = "linux")]
fn can_reinstall(info: &str, home: &Path) -> bool {
    let user_dir = home.join(".local/share/flatpak");
    info.lines().any(|l| l.strip_prefix("app-path=").is_some_and(|p| Path::new(p).starts_with(&user_dir)))
        && info.lines().any(|l| l.trim() == "org.freedesktop.Flatpak=talk")
}

/// Windows: the installer `install` downloaded, until `run_installer`.
#[cfg(target_os = "windows")]
static INSTALLER: std::sync::Mutex<Option<(cargo_packager_updater::Update, Vec<u8>)>> = std::sync::Mutex::new(None);

/// Windows: runs the installer `install` downloaded. It closes Bars and
/// starts the new version, and this process ends in here: call it once
/// nothing is being dictated. It returns only when it failed.
#[cfg(target_os = "windows")]
pub fn run_installer() -> anyhow::Result<()> {
    let waiting = INSTALLER.lock().unwrap_or_else(|e| e.into_inner()).take();
    let (update, installer) = waiting.ok_or_else(|| anyhow!("no update was downloaded"))?;
    update.install(installer)?;
    Ok(())
}

/// Downloads, verifies and installs the latest release, and returns the
/// version installed. macOS: the app is replaced on disk; restart into it
/// with `relaunch_after_exit`. Windows: the installer is only downloaded
/// and checked here, since running it ends this process; `run_installer`
/// does that.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn install() -> anyhow::Result<String> {
    use cargo_packager_updater::{Config, WindowsConfig, WindowsUpdateInstallMode};
    if !can_install() {
        return Err(anyhow!("this copy of Bars can't update itself"));
    }
    let config = Config {
        endpoints: vec![manifest_url().parse()?],
        pubkey: PUBLIC_KEY.into(),
        windows: Some(WindowsConfig {
            installer_args: None,
            install_mode: Some(WindowsUpdateInstallMode::Passive),
        }),
    };
    let current = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let update = cargo_packager_updater::check_update(current, config)?.ok_or_else(|| anyhow!("no newer version"))?;
    log::info!("installing Bars {}", update.version);
    #[cfg(target_os = "macos")]
    update.download_and_install()?;
    let version = update.version.clone();
    #[cfg(target_os = "windows")]
    {
        let installer = update.download()?;
        *INSTALLER.lock().unwrap_or_else(|e| e.into_inner()) = Some((update, installer));
    }
    Ok(version)
}

/// Linux: installs the latest release from the file this build updates
/// from, and returns its version. The tarball's `bin/bars` replaces this
/// program, or the Flatpak is reinstalled from the bundle; either way the
/// new Bars has to start and say it's that version. This one keeps running
/// the old version until it exits; restart into the new one with
/// `relaunch_after_exit`.
#[cfg(target_os = "linux")]
pub fn install() -> anyhow::Result<String> {
    if !can_install() {
        return Err(anyhow!("this copy of Bars can't update itself"));
    }
    // Read again, as on macOS and Windows: since the look that offered it,
    // the release's files may have been replaced, or a newer one published.
    let release = check()?.ok_or_else(|| anyhow!("no newer version"))?;
    let file = release.file.as_ref().ok_or_else(|| anyhow!("no update for this copy of Bars"))?;
    log::info!("installing Bars {}", release.version);
    let data = download(&file.url)?;
    verify(&data, &file.signature, PUBLIC_KEY).context("checking the update's signature")?;
    if crate::paths::is_flatpak() {
        reinstall_flatpak(&data, &release.version).context("reinstalling the Flatpak")?;
    } else {
        let exe = crate::paths::exe().ok_or_else(|| anyhow!("can't find this program"))?;
        replace_from_tarball(&data, exe, &release.version).context("replacing Bars")?;
    }
    Ok(release.version.clone())
}

/// The largest release file accepted; they're about 30 MB.
#[cfg(target_os = "linux")]
const DOWNLOAD_LIMIT: u64 = 256 << 20;

/// Downloads a release file, giving up on a connection that stalls.
#[cfg(target_os = "linux")]
fn download(url: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let mut resp = crate::models::http_agent()
        .get(url)
        .config()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .call()
        .context("downloading the update")?;
    let mut data = Vec::new();
    (resp.body_mut().with_config().limit(DOWNLOAD_LIMIT).reader())
        .read_to_end(&mut data)
        .context("downloading the update")?;
    Ok(data)
}

/// Checks `data` against a signature from `cargo packager signer sign`, as
/// the macOS and Windows updater does: both it and the key are base64 of
/// minisign's text files.
#[cfg(target_os = "linux")]
fn verify(data: &[u8], signature: &str, key: &str) -> anyhow::Result<()> {
    use base64::Engine;
    let text = |b64: &str| -> anyhow::Result<String> {
        Ok(String::from_utf8(base64::engine::general_purpose::STANDARD.decode(b64.trim())?)?)
    };
    let key = minisign_verify::PublicKey::decode(&text(key)?)?;
    let signature = minisign_verify::Signature::decode(&text(signature)?)?;
    key.verify(data, &signature, true)?;
    Ok(())
}

/// Puts the tarball's `bin/bars` in place of `exe`: written next to it,
/// tried, then renamed over it, so it's never half there. The running Bars
/// keeps the old file open until it exits.
#[cfg(target_os = "linux")]
fn replace_from_tarball(data: &[u8], exe: &Path, version: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let new = exe.with_file_name(".bars-update");
    let _ = std::fs::remove_file(&new);
    let result = (|| -> anyhow::Result<()> {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(data));
        let mut found = false;
        for entry in tar.entries()? {
            let mut entry = entry?;
            if !entry.path()?.ends_with("bin/bars") {
                continue;
            }
            anyhow::ensure!(
                entry.header().entry_type().is_file() && entry.size() > 0,
                "the update's bin/bars isn't a program"
            );
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o755).open(&new)?;
            std::io::copy(&mut entry, &mut file)?;
            file.sync_all()?;
            found = true;
            break;
        }
        anyhow::ensure!(found, "the update has no bin/bars");
        check_version(std::process::Command::new(&new).arg("--version"), version)?;
        std::fs::rename(&new, exe)?;
        // So the rename itself survives a crash.
        if let Some(dir) = exe.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    result
}

/// Flatpak: installs the bundle over this Bars with `flatpak` on the host,
/// then checks the installed Bars is `version`. The settings and models in
/// ~/.var/app stay.
#[cfg(target_os = "linux")]
fn reinstall_flatpak(data: &[u8], version: &str) -> anyhow::Result<()> {
    use std::process::Command;
    // Bars' cache folder has the same path on the host.
    let dir = dirs::cache_dir().ok_or_else(|| anyhow!("no cache folder"))?;
    std::fs::create_dir_all(&dir)?;
    let bundle = dir.join("bars-update.flatpak");
    std::fs::write(&bundle, data)?;
    let result = (|| -> anyhow::Result<()> {
        let install = run(
            Command::new("flatpak-spawn")
                .args(["--host", "flatpak", "install", "--user", "--reinstall", "--noninteractive", "-y"])
                .arg(&bundle),
            Duration::from_secs(600),
        )?;
        anyhow::ensure!(
            install.status.success(),
            "flatpak install failed: {}",
            String::from_utf8_lossy(&install.stderr).trim()
        );
        check_version(
            Command::new("flatpak-spawn").args(["--host", "flatpak", "run", crate::paths::APP_ID, "--version"]),
            version,
        )
    })();
    let _ = std::fs::remove_file(&bundle);
    result
}

/// Runs a new Bars with `--version` (it prints that and exits before doing
/// anything else) and checks it says `version`, so one that can't start
/// here (another architecture, a missing library) or is another release
/// isn't kept.
#[cfg(target_os = "linux")]
fn check_version(cmd: &mut std::process::Command, version: &str) -> anyhow::Result<()> {
    let out = run(cmd, Duration::from_secs(30))?;
    let said = String::from_utf8_lossy(&out.stdout);
    let said = said.trim();
    anyhow::ensure!(
        out.status.success() && said == format!("Bars {version}"),
        "the update didn't start as Bars {version} ({said:?})"
    );
    Ok(())
}

/// Runs `cmd` to the end, or stops it after `limit`.
#[cfg(target_os = "linux")]
fn run(cmd: &mut std::process::Command, limit: Duration) -> anyhow::Result<std::process::Output> {
    use nix::sys::signal::{Signal, kill};
    use std::process::Stdio;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // A program written a moment ago (the update's own binary) can be
    // "busy" for an instant more: another thread starting a process just
    // then holds the file open until its own program takes over.
    let mut tries = 0;
    let child = loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 10 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            spawned => break spawned?,
        }
    };
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(out) => Ok(out?),
        Err(_) => {
            let _ = kill(pid, Signal::SIGTERM);
            // One that doesn't listen is killed, rather than left behind.
            if rx.recv_timeout(Duration::from_secs(3)).is_err() {
                let _ = kill(pid, Signal::SIGKILL);
            }
            Err(anyhow!("{:?} didn't finish in {} s", cmd.get_program(), limit.as_secs()))
        }
    }
}

/// macOS and Linux: starts Bars (replaced, perhaps) again once this process
/// has exited. One that hasn't after 10 s is stuck (in the audio system,
/// say) and is killed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn relaunch_after_exit() -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "linux")]
    if crate::paths::is_flatpak() {
        // On the host, where this sandbox's process IDs mean nothing: wait
        // until no instance of Bars' Flatpak is left, then start the one now
        // installed. Detached (setsid -f), as the sandbox lasts as long as a
        // flatpak-spawn in it waits for its command.
        let script = r#"i=0; while flatpak ps --columns=application | grep -qx "$1"; do i=$((i+1)); [ "$i" -gt 50 ] && flatpak kill "$1"; sleep 0.2; done; exec flatpak run "$1""#;
        Command::new("flatpak-spawn")
            .args(["--host", "setsid", "-f", "sh", "-c", script, "sh", crate::paths::APP_ID])
            .stdin(Stdio::null())
            .spawn()?;
        return Ok(());
    }
    let exe = crate::paths::exe().ok_or_else(|| anyhow!("can't find this program"))?;
    #[cfg(target_os = "macos")]
    let (start, target) = (
        r#"open "$2""#,
        exe.ancestors()
            .nth(3)
            .filter(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or_else(|| anyhow!("not inside an app"))?,
    );
    #[cfg(target_os = "linux")]
    let (start, target) = (r#"exec "$2""#, exe);
    let script = format!(r#"i=0; while kill -0 "$1" 2>/dev/null; do i=$((i+1)); [ "$i" -gt 50 ] && kill -9 "$1"; sleep 0.2; done; {start}"#);
    Command::new("/bin/sh")
        .args(["-c", &script, "sh"])
        .arg(std::process::id().to_string())
        .arg(target)
        .stdin(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str) -> Manifest {
        Manifest { version: version.into(), notes: Some(" ".into()), linux: HashMap::new() }
    }

    #[test]
    fn offers_only_newer_versions() {
        assert_eq!(newer("0.3.0", None, manifest("0.3.1")).map(|r| r.version), Some("0.3.1".into()));
        assert_eq!(newer("0.3.0", None, manifest("v0.4.0")).map(|r| r.version), Some("0.4.0".into()));
        assert_eq!(newer("0.3.0", None, manifest("0.3.0")), None);
        assert_eq!(newer("0.3.1", None, manifest("0.3.0")), None);
        assert_eq!(newer("0.3.0", None, manifest("not a version")), None);
        // Blank notes are dropped.
        assert_eq!(newer("0.3.0", None, manifest("0.3.1")).unwrap().notes, None);
    }

    const LATEST: &str = r#"{"version":"0.3.1","notes":"Fixes","pub_date":"2026-10-12T00:00:00Z",
        "platforms":{"macos-aarch64":{"url":"https://example.com/a.tar.gz","signature":"x","format":"app"}},
        "linux":{"x86_64":{"tarball":{"url":"https://example.com/t.tar.gz","signature":"t"},
                           "flatpak":{"url":"https://example.com/b.flatpak","signature":"f"}}}}"#;

    #[test]
    fn reads_the_release_manifest() {
        let release = |package| newer("0.3.0", package, serde_json::from_str(LATEST).unwrap()).unwrap();
        let plain = release(None);
        assert_eq!((plain.version.as_str(), plain.notes.as_deref(), plain.file), ("0.3.1", Some("Fixes"), None));
        let file = |url: &str, signature: &str| Some(ReleaseFile { url: url.into(), signature: signature.into() });
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(release(Some("tarball")).file, file("https://example.com/t.tar.gz", "t"));
            assert_eq!(release(Some("flatpak")).file, file("https://example.com/b.flatpak", "f"));
        }
        assert_eq!(release(Some("appimage")).file, None);
        // Before Linux files were published.
        let old = r#"{"version":"0.3.1","platforms":{}}"#;
        assert_eq!(newer("0.3.0", Some("tarball"), serde_json::from_str(old).unwrap()).unwrap().file, None);
    }

    /// Signed by `cargo packager signer sign`, as the release workflow does,
    /// with a throwaway key.
    #[cfg(target_os = "linux")]
    #[test]
    fn checks_update_signatures() {
        const KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDMwNjZCNDUzOERDODcxMUMKUldRY2NjaU5VN1JtTUl4SGR1QjU0eno2MkZTOWNyWWxSdklTN1Y5eE5ORzVFSXFPUVkvL2pxNDgK";
        const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIGNhcmdvLXBhY2thZ2VyIHNlY3JldCBrZXkKUlVRY2NjaU5VN1JtTUJkWiswMThBSUE1eEVtQjlhWU1wUWcvb0VWZHVzSzVQVzhqeXhxOU1qbUtmcVVQb2hoYk5tVnhuZHhPUEFxdmM2a3hCMFhtd0tPalFuRXJoYXU5cHdvPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzMjQ4MTg4CWZpbGU6cGF5bG9hZAo4d0p5emNVeEV0eDRaU0xTY1N6VW4wNWdUcy9Zc2RHMktQQXAvcjh1Sm1KNWRubS96YXN1aDBGd3VvK0IwMFcwTnQxa1lIaHAwMnFFVVBJcG9jSDZDQT09Cg==";
        assert!(verify(b"Bars update", SIGNATURE, KEY).is_ok());
        assert!(verify(b"Bars updatf", SIGNATURE, KEY).is_err());
        assert!(verify(b"Bars update", SIGNATURE, PUBLIC_KEY).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reinstalls_only_a_user_flatpak_that_may_run_flatpak() {
        let info = |app_path: &str, policy: &str| {
            format!("[Instance]\napp-path={app_path}\nbranch=master\n[Session Bus Policy]\n{policy}\n")
        };
        let home = Path::new("/home/u");
        let user = "/home/u/.local/share/flatpak/app/xyz.hink.Bars/x86_64/master/abc/files";
        let system = "/var/lib/flatpak/app/xyz.hink.Bars/x86_64/master/abc/files";
        assert!(can_reinstall(&info(user, "org.freedesktop.Flatpak=talk"), home));
        assert!(!can_reinstall(&info(system, "org.freedesktop.Flatpak=talk"), home));
        assert!(!can_reinstall(&info(user, "org.kde.StatusNotifierWatcher=talk"), home));
        assert!(!can_reinstall(&info("/home/u/.local/share/flatpak-other/app", "org.freedesktop.Flatpak=talk"), home));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaces_only_a_binary_in_a_folder_it_can_write_to() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("writable");
        let exe = dir.0.join("bars");
        std::fs::write(&exe, "old").unwrap();
        assert!(replaceable(&exe));
        // Already replaced by something else since Bars started.
        assert!(!replaceable(&dir.0.join("bars (deleted)")));
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o555)).unwrap();
        let read_only = replaceable(&exe);
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root can write anywhere.
        let root = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap()) == 0;
        assert!(!read_only || root);
    }

    /// A folder of its own under the system's temp folder, removed after.
    #[cfg(target_os = "linux")]
    struct TempDir(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("bars-update-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A release tarball holding `entries`: (path, contents, symlink target).
    #[cfg(target_os = "linux")]
    fn tarball(entries: &[(&str, &str, Option<&str>)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, contents, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o755);
            match link {
                Some(target) => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    tar.append_link(&mut header, path, target).unwrap();
                }
                None => {
                    header.set_size(contents.len() as u64);
                    tar.append_data(&mut header, path, contents.as_bytes()).unwrap();
                }
            }
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replaces_the_binary_with_the_tarballs() {
        let dir = TempDir::new("replace");
        let exe = dir.0.join("bars");
        std::fs::write(&exe, "old").unwrap();
        // The running copy keeps the old file.
        let running = std::fs::File::open(&exe).unwrap();
        let new = "#!/bin/sh\necho 'Bars 9.9.9'\n";
        let data = tarball(&[("bars-linux-x86_64/README.md", "hi", None), ("bars-linux-x86_64/bin/bars", new, None)]);
        replace_from_tarball(&data, &exe, "9.9.9").unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), new);
        assert!(!dir.0.join(".bars-update").exists());
        let mut old = String::new();
        std::io::Read::read_to_string(&mut &running, &mut old).unwrap();
        assert_eq!(old, "old");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keeps_the_binary_when_the_tarballs_wont_do() {
        let dir = TempDir::new("keep");
        let exe = dir.0.join("bars");
        std::fs::write(&exe, "old").unwrap();
        let cases = [
            tarball(&[("bars-linux-x86_64/README.md", "hi", None)]),
            tarball(&[("bars-linux-x86_64/bin/bars", "", None)]),
            tarball(&[("bars-linux-x86_64/bin/bars", "", Some("/bin/sh"))]),
            // Another release, or one that can't start here.
            tarball(&[("bars-linux-x86_64/bin/bars", "#!/bin/sh\necho 'Bars 9.9.8'\n", None)]),
            tarball(&[("bars-linux-x86_64/bin/bars", "\x7fELF not really", None)]),
            b"not a tarball".to_vec(),
        ];
        for data in cases {
            assert!(replace_from_tarball(&data, &exe, "9.9.9").is_err());
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old");
            assert!(!dir.0.join(".bars-update").exists());
        }
    }
}
