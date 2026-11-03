//! The speech engine: owns the loaded model on a dedicated thread and serves
//! transcription requests in order.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use transcribe_rs::onnx::Quantization;
use transcribe_rs::onnx::canary::CanaryModel;
use transcribe_rs::onnx::parakeet::ParakeetModel;
use transcribe_rs::{SpeechModel, TranscribeOptions, TranscriptionResult, TranscriptionSegment};

use crate::learn::{self, Heard, Pieces};
use crate::models::{self, Family, ModelInfo};
use crate::segment::Piece;

#[derive(Clone, Debug, PartialEq)]
pub enum EngineStatus {
    /// No model selected or the selected one isn't downloaded.
    Missing(String),
    Loading(String),
    Ready { id: String, load_ms: u64 },
    /// Model was unloaded to save memory; it reloads on next use.
    Unloaded(String),
    Failed { id: String, error: String },
}

#[derive(Clone, Debug)]
pub struct Transcript {
    pub request: u64,
    pub text: String,
    pub audio_secs: f32,
    pub infer_ms: u64,
    pub model: String,
    /// The words as the model heard them, with its confidence in each
    /// (Parakeet only; learning from corrections uses them).
    pub heard: Option<Vec<Heard>>,
}

pub enum EngineEvent {
    Status(EngineStatus),
    /// The loaded model's vocabulary, which learning needs. None for models
    /// learning doesn't support.
    Pieces(Option<Arc<Pieces>>),
    Done(Transcript),
    Failed { request: u64, error: String },
}

enum Cmd {
    Select { id: String, threads: Option<usize>, idle_unload: Option<Duration> },
    /// `keep`: only the words heard there (a piece of a longer recording
    /// and the audio around it).
    Transcribe { request: u64, samples: Vec<f32>, rate: u32, keep: Option<Range<usize>>, language: Option<String> },
    Unload,
}

#[derive(Clone)]
pub struct Engine {
    tx: Sender<Cmd>,
}

impl Engine {
    pub fn spawn(events: Sender<EngineEvent>) -> Engine {
        let (tx, rx) = unbounded();
        thread::Builder::new()
            .name("bars-engine".into())
            .spawn(move || Worker::new(events).run(rx))
            .expect("spawn engine thread");
        Engine { tx }
    }

    /// Choose (and eagerly load) a model.
    pub fn select(&self, id: &str, threads: Option<usize>, unload_after_mins: u32) {
        let idle_unload = (unload_after_mins > 0)
            .then(|| Duration::from_secs(unload_after_mins as u64 * 60));
        let _ = self.tx.send(Cmd::Select {
            id: id.to_string(),
            threads,
            idle_unload,
        });
    }

    /// Transcribes mono `samples` recorded at `rate` (converted to 16 kHz
    /// here, on the engine's thread).
    pub fn transcribe(&self, request: u64, samples: Vec<f32>, rate: u32, language: Option<String>) {
        let _ = self.tx.send(Cmd::Transcribe {
            request,
            samples,
            rate,
            keep: None,
            language,
        });
    }

    /// Transcribes a piece of a long recording: the words heard within it,
    /// with the audio around it heard too, so its first and last words
    /// are taken as the middle of a sentence where they are.
    pub fn transcribe_piece(&self, request: u64, piece: Piece, rate: u32, language: Option<String>) {
        let _ = self.tx.send(Cmd::Transcribe {
            request,
            samples: piece.samples,
            rate,
            keep: Some(piece.keep),
            language,
        });
    }

    pub fn unload(&self) {
        let _ = self.tx.send(Cmd::Unload);
    }
}

/// Parakeet is kept by type: learning reads its confidences.
enum Model {
    Parakeet(ParakeetModel),
    Other(Box<dyn SpeechModel + Send>),
}

impl Model {
    fn speech(&mut self) -> &mut dyn SpeechModel {
        match self {
            Model::Parakeet(m) => m,
            Model::Other(m) => m.as_mut(),
        }
    }

