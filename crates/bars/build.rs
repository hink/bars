//! Windows: embeds the app icon and version information in bars.exe, so
//! Explorer, Apps & features, Task Manager and management tools (Intune)
//! show them. GPUI loads icon resource 1 as the window icon. GPUI embeds
//! its own manifest, so none is added here.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

fn windows_resources() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("../../packaging/windows/bars.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let icon = icon.canonicalize().expect("packaging/windows/bars.ico");

    let env = |name: &str| std::env::var(name).unwrap();
    let version = env("CARGO_PKG_VERSION");
    let numeric = format!(
        "{},{},{},0",
        env("CARGO_PKG_VERSION_MAJOR"),
        env("CARGO_PKG_VERSION_MINOR"),
        env("CARGO_PKG_VERSION_PATCH")
    );
    // Backslashes are escapes in .rc strings.
    let icon = icon.display().to_string().replace('\\', "\\\\");
    // Plain numbers instead of winver.h's names, so no SDK headers are needed:
    // VOS_NT_WINDOWS32, VFT_APP, US English with Unicode.
    let rc = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEFLAGSMASK 0x3F
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Chris Hinkley"
      VALUE "FileDescription", "Bars"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "bars"
      VALUE "LegalCopyright", "Copyright (c) Chris Hinkley"
      VALUE "OriginalFilename", "bars.exe"
      VALUE "ProductName", "Bars"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = PathBuf::from(env("OUT_DIR")).join("bars.rc");
    std::fs::write(&path, rc).unwrap();
    embed_resource::compile(&path, embed_resource::NONE).manifest_optional().unwrap();
}
