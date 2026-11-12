//! Catalog of speech models Bars can run, the polish model, plus
//! download/installation.
//!
//! Adding a model = adding a [`ModelInfo`] entry (and, for a new engine
//! family, a loader arm in `engine.rs`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use crate::paths;

pub const DEFAULT_MODEL: &str = "parakeet-ultra";
/// The polish model new installs start on.
pub const DEFAULT_POLISH: &str = "muesli-0.8b-v12";

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
    /// SHA-256 of the file, in hex: a download is checked against it.
    pub sha256: &'static str,
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

/// A file at a revision of its repository, never a branch: what's
/// downloaded is then exactly what the sizes and checksums here describe,
/// whatever is published there later.
macro_rules! hf {
    ($repo:literal @ $rev:literal, $path:literal) => {
        concat!("https://huggingface.co/", $repo, "/resolve/", $rev, "/", $path)
    };
}

pub static CATALOG: &[ModelInfo] = &[
    // Moondream's further-trained Parakeet v3: the same model and tokenizer,
    // new weights.
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
                sha256: "714047d70be4b612dce02e0ab2f9e0d6552f78adc421f408d154070796bc2dcf",
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "int8/decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 18_203_490,
                sha256: "667ffde2242a560923cba6bc7f69ef03d736f8a36ca9588f2e6f9fa818cf2fc1",
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
                sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
            },
            ModelFile {
                url: hf!("Olicorne/parakeet-tdt-0.6b-v3-ultra-onnx" @ "b27f93c2238aa35e138996c005a62f1e8430f801", "vocab.txt"),
                name: "vocab.txt",
                size: 93_939,
                sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
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
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx" @ "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 652_183_999,
                sha256: "6139d2fa7e1b086097b277c7149725edbab89cc7c7ae64b23c741be4055aff09",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx" @ "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce", "decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 18_202_004,
                sha256: "eea7483ee3d1a30375daedc8ed83e3960c91b098812127a0d99d1c8977667a70",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx" @ "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
                sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx" @ "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce", "vocab.txt"),
                name: "vocab.txt",
                size: 93_939,
                sha256: "d58544679ea4bc6ac563d1f545eb7d474bd6cfa467f0a6e2c1dc1c7d37e3c35d",
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
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx" @ "0bbb45a3365852604aef28b538a8f066f4ccaa85", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 652_184_014,
                sha256: "3e0581fda6ab843888b51e56d7ee78b6d5bc3237ec113af1f732d1d5286aa155",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx" @ "0bbb45a3365852604aef28b538a8f066f4ccaa85", "decoder_joint-model.int8.onnx"),
                name: "decoder_joint-model.int8.onnx",
                size: 8_998_286,
                sha256: "a449f49acd68979d418651dd2dcb737cc0f1bf0225e009e29ee326354edbf7d3",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx" @ "0bbb45a3365852604aef28b538a8f066f4ccaa85", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
                sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v2-onnx" @ "0bbb45a3365852604aef28b538a8f066f4ccaa85", "vocab.txt"),
                name: "vocab.txt",
                size: 9_384,
                sha256: "ec182b70dd42113aff6c5372c75cac58c952443eb22322f57bbd7f53977d497d",
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
                url: hf!("istupakov/canary-180m-flash-onnx" @ "92c2231a4e2b2524277fea759be967d2e6edfc49", "encoder-model.int8.onnx"),
                name: "encoder-model.int8.onnx",
                size: 133_710_896,
                sha256: "996d1c89e6cbc891a7c88bf410884c178ffa474f7b13084522ac74a5e144cc81",
            },
            ModelFile {
                url: hf!("istupakov/canary-180m-flash-onnx" @ "92c2231a4e2b2524277fea759be967d2e6edfc49", "decoder-model.int8.onnx"),
                name: "decoder-model.int8.onnx",
                size: 79_520_211,
                sha256: "9dd9c447872088c912e916d73751f9621a54085d5bc46788454fe904db51a914",
            },
            ModelFile {
                url: hf!("istupakov/parakeet-tdt-0.6b-v3-onnx" @ "8f23f0c03c8761650bdb5b40aaf3e40d2c15f1ce", "nemo128.onnx"),
                name: "nemo128.onnx",
                size: 139_764,
                sha256: "a9fde1486ebfcc08f328d75ad4610c67835fea58c73ba57e3209a6f6cf019e9f",
            },
            ModelFile {
                url: hf!("istupakov/canary-180m-flash-onnx" @ "92c2231a4e2b2524277fea759be967d2e6edfc49", "vocab.txt"),
                name: "vocab.txt",
                size: 53_555,
                sha256: "2dae6fc7815f9640645e0c765522b278ee0cef49b482d91f6913e334628d3e77",
            },
        ],
    },
];

