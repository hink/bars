#!/usr/bin/env python3
"""Writes latest.json, the manifest the in-app updater reads from the latest
release (crates/bars-core/src/update.rs), from a release's signed update
files: the macOS .app.tar.gz, the Windows -setup.exe, and the Linux tarball
and Flatpak bundle, each with its .sig. Without any signature (no updater
key) nothing is written. The Linux files go under "linux", by architecture
and kind: the macOS and Windows updater rejects a manifest with a format it
doesn't know in "platforms". The release's notes, which the app shows when it
offers the update, come from its entry in the AppStream metainfo.

    scripts/update-manifest.py v0.3.1 artifacts
"""
import datetime
import json
import pathlib
import re
import sys
import xml.etree.ElementTree as ET

REPO = "hink/bars"
METAINFO = pathlib.Path(__file__).resolve().parent.parent / "packaging/linux/xyz.hink.Bars.metainfo.xml"
# File name -> (updater target, format). Arches as the updater names them.
PATTERNS = [
    (re.compile(r"_(aarch64|x86_64)\.app\.tar\.gz$"), "macos", "app"),
    (re.compile(r"_(x64|arm64)-setup\.exe$"), "windows", "nsis"),
]
ARCH = {"x64": "x86_64", "arm64": "aarch64"}
# File name -> the kind of Linux build that updates itself from it.
LINUX = [
    (re.compile(r"^bars-linux-(x86_64|aarch64)\.tar\.gz$"), "tarball"),
    (re.compile(r"^bars-(x86_64|aarch64)\.flatpak$"), "flatpak"),
]



def release_notes(version):
    """What changed in `version`, in a line: its first change in the
    metainfo, and how many more there are. None if it has no entry."""
    try:
        releases = ET.parse(METAINFO).getroot().iter("release")
    except (OSError, ET.ParseError) as e:
        print(f"no release notes: {e}", file=sys.stderr)
        return None
    for release in releases:
        if release.get("version") != version:
            continue
        description = release.find("description")
        if description is None:
            return None
        changes = [" ".join("".join(item.itertext()).split()) for item in description.iter("li")]
        changes = [c for c in changes if c] or [" ".join("".join(description.itertext()).split())]
        if not changes[0]:
            return None
        more = len(changes) - 1
        return changes[0] + (f" Plus {more} more change{'s' if more > 1 else ''}." if more else "")
    return None


tag, folder = sys.argv[1], pathlib.Path(sys.argv[2])
platforms = {}
linux = {}
for path in sorted(folder.iterdir()):
    sig = path.with_name(path.name + ".sig")
    if not sig.exists():
        continue
    url = f"https://github.com/{REPO}/releases/download/{tag}/{path.name}"
    for pattern, os_name, fmt in PATTERNS:
        m = pattern.search(path.name)
        if m:
            arch = ARCH.get(m.group(1), m.group(1))
            platforms[f"{os_name}-{arch}"] = {"url": url, "signature": sig.read_text().strip(), "format": fmt}
    for pattern, kind in LINUX:
        m = pattern.search(path.name)
        if m:
            linux.setdefault(m.group(1), {})[kind] = {"url": url, "signature": sig.read_text().strip()}

if not platforms and not linux:
    print("no signed update files; latest.json not written", file=sys.stderr)
    sys.exit(0)
manifest = {
    "version": tag.removeprefix("v"),
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": platforms,
    "linux": linux,
}
notes = release_notes(manifest["version"])
if notes:
    manifest["notes"] = notes
else:
    print(f"no notes for {manifest['version']} in {METAINFO.name}", file=sys.stderr)
(folder / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
found = [*platforms, *(f"linux-{arch}-{kind}" for arch, kinds in linux.items() for kind in kinds)]
print(f"latest.json: {', '.join(found)}", file=sys.stderr)