    /// The words of the last transcription, with confidences: all of
    /// them, or those of the tokens `kept`.
    fn heard(&mut self, kept: Option<&[bool]>) -> Option<Vec<Heard>> {
        let Model::Parakeet(m) = self else { return None };
        let record = m.take_decode()?;
        if kept.is_some_and(|k| k.len() != record.tokens.len()) {
            return None;
        }
        let vocab = m.vocab();
        let (tokens, probs): (Vec<&str>, Vec<f32>) = record
            .tokens
            .iter()
            .zip(&record.probs)
            .enumerate()
            .filter(|(i, _)| kept.is_none_or(|k| k[*i]))
            .map(|(_, (&t, &p))| (vocab.get(t as usize).map_or("", String::as_str), p))
            .unzip();
        Some(learn::heard_words(&tokens, &probs))
    }
}

struct Loaded {
    id: String,
    family: Family,
    model: Model,
}

struct Worker {
    events: Sender<EngineEvent>,
    selected: Option<String>,
    threads: Option<usize>,
    idle_unload: Option<Duration>,
    loaded: Option<Loaded>,
    last_used: Instant,
}

impl Worker {
    fn new(events: Sender<EngineEvent>) -> Self {
        Worker {
            events,
            selected: None,
            threads: None,
            idle_unload: None,
            loaded: None,
            last_used: Instant::now(),
        }
    }

    fn status(&self, s: EngineStatus) {
        let _ = self.events.send(EngineEvent::Status(s));
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    if let (Some(limit), Some(l)) = (self.idle_unload, &self.loaded) {
                        if self.last_used.elapsed() > limit {
                            let id = l.id.clone();
                            self.loaded = None;
                            log::info!("unloaded idle model {id}");
                            self.status(EngineStatus::Unloaded(id));
                        }
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            match cmd {
                Cmd::Select { id, threads, idle_unload } => {
                    let changed = self.selected.as_deref() != Some(&id) || self.threads != threads;
                    self.selected = Some(id);
                    self.threads = threads;
                    self.idle_unload = idle_unload;
                    if changed || self.loaded.is_none() {
                        self.loaded = None;
                        let _ = self.ensure_loaded();
                    }
                }
                Cmd::Transcribe { request, samples, rate, keep, language } => {
                    // In seconds, open-ended at the recording's start and end.
                    let secs = |i: usize| i as f32 / rate as f32;
                    let span = keep.as_ref().map(|k| {
                        let start = if k.start == 0 { f32::NEG_INFINITY } else { secs(k.start) };
                        let end = if k.end >= samples.len() { f32::INFINITY } else { secs(k.end) };
                        start..end
                    });
                    let audio_secs = secs(keep.map_or(samples.len(), |k| k.len()));
                    let samples = crate::audio::resample(&samples, rate, crate::audio::SAMPLE_RATE);
                    let result = self.ensure_loaded().and_then(|()| self.run_piece(&samples, span, language));
                    self.last_used = Instant::now();
                    match result {
                        Ok((text, heard, infer_ms)) => {
                            let model = self.loaded.as_ref().map(|l| l.id.clone()).unwrap_or_default();
                            let _ = self.events.send(EngineEvent::Done(Transcript {
                                request,
                                text,
                                audio_secs,
                                infer_ms,
                                model,
                                heard,
                            }));
                        }
                        Err(e) => {
                            let _ = self.events.send(EngineEvent::Failed {
                                request,
                                error: format!("{e:#}"),
                            });
                        }
                    }
                }
                Cmd::Unload => {
                    if let Some(l) = self.loaded.take() {
                        self.status(EngineStatus::Unloaded(l.id));
                    }
                }
            }
        }
    }

    fn ensure_loaded(&mut self) -> anyhow::Result<()> {
        if self.loaded.is_some() {
            return Ok(());
        }
        let id = self.selected.clone().ok_or_else(|| anyhow!("no model selected"))?;
        let Some(info) = models::find(&id) else {
            self.status(EngineStatus::Missing(id.clone()));
            return Err(anyhow!("unknown model {id}"));
        };
        if !info.is_installed() {
            self.status(EngineStatus::Missing(id.clone()));
            return Err(anyhow!("{} is not downloaded yet", info.name));
        }
        self.status(EngineStatus::Loading(id.clone()));
        let started = Instant::now();
        match load_model(info, &info.dir()) {
            Ok(mut model) => {
                // Warm-up pass: the first inference pays for allocator/kernel
                // setup; do it now instead of on the user's first dictation.
                let _ = model.speech().transcribe(&vec![0.0; 16_000], &TranscribeOptions::default());
                let _ = model.heard(None);
                let pieces = match &model {
                    Model::Parakeet(m) => Some(Arc::new(Pieces::from_vocab(m.vocab()))),
                    Model::Other(_) => None,
                };
                let _ = self.events.send(EngineEvent::Pieces(pieces));
                let load_ms = started.elapsed().as_millis() as u64;
                log::info!("loaded {} in {load_ms} ms", info.id);
                self.loaded = Some(Loaded {
                    id: id.clone(),
                    family: info.family,
                    model,
                });
                self.last_used = Instant::now();
                self.status(EngineStatus::Ready { id, load_ms });
                Ok(())
            }
            Err(e) => {
                let error = format!("{e:#}");
                log::error!("loading {id} failed: {error}");
                self.status(EngineStatus::Failed { id, error: error.clone() });
                Err(anyhow!(error))
            }
        }
    }