/// Spokenly Cleanup 1 by Spokenly, the first polish model (see `polish`),
/// still offered. Its license and notice come with it, as the Apache
/// License asks. Pinned to a revision, so the sizes stay right.
pub static SPOKENLY: ModelInfo = ModelInfo {
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
            name: "cleanup-1-q8_0.gguf",
            size: 639_442_784,
            sha256: "66509e0fe1578d2ac4b0773c79a6c83e8d6f218618dfe11e4532beacd999b6e8",
        },
        ModelFile {
            url: hf!("spokenly/cleanup-1-gguf" @ "79936c95e5c0ea9162ae7a435e64cb3127020e36", "LICENSE"),
            name: "LICENSE",
            size: 11_358,
            sha256: "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
        },
        ModelFile {
            url: hf!("spokenly/cleanup-1-gguf" @ "79936c95e5c0ea9162ae7a435e64cb3127020e36", "NOTICE"),
            name: "NOTICE",
            size: 554,
            sha256: "c8c2a5b22187685c78261d15938882f1fa91ba75d05d6c469f3b7cb4162fb769",
        },
    ],
};

/// Muesli 0.8B (v12) by Muesli, the default polish model (see `polish`): a
/// fine-tune of Qwen3.5-0.8B, Apache-2.0. Pinned to a revision, so the
/// size stays right.
pub static MUESLI: ModelInfo = ModelInfo {
    id: "muesli-0.8b-v12",
    name: "Muesli 0.8B",
    tagline: "Removes fillers and false starts, applies spoken corrections, keeps your wording.",
    family: Family::Polish,
    languages: "English",
    multilingual: false,
    accuracy: 5,
    speed: 3,
    files: &[ModelFile {
    url: hf!("Muesli-HQ/qwen35-0.8b-muesli-cleanup-v12-gguf" @ "0e6b7446a64b170881bfe1af4b3ba892d18ecf3a", "qwen35-0.8b-muesli-v12-Q4_K_M.gguf"),
    name: "qwen35-0.8b-muesli-v12-Q4_K_M.gguf",
    size: 529_296_832,
    sha256: "3e3eabc91542e66f672230b7361917f5fa3a1e06ace61e8157c7a34d121abf8a",
}],
};

/// The polish models to choose from, the default first. Each one's first
/// file is the model.
pub static POLISH_MODELS: [&ModelInfo; 2] = [&MUESLI, &SPOKENLY];

/// The polish model with this id, or the default.
pub fn polish_model(id: &str) -> &'static ModelInfo {
    POLISH_MODELS.iter().copied().find(|m| m.id == id).unwrap_or(POLISH_MODELS[0])
}

/// Who made a polish model, as its name is credited ("… by Spokenly").
pub fn polish_maker(m: &ModelInfo) -> &'static str {
    if m.id == MUESLI.id { "Muesli" } else { "Spokenly" }
}

/// A speech model by id.
pub fn find(id: &str) -> Option<&'static ModelInfo> {
    CATALOG.iter().find(|m| m.id == id)
}

/// Every model Bars downloads: the speech models and the polish models.
pub fn all() -> impl Iterator<Item = &'static ModelInfo> {
    CATALOG.iter().chain(POLISH_MODELS.iter().copied())
}

/// Any downloadable model by id.
pub fn find_any(id: &str) -> Option<&'static ModelInfo> {
    all().find(|m| m.id == id)
}

/// Progress callback: (bytes done, bytes total).
pub type Progress<'a> = &'a mut dyn FnMut(u64, u64);

/// HTTP client for downloads and update checks. A server that never
/// answers is given up on; how long a body may take is up to each request
/// (a model takes as long as it takes: see `copy`).
pub(crate) fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!("Bars/", env!("CARGO_PKG_VERSION")))
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // The system's trusted certificates, not just Mozilla's bundled list.
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// A download gives up once nothing has arrived for this long.
const STALLED_AFTER: Duration = Duration::from_secs(60);
/// How often a download looks at whether it was cancelled.
const CANCEL_CHECK: Duration = Duration::from_millis(250);

/// Downloads every file of `model` into its directory. Files are written to
/// `*.part`, checked against the catalog's size and checksum, and only then
/// renamed, so an interrupted or damaged download never looks installed.
/// Already-complete files are skipped, and a download that broke off
/// carries on from where it stopped.
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
        let part = dest.with_extension(format!(
            "{}.part",
            dest.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        fetch(&agent, f, &part, &cancel, &mut |n| progress(done_before + n, total))
            .with_context(|| format!("downloading {}", f.name))?;
        if let Err(e) = verify(&part, f, &cancel) {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &dest)?;
        done_before += f.size;
        progress(done_before, total);
    }
    Ok(())
}

