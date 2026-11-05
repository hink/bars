<p align="center"><img src="packaging/linux/icons/128x128/xyz.hink.Bars.png" width="96" alt="Bars"></p>

# Bars

Spit some bars! Hold a shortcut, speak, let go: your words appear wherever
your cursor is, in any app. Bars is private, on-device voice dictation, so
nothing leaves your computer.

Bars is built in Rust with [GPUI](https://www.gpui.rs) (Zed's UI framework,
via [GPUI Kit](https://gpui-kit.com)), runs NVIDIA's Parakeet speech models
on the CPU with ONNX Runtime, and polishes English dictations with
Muesli 0.8B on llama.cpp.

## Using it

| Gesture | What happens |
| --- | --- |
| **Hold** `Ctrl` + `Super` | Talk; release to insert the text |
| **Double-tap** `Ctrl` + `Super` | Hands-free: keep talking, tap again to insert |
| `Esc` while recording | Cancel (the app doesn't also get the `Esc`) |
| Tray menu → *Paste last transcript* | Re-insert the last dictation |
| `bars --toggle` | Start/stop from any custom key binding or script |

The shortcut, activation style (hold + double-tap / hold / toggle), microphone,
insertion method, model and more are in the settings window (click the tray
icon). You can add more than one shortcut, all working the same way. A
shortcut can be a modifier on its own, like Right Option or Right Ctrl (only
that side's key counts, so the other one works as usual), a key on its own,
like F9 or Pause (any key that doesn't type or edit text), or a mouse button:
the middle button or a side button, on its own or with modifiers (never left
or right click). On macOS and Windows the key or button then does nothing
else; on Linux it also reaches the app, so pick one the app doesn't use. Say **"new line"** or **"new paragraph"** to break lines; filler words
("um", "uh") are removed; the Dictionary page holds word replacements and
spoken snippets.

*Mute other audio* (off by default, under Microphone) silences music, videos
and other apps while you dictate, and turns them back on when you stop.
Windows and Linux mute each app but Bars, so its sounds still play; macOS
14.2 and later do the same with a Core Audio tap, which takes the System
Audio Recording permission (macOS asks when you turn the option on; Bars
reads the tap only to mute it, and never records). Older versions mute the
output instead, which an output without a mute or volume control (most
audio interfaces) doesn't allow. Only what Bars muted is turned back on,
and if Bars crashes meanwhile, its next start turns it back on.

Bars reads the text around the cursor through the accessibility API, to add
a space only where one is needed, continue a sentence in lowercase and drop
the full stop when the sentence carries on (on Linux this works in apps with
accessibility support, such as GTK apps). Long text pasted into a coding
agent in a terminal (Claude Code, Codex, Droid, pi or OpenCode, also inside
herdr) goes in pieces, so it shows as text instead of "[Pasted text]". If
no text box had focus, the pill says so and offers the text to copy. Pasted
text is kept out of clipboard-history apps, and everything you had copied
(images and files too) comes back afterwards. macOS 27 asks before an app
reads the clipboard, and its Allow lasts one read; until Bars is allowed
under Privacy & Security › Paste from Other Apps (*Allow…* next to Restore
clipboard gets it there), Bars doesn't read it and the clipboard keeps the
dictation.

With a Parakeet model, Bars learns from your corrections: fix a word it got
wrong, in the app you dictated into or on the History page, and it adds the
fix to your dictionary (the pill offers Undo). It only learns fixes of words
the model was unsure of that look or sound like what it wrote, never
rewordings or everyday words; less clear ones wait under Suggestions on the
Dictionary page until they're seen again. Only the word pairs are kept.

English dictations are polished by a second small model, Muesli 0.8B by
Muesli (or Spokenly Cleanup 1 by Spokenly, picked on the Models page):
filler words, repeats and false starts go, and spoken corrections are
applied ("send it to John, no wait, to Maria"). Spokenly also writes
numbers, email addresses and web addresses as you'd type them. The model
also sees the app's name and the text around the cursor, to continue a
sentence.
Questions and instructions stay as you said them, so dictating to a coding
agent is safe, and Bars checks every result: one that adds words or line
breaks of its own, loses most of what you said or takes too long is
dropped, and the dictation goes in as heard. Output polish is on by
default, downloads during setup and can be turned off on the Models page.

When it starts and every 24 hours while it runs (sleep included), Bars
looks for a newer version: it fetches a small file (`latest.json`) from
the latest GitHub release, and sends nothing else. It
only tells you, with a notification and in the tray menu; nothing is
installed until you choose Update (tray menu, settings, or `bars --update`).
That downloads the new version, checks its signature and installs it. On
Windows the installer then restarts Bars, macOS restarts into it once
you're not dictating, and Linux asks whether to restart now. On Linux the
release tarball's `bin/bars` replaces itself, wherever you put it (if Bars
can write there), and the release Flatpak reinstalls itself from the
release's bundle (if it's installed for your user, not system-wide). Either
way the new version has to start before it's kept. Builds from source and
distro packages open the release page instead. The daily check can be
turned off in the settings, under Updates.

## Speech models

All models run locally on the CPU (int8 ONNX). Measured on an 11 s clip:

| Model | Download | Languages | Ryzen 9950X3D | 2 cores |
| --- | --- | --- | --- | --- |
| **Parakeet Ultra** (default) | 668 MB | 25 European, auto-detect | ≈ v3 | ≈ v3 |
| Parakeet TDT 0.6B v3 | 671 MB | 25 European, auto-detect | 245 ms | 513 ms || Parakeet TDT 0.6B v2 | 661 MB | English | ≈ v3 | ≈ v3 |
| Canary 180M Flash | 213 MB | en, de, es, fr | 275 ms | 582 ms |

Models download from Hugging Face on first run into
`~/.local/share/bars/models` (Flatpak: `~/.var/app/xyz.hink.Bars/data`).

[Parakeet Ultra](https://huggingface.co/moondream/parakeet-ultra) is
Moondream's further-trained Parakeet v3 (CC-BY-4.0, as ONNX by
[Olicorne](https://huggingface.co/Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx)):
the same model with better weights. On an M4 Pro it made fewer mistakes than
v3 and ran 20–45% faster. People already using another model are offered it
once, in the sidebar.

Long dictations are transcribed while you talk. Once one is 12 seconds
long, Bars cuts it in the next pause (without one, at the quietest moment
before 28 seconds) and the model transcribes that piece in the background.
It hears a few seconds either side of the piece, so a sentence that runs on
past a cut is punctuated as one, and keeps only the words within it. Each
piece is polished as it comes, after the text before it. When you stop,
only the last piece is left: on an M4 Pro, 48 seconds of speech goes in
0.3 s after you stop instead of 1.2 s. The text still goes into the app in
one go, and shorter dictations are never cut.

### Polish models

Both run on the CPU with llama.cpp. On 56 test dictations (restarts,
fillers, sentences that must stay as they are, and questions and
instructions that must not be answered), measured on a Ryzen 9950X3D:

| Model | Download | Exactly right | Commands changed | Typical | Longest | Typical, 2 cores |
| --- | --- | --- | --- | --- | --- | --- |
| **Muesli 0.8B** (default) | 529 MB | 54/56 | 0/13 | 194 ms | 1.1 s | 295 ms |
| Spokenly Cleanup 1 | 639 MB | 48/56 | 0/13 | 87 ms | 0.45 s | 212 ms |

[Muesli 0.8B](https://huggingface.co/Muesli-HQ/qwen35-0.8b-muesli-cleanup-v12-gguf)
by Muesli (v12, a fine-tune of Qwen3.5-0.8B, Apache-2.0) catches more spoken
corrections and false starts; it keeps numbers as said. Its instructions,
the app's name and the text around the cursor are read into it while
you're still talking, so only the dictation is left when you stop. People
already polishing with Spokenly are offered it once, in the sidebar, and a
settings file from before there was a choice keeps Spokenly where it's
downloaded.

[Spokenly Cleanup 1](https://huggingface.co/spokenly/cleanup-1) by Spokenly
(a fine-tune of Qwen3-0.6B, Apache-2.0; its license and notice download
with it) is 8-bit GGUF. On an M4 Pro a typical dictation takes about 45 ms
and a 90-word email about 190 ms. Most of a clean-up copies the dictation,
so Bars guesses the next tokens from it and has the model check them all
in one pass, which halves the time without changing the result. Muesli's
recurrent layers can't take back a wrong guess, so it goes one token at a
time. For either model, reading ahead doesn't change the result.

Metal isn't used: it isn't faster for a model this small, and it compiles
its shaders for 15 seconds the first time each new version starts. On
x86-64 polish needs AVX2 (Intel since 2013, AMD since 2015).
Dictations that look like another language are left alone.
Adding a model is a catalog entry in `crates/bars-core/src/models.rs`; any
engine supported by [transcribe-rs](https://github.com/cjpais/transcribe-rs)
(Parakeet, Canary, Moonshine, SenseVoice, Whisper...) can be wired in.

## How it reaches other apps (Linux)

Bars picks the best available backend and falls back automatically:

| Need | Preferred | Fallback (no setup) |
| --- | --- | --- |
| Global shortcut | **evdev**: reads keyboards (and mice, for a mouse-button shortcut); any chord, including modifier-only `Ctrl+Super`, a modifier on its own and a key on its own | **GlobalShortcuts portal**: desktop owns the binding, keys only (KDE, GNOME 48+, Hyprland) |
| Insert text | **uinput** virtual keyboard: paste or layout-aware typing | **RemoteDesktop portal**: asks once, remembered |
| Clipboard paste | Wayland data-control, with the old clipboard restored | Type key by key |
| Recording pill | **layer-shell** overlay that never takes focus | Unfocused popup window |

The preferred backends need read access to keyboards (and mice) and write
access to `/dev/uinput`. Bars requests this with a small udev rule that grants it to
the user logged in at the seat (systemd `uaccess`), with no group changes and
no reboot. The **setup assistant** opens on first launch and walks through
the microphone, this permission (one click: "Allow access" shows the
system's password prompt; from Flatpak it goes through the host), the model
download and a first dictation. If you'd rather not change system settings,
it offers the portal route instead. When Bars autostarts with something
missing, it posts a notification rather than opening a window. Manual route:

```sh
pkexec ./scripts/install-udev-rule.sh
```

Note: like membership in the `input` group, this lets programs running as you
read keyboard and mouse events. Mice were added to the rule for mouse-button
shortcuts; with an older rule, setup asks once more when a shortcut is a
mouse button.

The default paste shortcut is **Shift+Insert**, with the text placed on both the
clipboard and the primary selection, so it works in GUI apps and terminals
alike. Ctrl+V and Ctrl+Shift+V are available too.

## Building

Development happens in a toolbox/distrobox with Rust, a C compiler and the
usual headers (`wayland-devel libxkbcommon-devel alsa-lib-devel
fontconfig-devel vulkan-loader-devel`). llama.cpp (polish) also needs
CMake and libclang (`cmake clang-devel`; on macOS `brew install cmake`).

```sh
cargo run -p bars                       # debug build
cargo test -p bars-core                 # core unit tests
cargo run --release -p bars-core --example bench -- testdata/jfk.wav   # model speed
cargo run --release -p bars-core --example polish -- "um so i think we should uh ship it"   # polish
cargo run -p bars-core --example headless   # controller without UI
cargo run -p bars-core --example shortcuts -- Ctrl+Super Mouse4   # what the shortcut listener sees
```

### Flatpak (recommended distribution)

```sh
flatpak install --user flathub org.flatpak.Builder org.freedesktop.Sdk.Extension.rust-stable//26.08 org.freedesktop.Sdk.Extension.llvm22//26.08
./scripts/build-flatpak.sh               # builds + installs for the current user
BUNDLE=1 ./scripts/build-flatpak.sh      # also writes bars.flatpak
flatpak run xyz.hink.Bars
```

Why Flatpak over AppImage: it's the native app format on image-based distros
(Silverblue, Kinoite, Bazzite, Aurora...), works the same across distros
without glibc/FUSE issues, updates incrementally, and the code runs natively.
ONNX Runtime is linked statically and rendering uses Vulkan, so there's no
performance penalty.

## Releases

`.github/workflows/release.yml` builds every platform when a `v*` tag is
pushed and attaches the results to a draft GitHub Release:

| Platform | Artifact |
| --- | --- |
| Linux | `bars-x86_64.flatpak` (any distro) and `bars-linux-x86_64.tar.gz` (glibc 2.38+: Fedora 39+, Ubuntu 24.04+, Debian 13+) |
| Windows | NSIS installer for x64 (`.exe`, per-user, no admin needed) |
| macOS | `.dmg` (Apple Silicon; ONNX Runtime no longer ships Intel Mac builds) |

```sh
git tag v0.1.0 && git push origin v0.1.0
```

The Windows build is unsigned, so SmartScreen warns on first launch. The
macOS app and DMG are signed with a Developer ID Application certificate
(secrets `MACOS_DEVELOPER_ID_P12`, a base64 .p12 with Apple's intermediate,
and `MACOS_DEVELOPER_ID_PASSWORD`; see `scripts/macos-signing-keychain.sh`)
and notarized with an App Store Connect API key (`APPLE_API_KEY`, the .p8
file's contents, `APPLE_API_KEY_ID` and `APPLE_API_ISSUER_ID`), so they open
without a warning. macOS keys Accessibility and Microphone access to the
certificate's Team ID, so it lasts across updates and certificate renewals.
Without the secrets the app is signed ad hoc and not notarized.

In-app updates read `latest.json` from the latest published release (a
draft is never "latest", so publishing the release is what offers it).
`scripts/update-manifest.py` writes it from the macOS `.app.tar.gz`, the
Windows installer, and the Linux tarball and Flatpak bundle, each signed
with the updater key (secrets `UPDATER_PRIVATE_KEY` and
`UPDATER_PRIVATE_KEY_PASSWORD`). Only the release workflow's Linux builds
update themselves: it builds them with `BARS_PACKAGE=tarball` or
`BARS_PACKAGE=flatpak`. The key is made
once with `cargo packager signer generate`; its public half goes in
`PUBLIC_KEY` in `crates/bars-core/src/update.rs`. Keep the private key safe:
installed copies only accept updates signed with it. Without the secrets no
`latest.json` is published; with an empty `PUBLIC_KEY`, Bars announces
updates but only opens the release page.

CI (Actions > CI > Run workflow) also builds Windows and macOS and runs
`scripts/smoke`: the app is started on a real runner, driven through its
command line, and screenshots plus logs are uploaded.

## Layout

```
crates/bars-core   engine, audio, hotkeys, text injection, config, history
  src/controller.rs   single-threaded state machine; UI talks to it by message
  src/hotkey.rs       chords + push-to-talk/double-tap recognizer (tested)
  src/engine.rs       model worker thread (transcribe-rs / ONNX Runtime)
  src/audio.rs        cpal capture (PulseAudio/PipeWire), resampling, cues
  src/segment.rs      cutting long dictations at pauses while you talk (tested)
  src/text.rs         filler removal, dictionary, snippets, voice commands
  src/learn.rs        learning dictionary words from corrections (tested)
  src/polish.rs       polish: Muesli or Spokenly Cleanup 1 on llama.cpp, and the checks
                      on what it returns (tested)
  src/update.rs       update checks (GitHub Releases), installs on request
  src/platform/       OS backends: linux/ (evdev, uinput, portals, data-control
                      clipboard, AT-SPI), win/ (keyboard hook, clipboard, UI
                      Automation), mac/ (event tap, pasteboard, accessibility,
                      TCC permissions), native_inject.rs (Windows/macOS paste
                      + typing), paste.rs (paste-and-confirm, shared),
                      focus.rs (focused app and field text), mute.rs
                      (muting other audio while dictating)
crates/bars        GPUI app: tray, overlay pill, settings window
  src/overlay_native.rs  Windows/macOS pill: click-through, active screen, blur (macOS)
  src/ipc_*.rs          single instance + CLI (D-Bus on Linux, local socket elsewhere)
third_party/        gpui-pre-linux with a Wayland frame-waker fix, and
                    transcribe-rs reporting Parakeet's token confidences
                    (see the .patch files)
packaging/          .desktop, AppStream, icons (from artwork/), udev rule, Flatpak manifest, macOS Info.plist and icon
scripts/smoke/      Windows/macOS runtime smoke tests run by CI
```

The shortcut, insertion, overlay, tray, autostart and permission backends
are implemented on all three platforms. What the setup assistant asks for
lives in `platform/requirements.rs`: keyboard access on Linux (udev rule),
microphone privacy on Windows, and Microphone and Accessibility on macOS
(Accessibility, called Device Control and Data Access since macOS 27,
covers both noticing the shortcut and typing). macOS
keyboard simulation runs on the main thread: layout lookups abort the app
anywhere else.

`scripts/package-macos.sh` builds the DMG: cargo-packager makes Bars.app,
dmgbuild the disk image with its background and icon layout.

Dev helpers: `BARS_ONBOARDING=<welcome|microphone|access|model|try|done>`
opens the assistant at a step; `BARS_DEV_NO_ACCESS=1` previews it as a user
without keyboard access; `BARS_PAGE=<home|general|model|...>` opens settings
at a page; `BARS_DEV_CAPTURE=1` starts recording a new shortcut at launch
(used by the macOS smoke test, which presses keys with
`scripts/smoke/post-keys.swift`).
