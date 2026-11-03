//! Catalog of speech models Bars can run, the polish model, plus
//! download/installation.
//!
//! Adding a model = adding a [`ModelInfo`] entry (and, for a new engine
//! family, a loader arm in `engine.rs`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, bail};

use crate::paths;

pub const DEFAULT_MODEL: &str = "parakeet-ultra";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Family {
    Parakeet,
    Canary,
    /// Not a speech model: polishes transcripts (see `polish`).
    Polish,
}

#[derive(Clone, Debug)]
pub struct ModelFile {
    pub url: &'static str,
    /// File name inside the model directory.
    pub name: &'static str,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub tagline: &'static str,
    pub family: Family,
    pub languages: &'static str,
    /// Whether the model accepts a language hint / is multilingual.
    pub multilingual: bool,
    /// 1-5, relative.
    pub accuracy: u8,
    /// 1-5, relative (CPU).
    pub speed: u8,
    pub files: &'static [ModelFile],
}

impl ModelInfo {
    pub fn size_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn dir(&self) -> PathBuf {
        paths::models_dir().join(self.id)
    }

    pub fn is_installed(&self) -> bool {
        let dir = self.dir();
        self.files.iter().all(|f| {
            std::fs::metadata(dir.join(f.name))
                .map(|m| m.len() == f.size)
                .unwrap_or(false)
        })
    }

    pub fn delete(&self) -> std::io::Result<()> {
        match std::fs::remove_dir_all(self.dir()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
const HF: &str = "https://huggingface.co";

macro_rules! hf {
    ($repo:literal, $path:literal) => {
        concat!("https://huggingface.co/", $repo, "/resolve/main/", $path)
    };
    ($repo:literal @ $rev:literal, $path:literal) => {
        concat!("https://huggingface.co/", $repo, "/resolve/", $rev, "/", $path)
    };
}

pub static CATALOG: &[ModelInfo] = &[
    // Moondream's further-trained Parakeet v3: the same model and tokenizer,
    // new weights. Pinned to a revision, so the sizes stay right.
    ModelInfo {
        id: "parakeet-ultra",
        name: "Parakeet Ultra",
        tagline: "Best accuracy, ~40x real-time on a modern CPU.",
        family: Family::Parakeet,
        languages: "25 European languages · auto-detect",
        multilingual: true,
        accuracy: 5,
        speed: 4,
        files: &[
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "int8/encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 649_431_593,
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "int8/decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 18_203_490,
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "vocab.txt"),
                name: "vocab.txt",
                size: 93_939,
            },
        ],
    },
    ModelInfo {
        id: "parakeet-tdt-0.6b-v3",
        name: "Parakeet v3",
        tagline: "NVIDIA's original, which Ultra is trained from.",
        family: Family::Parakeet,
        languages: "25 European languages · auto-detect",
        multilingual: true,
        accuracy: 5,
        speed: 4,
        files: &[
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 652_183_999,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx", "decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 18_202_004,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx", "vocab.txt"),
                name: "vocab.txt",
                size: 93_939,
            },
        ],
    },
    ModelInfo {
        id: "parakeet-tdt-0.6b-v2",
        name: "Parakeet v2",
        tagline: "English-only sibling of v3, tuned for English.",
        family: Family::Parakeet,
        languages: "English",
        multilingual: false,
        accuracy: 5,
        speed: 4,
        files: &[
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 652_184_014,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx", "decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 8_998_286,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx", "vocab.txt"),
                name: "vocab.txt",
                size: 9_384,
            },
        ],
    },
    ModelInfo {
        id: "canary-180m-flash",
        name: "Canary Flash",
        tagline: "A third of the size. Great for low disk or memory.",
        family: Family::Canary,
        languages: "English, German, Spanish, French",
        multilingual: true,
        accuracy: 4,
        speed: 4,
        files: &[
            ModelFile {
                url: hf!("istupakov/canary-180m-flash-onnx", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 133_710_896,
            },
            ModelFile {
                url: hf!("istupakov/canary-180m-flash-onnx", "decoder-model.int8.onnx"),
                name: "decoder-model.int8.onnx",
                size: 79_520_211,
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
            },
            ModelFile {
                url: hf!("istupakov/canary-180m-flash-onnx", "vocab.txt"),
                name: "vocab.txt",
                size: 53_555,
            },
        ],
    },
];