/// Downloads `file` to `part`, after whatever an earlier attempt left
/// there. The part stays when the connection fails, to carry on from; it's
/// removed when the download is cancelled or can't be written.
fn fetch(
    agent: &ureq::Agent,
    file: &ModelFile,
    part: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> anyhow::Result<()> {
    let mut have = std::fs::metadata(part).map_or(0, |m| m.len());
    if have >= file.size {
        // Not the start of this file.
        have = 0;
    }
    // Uncompressed, so byte counts are the file's own.
    let get = || agent.get(file.url).header("Accept-Encoding", "identity");
    let resp = if have > 0 {
        match get().header("Range", format!("bytes={have}-")).call() {
            // The server won't carry on from there: from the start, then.
            Err(ureq::Error::StatusCode(416)) => {
                have = 0;
                get().call()?
            }
            other => other?,
        }
    } else {
        get().call()?
    };
    if resp.status() != 206 {
        // The whole file (a range that was ignored, too).
        have = 0;
    }
    if let Some(coming) = resp.body().content_length()
        && have + coming != file.size
    {
        bail!("it's {} bytes there, expected {}", have + coming, file.size);
    }
    let mut options = std::fs::OpenOptions::new();
    if have > 0 {
        options.append(true);
    } else {
        options.write(true).create(true).truncate(true);
    }
    let mut out = std::io::BufWriter::new(options.open(part)?);
    progress(have);
    let body = resp.into_body().into_with_config().limit(u64::MAX).reader();
    let copied = copy(body, &mut out, cancel, STALLED_AFTER, &mut |n| progress(have + n));
    let flushed = out.flush();
    drop(out);
    match (copied, flushed) {
        (Ok(_), Ok(())) => Ok(()),
        (Err(Stopped::Cancelled), _) => {
            let _ = std::fs::remove_file(part);
            bail!("cancelled")
        }
        (Err(Stopped::Write(e)), _) | (Ok(_), Err(e)) => {
            let _ = std::fs::remove_file(part);
            Err(e).context("writing the file")
        }
        (Err(Stopped::Stalled), _) => bail!("nothing arrived for {} seconds", STALLED_AFTER.as_secs()),
        (Err(Stopped::Read(e)), _) => Err(e).context("the connection failed"),
    }
}

/// Why a copy ended early.
#[derive(Debug)]
enum Stopped {
    Cancelled,
    /// Nothing came for too long.
    Stalled,
    Read(std::io::Error),
    Write(std::io::Error),
}

/// Copies `from` into `out`, returning the bytes copied. The reading is
/// done on a thread of its own: a read from a connection that has gone
/// quiet can't be interrupted, only left behind, and this way a stalled
/// download ends (and a cancelled one ends at once) instead of hanging.
fn copy(
    mut from: impl Read + Send + 'static,
    out: &mut impl Write,
    cancel: &AtomicBool,
    stalled_after: Duration,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, Stopped> {
    let (tx, rx) = crossbeam_channel::bounded::<std::io::Result<Vec<u8>>>(8);
    std::thread::Builder::new()
        .name("bars-download-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let read = from.read(&mut buf).map(|n| buf[..n].to_vec());
                let last = !matches!(&read, Ok(chunk) if !chunk.is_empty());
                if tx.send(read).is_err() || last {
                    return;
                }
            }
        })
        .map_err(Stopped::Read)?;
    let mut done = 0u64;
    let mut heard = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stopped::Cancelled);
        }
        match rx.recv_timeout(CANCEL_CHECK) {
            Ok(Ok(chunk)) if chunk.is_empty() => return Ok(done),
            Ok(Ok(chunk)) => {
                out.write_all(&chunk).map_err(Stopped::Write)?;
                done += chunk.len() as u64;
                heard = Instant::now();
                progress(done);
            }
            Ok(Err(e)) => return Err(Stopped::Read(e)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if heard.elapsed() >= stalled_after {
                    return Err(Stopped::Stalled);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err(Stopped::Read(std::io::Error::other("the download ended early")));
            }
        }
    }
}

/// Whether a finished download is the file the catalog describes.
fn verify(path: &Path, file: &ModelFile, cancel: &AtomicBool) -> anyhow::Result<()> {
    let got = std::fs::metadata(path)?.len();
    if got != file.size {
        bail!("{} is {got} bytes, expected {}", file.name, file.size);
    }
    if sha256_hex(path, cancel)? != file.sha256 {
        bail!("{} doesn't match its checksum", file.name);
    }
    Ok(())
}