    /// Transcribes `samples`, keeping the words that begin within `span`
    /// (seconds into them) if given: its text, words heard and time taken.
    fn run_piece(&mut self, samples: &[f32], span: Option<Range<f32>>, language: Option<String>) -> anyhow::Result<(String, Option<Vec<Heard>>, u64)> {
        let timed = matches!(self.loaded.as_ref().map(|l| &l.model), Some(Model::Parakeet(_)));
        let (result, ms) = match &span {
            // Without word timings, the piece on its own.
            Some(span) if !timed => {
                let at = |t: f32| ((t.max(0.0) * crate::audio::SAMPLE_RATE as f32) as usize).min(samples.len());
                self.run_one(&samples[at(span.start)..at(span.end).max(at(span.start))], language)?
            }
            _ => self.run_one(samples, language)?,
        };
        let model = &mut self.loaded.as_mut().ok_or_else(|| anyhow!("model not loaded"))?.model;
        let tokens = result.segments.as_deref().filter(|_| timed);
        let (text, heard) = match (span, tokens) {
            (Some(span), Some(tokens)) => {
                let (text, kept) = words_within(tokens, &span);
                (text, model.heard(Some(&kept)))
            }
            _ => (result.text.trim().to_string(), model.heard(None)),
        };
        Ok((text, heard, ms))
    }

    fn run_one(&mut self, samples: &[f32], language: Option<String>) -> anyhow::Result<(TranscriptionResult, u64)> {
        let loaded = self.loaded.as_mut().ok_or_else(|| anyhow!("model not loaded"))?;
        let started = Instant::now();
        let options = TranscribeOptions {
            language: language.clone(),
            // A little trailing silence helps decoders finish the last word.
            trailing_silence_ms: Some(200),
            ..Default::default()
        };
        let options = match loaded.family {
            // Canary needs an explicit source language; default to English.
            Family::Canary => TranscribeOptions {
                language: Some(language.unwrap_or_else(|| "en".into())),
                ..options
            },
            _ => options,
        };
        let mut result = loaded.model.speech().transcribe(samples, &options).map_err(|e| anyhow!("{e}"))?;
        // Parakeet can hear nothing at all in a short phrase with little
        // silence around it ("store yesterday" in 1.5 s), which it gets with
        // about a second of silence on both sides and a louder signal. Only
        // an empty result is retried, so nothing it already hears changes.
        if result.text.trim().is_empty() {
            let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let gain = (0.5 / peak.max(1e-6)).min(10.0);
            let louder: Vec<f32> = samples.iter().map(|s| s * gain).collect();
            let roomier = TranscribeOptions {
                leading_silence_ms: Some(1250),
                trailing_silence_ms: Some(1200),
                ..options
            };
            result = loaded.model.speech().transcribe(&louder, &roomier).map_err(|e| anyhow!("{e}"))?;
            if !result.text.trim().is_empty() {
                log::info!("heard nothing at first; heard it with more silence around it");
            }
        }
        Ok((result, started.elapsed().as_millis() as u64))
    }
}

/// The words of a transcription (Parakeet's timed tokens) that begin within
/// `span`, in seconds: their text, and which tokens they are.
fn words_within(tokens: &[TranscriptionSegment], span: &Range<f32>) -> (String, Vec<bool>) {
    let mut kept = Vec::with_capacity(tokens.len());
    let mut inside = false;
    for (i, t) in tokens.iter().enumerate() {
        if i == 0 || t.text.starts_with([' ', '\u{2581}']) {
            inside = span.contains(&t.start);
        }
        kept.push(inside);
    }
    let text = tokens.iter().zip(&kept).filter(|(_, k)| **k).map(|(t, _)| t.text.as_str()).collect::<String>();
    (join_tokens(&text).trim().to_string(), kept)
}