/// Spokenly Cleanup 1 by Spokenly, which polishes English dictations (see
/// `polish`). Its license and notice come with it, as the Apache License
/// asks. Pinned to a revision, so the sizes stay right.
pub static POLISH: ModelInfo = ModelInfo {
    id: "spokenly-cleanup-1",
    name: "Spokenly Cleanup 1",
    tagline: "Removes fillers and false starts, applies spoken corrections, writes numbers as digits.",
    family: Family::Polish,
    languages: "English",
    multilingual: false,
    accuracy: 4,
    speed: 4,
    files: &[
        ModelFile {
            url: hf!("spokenly/cleanup-1-gguf" @ "79936c95e5c0ea9162ae7a435e64cb3127020e36", "cleanup-1-q8_0.gguf"),
            name: crate::polish::MODEL_FILE,
            size: 639_442_784,
        },
        ModelFile {
            url: hf!("spokenly/cleanup-1-gguf" @ "79936c95e5c0ea9162ae7a435e64cb3127020e36", "LICENSE"),
            name: "LICENSE",
            size: 11_358,
        },
        ModelFile {
            url: hf!("spokenly/cleanup-1-gguf" @ "79936c95e5c0ea9162ae7a435e64cb3127020e36", "NOTICE"),
            name: "NOTICE",
            size: 554,
        },
    ],
};

/// A speech model by id.
pub fn find(id: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Every model Bars downloads: the speech models and the polish model.
pub fn all() -> impl Iterator<Item = &'static ModelInfo> {
    CATALOG.iter().chain(std::iter::once(&POLISH))
}

/// Any downloadable model by id.
pub fn find_any(id: &str) -> Option<&'static ModelInfo> {
    all().find(|m| m.id == id)
}

/// Progress callback: (bytes done, bytes total).
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// HTTP client for downloads and update checks.
pub(crate) fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!("Bars/", env!("CARGO_PKG_VERSION")))
        // The system's trusted certificates, not just Mozilla's bundled list.
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Downloads every file of `model` into its directory. Files are written to
/// `*.part` and renamed on completion, so an interrupted download never
/// looks installed. Already-complete files are skipped.
pub fn download(model: &ModelInfo, cancel: Arc<AtomicBool>, progress: Progress) -> anyhow::Result<()> {
    let dir = model.dir();
    std::fs::create_dir_all(&dir)?;
    let agent = http_agent();

    let total = model.size_bytes();
    let mut done_before = 0u64;

    for f in model.files {
        let dest = dir.join(f.name);
        if std::fs::metadata(&dest).is_ok_and(|m| m.len() == f.size) {
            done_before += f.size;
            progress(done_before, total);
            continue;
        }
        fetch(&agent, f.url, &dest, &cancel, &mut |n| progress(done_before + n, total))
            .with_context(|| format!("downloading {}", f.name))?;
        let got = std::fs::metadata(&dest)?.len();
        if got != f.size {
            let _ = std::fs::remove_file(&dest);
            bail!("{} is {got} bytes, expected {}", f.name, f.size);
        }
        done_before += f.size;
        progress(done_before, total);
    }
    Ok(())
}

fn fetch(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<()> {
    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    let mut resp = agent.get(url).call()?;
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(out);
            let _ = std::fs::remove_file(&part);
            bail!("cancelled");
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        progress(done);
    }
    out.flush()?;
    drop(out);
    std::fs::rename(&part, dest)?;
    Ok(())
}

/// Lists ids of installed models (the polish model too).
pub fn installed() -> Vec<&'static str> {
    all().filter(|m| m.is_installed()).map(|m| m.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        assert!(find(DEFAULT_MODEL).is_some());
        assert!(find(POLISH.id).is_none() && find_any(POLISH.id).is_some());
        for m in all() {
            assert!(m.files.iter().all(|f| f.url.starts_with(HF) && f.size > 0), "{}", m.id);
        }
    }
}