fn sha256_hex(path: &Path, cancel: &AtomicBool) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
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
        for m in POLISH_MODELS {
            assert!(find(m.id).is_none() && find_any(m.id).is_some() && polish_model(m.id).id == m.id);
            assert!(m.files[0].name.ends_with(".gguf"), "{}", m.id);
        }
        assert_eq!(POLISH_MODELS[0].id, DEFAULT_POLISH);
        assert_eq!(polish_model("gone").id, DEFAULT_POLISH);
        for m in all() {
            assert!(m.files.iter().all(|f| f.url.starts_with(HF) && f.size > 0), "{}", m.id);
            for f in m.files {
                // A revision, not a branch, and a checksum to hold it to.
                let rev = f.url.split("/resolve/").nth(1).and_then(|rest| rest.split('/').next()).unwrap_or("");
                assert!(rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()), "{} {}", m.id, f.name);
                assert!(f.sha256.len() == 64 && f.sha256.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')), "{} {}", m.id, f.name);
            }
        }
    }

    /// Reads what it's given, then never returns: a connection gone quiet.
    struct Quiet(std::io::Cursor<Vec<u8>>);

    impl Read for Quiet {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buf)? {
                0 => loop {
                    std::thread::park();
                },
                n => Ok(n),
            }
        }
    }

    #[test]
    fn a_copy_ends_when_cancelled_or_stalled() {
        let never = AtomicBool::new(false);
        let data: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        let mut seen = 0;
        let n = copy(std::io::Cursor::new(data.clone()), &mut out, &never, Duration::from_secs(5), &mut |n| seen = n).unwrap();
        assert_eq!((n, seen, &out), (200_000, 200_000, &data));

        // What arrived is kept; then nothing comes.
        let mut out = Vec::new();
        let stopped = copy(Quiet(std::io::Cursor::new(vec![7; 10])), &mut out, &never, Duration::from_millis(300), &mut |_| {});
        assert!(matches!(stopped, Err(Stopped::Stalled)), "{stopped:?}");
        assert_eq!(out, vec![7; 10]);

        let cancelled = AtomicBool::new(true);
        let stopped = copy(Quiet(std::io::Cursor::new(Vec::new())), &mut Vec::new(), &cancelled, Duration::from_secs(60), &mut |_| {});
        assert!(matches!(stopped, Err(Stopped::Cancelled)), "{stopped:?}");
    }

    /// Needs the network: fetches Parakeet's small feature model, whole
    /// and again from a part an interrupted download left.
    #[test]
    #[ignore]
    fn downloads_and_resumes_from_hugging_face() {
        let file = find(DEFAULT_MODEL).unwrap().files.iter().find(|f| f.name == "nemo128.onnx").unwrap();
        let part = std::env::temp_dir().join(format!("bars-models-fetch-{}.part", std::process::id()));
        let _ = std::fs::remove_file(&part);
        let (agent, never) = (http_agent(), AtomicBool::new(false));
        let mut seen = Vec::new();
        fetch(&agent, file, &part, &never, &mut |n| seen.push(n)).unwrap();
        verify(&part, file, &never).unwrap();
        assert_eq!((seen.first(), seen.last()), (Some(&0), Some(&file.size)));

        // Cut short: the rest is fetched, and progress starts where it was.
        let whole = std::fs::read(&part).unwrap();
        std::fs::write(&part, &whole[..50_000]).unwrap();
        let mut seen = Vec::new();
        fetch(&agent, file, &part, &never, &mut |n| seen.push(n)).unwrap();
        verify(&part, file, &never).unwrap();
        assert_eq!((seen.first(), seen.last()), (Some(&50_000), Some(&file.size)));
        assert_eq!(std::fs::read(&part).unwrap(), whole);

        // A part that can't be the start of the file is replaced.
        std::fs::write(&part, vec![0u8; file.size as usize + 10]).unwrap();
        fetch(&agent, file, &part, &never, &mut |_| {}).unwrap();
        verify(&part, file, &never).unwrap();
        let _ = std::fs::remove_file(&part);
    }

    #[test]
    fn checks_a_download_against_the_catalog() {
        let path = std::env::temp_dir().join(format!("bars-models-test-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let never = AtomicBool::new(false);
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_hex(&path, &never).unwrap(), abc);
        let file = |size, sha256| ModelFile { url: "", name: "test", size, sha256 };
        assert!(verify(&path, &file(3, abc), &never).is_ok());
        assert!(verify(&path, &file(4, abc), &never).unwrap_err().to_string().contains("3 bytes"));
        let other = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(verify(&path, &file(3, other), &never).unwrap_err().to_string().contains("checksum"));
        let _ = std::fs::remove_file(&path);
    }
}