/// Spaces tokens' text as Parakeet's decoder does (`\A\s|\s\B|(\s)\b`): a
/// space only before a word, never at the start or before punctuation.
fn join_tokens(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let word = |c: &char| c.is_alphanumeric() || *c == '_';
    let mut out = String::with_capacity(text.len());
    for (i, c) in chars.iter().enumerate() {
        if !c.is_whitespace() {
            out.push(*c);
        } else if i > 0 && chars.get(i + 1).is_some_and(word) {
            out.push(' ');
        }
    }
    out
}

fn load_model(info: &ModelInfo, dir: &Path) -> anyhow::Result<Model> {
    Ok(match info.family {
        Family::Parakeet => Model::Parakeet(ParakeetModel::load(dir, &Quantization::Int8).map_err(|e| anyhow!("{e}"))?),
        _ => Model::Other(load(info, dir, None)?),
    })
}

/// Loads a model by family. ONNX Runtime picks its intra-op thread count
/// (physical cores) itself.
pub fn load(info: &ModelInfo, dir: &Path, _threads: Option<usize>) -> anyhow::Result<Box<dyn SpeechModel + Send>> {
    let q = Quantization::Int8;
    let model: Box<dyn SpeechModel + Send> = match info.family {
        Family::Parakeet => Box::new(ParakeetModel::load(dir, &q).map_err(|e| anyhow!("{e}"))?),
        Family::Canary => Box::new(CanaryModel::load(dir, &q).map_err(|e| anyhow!("{e}"))?),
        Family::Polish => anyhow::bail!("{} is not a speech model", info.name),
    };
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(timed: &[(&str, f32)]) -> Vec<TranscriptionSegment> {
        timed.iter().map(|(text, start)| TranscriptionSegment { start: *start, end: start + 0.08, text: text.to_string() }).collect()
    }

    #[test]
    fn joins_tokens_as_the_decoder_does() {
        assert_eq!(join_tokens(" Hello, wor ld ."), "Hello, wor ld.");
        assert_eq!(join_tokens(" 10 % of  them"), "10% of them");
        assert_eq!(join_tokens("x "), "x");
    }

    #[test]
    fn keeps_the_words_that_begin_in_the_piece() {
        // "customers, and watch the" with a cut at 1.0 s, between the words.
        let t = tokens(&[(" custom", 0.2), ("ers", 0.5), (",", 0.7), (" and", 1.2), (" wat", 1.5), ("ch", 1.6), (" the", 1.9)]);
        let (text, kept) = words_within(&t, &(f32::NEG_INFINITY..1.0));
        assert_eq!(text, "customers,");
        assert_eq!(kept, [true, true, true, false, false, false, false]);
        assert_eq!(words_within(&t, &(1.0..f32::INFINITY)).0, "and watch the");
        // A word is kept whole by where it begins, even past the end.
        assert_eq!(words_within(&t, &(1.0..1.55)).0, "and watch");
        assert_eq!(words_within(&t, &(f32::NEG_INFINITY..f32::INFINITY)).0, "customers, and watch the");
    }

    /// Needs Parakeet Ultra downloaded; speaks with macOS's `say`.
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn parakeet_hears_a_repeated_word_as_often_as_it_was_said() {
        let info = models::find("parakeet-ultra").unwrap();
        let mut model = load(info, &info.dir(), None).expect("model downloaded");
        let wav = std::env::temp_dir().join(format!("bars-repeat-{}.wav", std::process::id()));
        for (said, times) in [("test test test", 3), ("test test test test test", 5)] {
            let ok = std::process::Command::new("say")
                .args(["-v", "Samantha", "--data-format=LEI16@16000", "-o"])
                .arg(&wav)
                .arg(said)
                .status()
                .unwrap()
                .success();
            assert!(ok, "say");
            let samples = transcribe_rs::audio::read_wav_samples(&wav).unwrap();
            let text = model.transcribe(&samples, &TranscribeOptions::default()).unwrap().text;
            let heard = text.to_lowercase().matches("test").count();
            assert_eq!(heard, times, "{said:?} came out as {text:?}");
        }
        let _ = std::fs::remove_file(&wav);
    }
}
