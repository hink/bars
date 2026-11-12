//! Learning from corrections. When the user fixes a word Bars wrote, and
//! the fix looks like a mishearing put right rather than a rewording, it
//! becomes a dictionary entry. Everything is local: word-level diffing,
//! spelling and sound similarity, and how unsure the speech model was of
//! the word it wrote (Parakeet only).
//!
//! Field text is only ever held in memory; just the learned word pairs are
//! kept.

use std::collections::HashSet;

use crate::config::Replacement;

/// The model's confidence below which a word counts as unsure: on test
/// audio, words it got wrong scored 0.14–0.51, words it got right 0.74+.
pub const UNSURE: f32 = 0.6;

/// A suggestion seen this many times becomes a dictionary entry.
pub const PROMOTE_AFTER: u32 = 2;

/// One word as the model wrote it, and how sure it was (the lowest
/// probability among its letter-bearing pieces; punctuation doesn't count).
#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub word: String,
    pub confidence: f32,
}

/// Groups a decode's tokens (texts from the vocabulary, word starts
/// beginning with a space) into words with confidences.
pub fn heard_words(tokens: &[&str], probs: &[f32]) -> Vec<Heard> {
    let mut out: Vec<Heard> = Vec::new();
    for (tok, &p) in tokens.iter().zip(probs) {
        let starts_word = tok.starts_with(' ') || out.is_empty();
        let letters = tok.chars().any(char::is_alphanumeric);
        if starts_word {
            out.push(Heard {
                word: tok.trim().to_string(),
                confidence: if letters { p } else { 1.0 },
            });
        } else if let Some(w) = out.last_mut() {
            w.word.push_str(tok);
            if letters {
                w.confidence = w.confidence.min(p);
            }
        }
    }
    out
}

/// The model's vocabulary pieces, to tell common words (one piece) from
/// rare ones and names (several pieces).
#[derive(Debug, Default)]
pub struct Pieces {
    set: HashSet<String>,
    max_chars: usize,
}

impl Pieces {
    pub fn from_vocab(vocab: &[String]) -> Pieces {
        // Special tokens (<unk>, <|endoftext|>, ...) are never pieces of text.
        let set: HashSet<String> = vocab
            .iter()
            .filter(|t| !t.is_empty() && !(t.starts_with('<') && t.ends_with('>')))
            .cloned()
            .collect();
        let max_chars = set.iter().map(|t| t.chars().count()).max().unwrap_or(1);
        Pieces { set, max_chars }
    }

    /// How many pieces the model needs for `text`, longest pieces first.
    pub fn count(&self, text: &str) -> usize {
        let mut n = 0;
        for word in text.split_whitespace() {
            let chars: Vec<char> = std::iter::once(' ').chain(word.chars()).collect();
            let mut i = 0;
            while i < chars.len() {
                let len = (1..=self.max_chars.min(chars.len() - i))
                    .rev()
                    .find(|&len| self.set.contains(&chars[i..i + len].iter().collect::<String>()))
                    .unwrap_or(1);
                n += 1;
                i += len;
            }
        }
        n
    }

    /// One piece, lowercase: an everyday word ("there", "good").
    fn is_common(&self, word: &str) -> bool {
        !self.set.is_empty() && word.chars().all(char::is_lowercase) && self.count(word) == 1
    }
}

/// A changed stretch: `from` (as inserted) became `to` (as edited).
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub from: String,
    pub to: String,
    /// The change isn't at the start of a sentence.
    pub mid_sentence: bool,
    /// Where `from` sits in the dictation, as a fraction of its words (to
    /// tell repeated words apart).
    pub at: f32,
}

/// A word with its surrounding punctuation set aside.
struct Word<'a> {
    core: &'a str,
    raw: &'a str,
}

fn split_words(text: &str) -> Vec<Word<'_>> {
    text.split_whitespace()
        .map(|raw| Word {
            core: raw.trim_matches(|c: char| !c.is_alphanumeric()),
            raw,
        })
        .filter(|w| !w.core.is_empty())
        .collect()
}

/// The small changes between what was inserted and what the field holds
/// now: one to three words replaced by one to three, next to an unchanged
/// word. None when more than half the words changed (a rewrite, not a fix).
pub fn find_edits(inserted: &str, edited: &str) -> Option<Vec<Edit>> {
    let a = split_words(inserted);
    let b = split_words(edited);
    if a.is_empty() || b.is_empty() {
        return None;
    }
    // Longest common subsequence of words.
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i].core == b[j].core {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut spans = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i].core == b[j].core {
            i += 1;
            j += 1;
            continue;
        }
        let (si, sj) = (i, j);
        while (i < n || j < m) && !(i < n && j < m && a[i].core == b[j].core) {
            if j < m && (i == n || lcs[i][j + 1] >= lcs[i + 1][j]) {
                j += 1;
            } else {
                i += 1;
            }
        }
        spans.push((si..i, sj..j));
    }
    let changed: usize = spans.iter().map(|(x, _)| x.len()).sum();
    if changed * 2 > n {
        return None;
    }
    let edits = spans
        .into_iter()
        .filter(|(x, y)| (1..=3).contains(&x.len()) && (1..=3).contains(&y.len()))
        // An unchanged word next to it anchors the fix.
        .filter(|(x, _)| x.start > 0 || x.end < n)
        .map(|(x, y)| Edit {
            from: a[x.clone()].iter().map(|w| w.core).collect::<Vec<_>>().join(" "),
            to: b[y].iter().map(|w| w.core).collect::<Vec<_>>().join(" "),
            mid_sentence: x.start > 0 && !a[x.start - 1].raw.ends_with(['.', '!', '?']),
            at: x.start as f32 / n as f32,
        })
        .collect();
    Some(edits)
}

/// The model's confidence in `edit.from`, found among the words it heard:
/// of repeated words, the one at the same place (clean-up may have changed
/// the text; then it's unknown).
pub fn confidence_of(edit: &Edit, heard: &[Heard]) -> Option<f32> {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let want: Vec<String> = edit.from.split_whitespace().map(norm).collect();
    let words: Vec<String> = heard.iter().map(|h| norm(&h.word)).collect();
    let expected = edit.at * words.len() as f32;
    words
        .windows(want.len())
        .enumerate()
        .filter(|(_, w)| *w == want.as_slice())
        .map(|(i, _)| i)
        .min_by(|a, b| (*a as f32 - expected).abs().total_cmp(&(*b as f32 - expected).abs()))
        .map(|i| heard[i..i + want.len()].iter().map(|h| h.confidence).fold(1.0, f32::min))
}

/// Finds a dictation again in its field while the user edits it, by its
/// own words. The cursor position can't be trusted everywhere (Ghostty
/// reports it at the top of the screen), and terminal programs redraw
/// boxes and wrap lines around what was typed.
#[derive(Clone, Debug)]
pub struct Tracker {
    words: Vec<String>,
    /// Words right before and after the dictation, once seen as inserted.
    context: Option<(Vec<String>, Vec<String>)>,
}

/// Neighbouring words that pin a dictation down.
const CONTEXT_WORDS: usize = 2;
/// How many words longer or shorter an edited dictation may be.
const MAX_GROWTH: usize = 6;

/// Box-drawing and block characters terminal programs draw around text.
fn decoration(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259F}' | '\u{25A0}'..='\u{25FF}')
}

/// The words of `text` as written, and their lowercase letters and digits
/// (to compare); decorations and symbol-only tokens drop out.
fn field_words(text: &str) -> Vec<(&str, String)> {
    text.split(|c: char| c.is_whitespace() || decoration(c))
        .filter_map(|raw| {
            let core: String = raw
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            (!core.is_empty()).then_some((raw, core))
        })
        .collect()
}

impl Tracker {
    pub fn new(inserted: &str) -> Tracker {
        Tracker {
            words: field_words(inserted).into_iter().map(|(_, c)| c).collect(),
            context: None,
        }
    }

    /// The dictation as it reads now in `text` (the field's contents), or
    /// None when it can't be found.
    pub fn locate(&mut self, text: &str) -> Option<String> {
        let words = field_words(text);
        let cores: Vec<&str> = words.iter().map(|(_, c)| c.as_str()).collect();
        let n = self.words.len();
        if n == 0 || cores.is_empty() {
            return None;
        }
        let join = |range: std::ops::Range<usize>| words[range].iter().map(|(raw, _)| *raw).collect::<Vec<_>>().join(" ");
        let Some((before, after)) = &self.context else {
            // First sighting: exactly as inserted. The last one (lowest on
            // a terminal screen, the newest) if it appears more than once.
            let pos = (0..=cores.len().saturating_sub(n)).rev().find(|&i| {
                cores.len() >= i + n && cores[i..i + n].iter().zip(&self.words).all(|(a, b)| a == b)
            })?;
            let before = cores[pos.saturating_sub(CONTEXT_WORDS)..pos].iter().map(|s| s.to_string()).collect();
            let after = cores[pos + n..(pos + n + CONTEXT_WORDS).min(cores.len())].iter().map(|s| s.to_string()).collect();
            self.context = Some((before, after));
            return Some(join(pos..pos + n));
        };
        // Later: whatever sits between the same neighbours.
        let matches_at = |i: usize, ctx: &[String]| {
            i + ctx.len() <= cores.len() && cores[i..i + ctx.len()].iter().zip(ctx).all(|(a, b)| *a == b.as_str())
        };
        let starts: Vec<usize> = if before.is_empty() {
            vec![0]
        } else {
            (0..cores.len()).filter(|&i| matches_at(i, before)).map(|i| i + before.len()).collect()
        };
        starts
            .into_iter()
            .filter_map(|start| {
                let end = if after.is_empty() {
                    cores.len()
                } else {
                    (start..=(start + n + MAX_GROWTH).min(cores.len())).find(|&j| matches_at(j, after))?
                };
                (end > start && (end - start).abs_diff(n) <= MAX_GROWTH).then_some((start, end))
            })
            // Several: the one closest in length, then the newest.
            .min_by_key(|&(start, end)| ((end - start).abs_diff(n), std::cmp::Reverse(start)))
            .map(|(start, end)| join(start..end))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A clear fix: add it to the dictionary now.
    Learn,
    /// Plausible: suggest it, and learn it once seen again.
    Suggest,
    Ignore(&'static str),
}

/// Decides what to do with an edit. `confidence` is the model's (None when
/// unknown); `known` holds pairs already in the dictionary or turned down.
pub fn judge(edit: &Edit, confidence: Option<f32>, pieces: &Pieces, known: &[Replacement]) -> Verdict {
    let (from, to) = (edit.from.as_str(), edit.to.as_str());
    let (from_lc, to_lc) = (from.to_lowercase(), to.to_lowercase());
    if from == to {
        return Verdict::Ignore("unchanged");
    }
    if known.iter().any(|r| r.from.eq_ignore_ascii_case(from) && r.to == to) {
        return Verdict::Ignore("already known");
    }
    if from.chars().chain(to.chars()).any(|c| c.is_ascii_digit()) {
        return Verdict::Ignore("numbers");
    }
    // A rule for an everyday word would fire everywhere ("there" → "their",
    // "will" → "Will"). Rules match whatever the capitals, so the word
    // counts as it reads in lowercase: "Your" at the start of a sentence is
    // as common as "your".
    let common = !from.contains(' ') && pieces.is_common(&from_lc);
    if from_lc == to_lc {
        // Only capitalization: a name, if it's mid-sentence.
        return if common {
            Verdict::Ignore("common word")
        } else if edit.mid_sentence && to.starts_with(char::is_uppercase) {
            Verdict::Suggest
        } else {
            Verdict::Ignore("capitalization")
        };
    }
    if grammar_only(&from_lc, &to_lc) {
        return Verdict::Ignore("grammar");
    }
    if common {
        return Verdict::Ignore("common word");
    }
    let similar = compact(&from_lc) == compact(&to_lc)
        || similarity(&from_lc, &to_lc) >= 0.6
        || sounds_alike(&from_lc, &to_lc);
    if !similar {
        return Verdict::Ignore("reworded");
    }
    // Names, jargon and words the model needs several pieces for. The
    // capital a sentence starts with doesn't make a name.
    let to = if edit.mid_sentence { to.to_string() } else { lowercase_first(to) };
    let rare = to.chars().any(char::is_uppercase)
        || pieces.count(&to) >= 3 * to.split_whitespace().count().max(1);
    let unsure = confidence.is_some_and(|c| c < UNSURE);
    match (unsure, rare) {
        (true, true) => Verdict::Learn,
        (true, false) | (false, true) => Verdict::Suggest,
        (false, false) => Verdict::Ignore("the model was sure"),
    }
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Contractions, possessives and simple inflections: grammar, not hearing.
fn grammar_only(from: &str, to: &str) -> bool {
    let strip = |s: &str| s.replace(['\'', '’'], "");
    if strip(from) == strip(to) {
        return true;
    }
    let (short, long) = if from.len() <= to.len() { (from, to) } else { (to, from) };
    long.strip_prefix(short)
        .is_some_and(|rest| ["s", "es", "ed", "d", "ing", "'s", "’s", "ly"].contains(&rest))
}

/// Letters and digits only: "chat gpt" and "ChatGPT" compare equal.
fn compact(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// 1 − edit distance / length of the longer string.
fn similarity(a: &str, b: &str) -> f32 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    1.0 - prev[b.len()] as f32 / longest as f32
}

/// A rough English sound key: consonants by sound, voiced and unvoiced
/// pairs merged (what speech models mix up), vowels dropped after the
/// first letter. "cooper netties" and "Kubernetes" both give "kprnts".
fn sound_key(s: &str) -> String {
    let w: Vec<char> = s.chars().filter(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_lowercase()).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < w.len() {
        let next = w.get(i + 1).copied();
        let soft = matches!(next, Some('e' | 'i' | 'y'));
        let (code, skip): (Option<char>, usize) = match w[i] {
            'a' | 'e' | 'i' | 'o' | 'u' | 'y' => (if i == 0 { Some('a') } else { None }, 1),
            'p' if next == Some('h') => (Some('f'), 2),
            'c' if next == Some('k') => (Some('k'), 2),
            'c' if next == Some('h') => (Some('x'), 2),
            's' if next == Some('h') => (Some('x'), 2),
            't' if next == Some('h') => (Some('0'), 2),
            'w' if next == Some('h') => (Some('w'), 2),
            'g' if next == Some('h') => (None, 2),
            'k' | 'g' | 'w' if i == 0 && next == Some('n') => (None, 1),
            'w' if i == 0 && next == Some('r') => (None, 1),
            'c' => (Some(if soft { 's' } else { 'k' }), 1),
            'g' => (Some(if soft { 'j' } else { 'k' }), 1),
            'b' => (Some('p'), 1),
            'd' => (Some('t'), 1),
            'v' => (Some('f'), 1),
            'z' => (Some('s'), 1),
            'q' => (Some('k'), 1),
            'x' => (Some('s'), 1),
            'h' => (None, 1),
            c => (Some(c), 1),
        };
        if let Some(c) = code
            && !out.ends_with(c)
        {
            out.push(c);
        }
        i += skip;
    }
    out
}

fn sounds_alike(a: &str, b: &str) -> bool {
    let (ka, kb) = (sound_key(a), sound_key(b));
    ka.len().max(kb.len()) >= 3 && (ka == kb || similarity(&ka, &kb) >= 0.75)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces() -> Pieces {
        let vocab: Vec<String> = [" there", " their", " good", " great", " the", " cooper", " net", "ties", " K", "uber", "net", "es", " chat", " g", "pt", " is", " a", " tool", " county", " country"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        Pieces::from_vocab(&vocab)
    }

    fn edit(from: &str, to: &str) -> Edit {
        Edit { from: from.into(), to: to.into(), mid_sentence: true, at: 0.5 }
    }

    #[test]
    fn groups_pieces_into_words_with_confidence() {
        let heard = heard_words(&[" K", "uber", "net", "es", ",", " is"], &[0.9, 0.3, 0.8, 0.95, 0.1, 0.99]);
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0].word, "Kubernetes,");
        // The comma's low score doesn't count.
        assert_eq!(heard[0].confidence, 0.3);
    }

    #[test]
    fn finds_small_fixes_and_skips_rewrites() {
        let e = find_edits("We use cooper netties for this.", "We use Kubernetes for this.").unwrap();
        assert_eq!(e, vec![Edit { from: "cooper netties".into(), to: "Kubernetes".into(), mid_sentence: true, at: 2.0 / 6.0 }]);
        // Punctuation around a word isn't a change.
        assert_eq!(find_edits("Hello, world.", "Hello world!").unwrap(), vec![]);
        // More than half changed: a rewrite.
        assert!(find_edits("This is fine", "That was not okay").is_none());
        // A one-word dictation changed entirely is a rewrite too.
        assert!(find_edits("Chris", "Kris").is_none());
    }

    #[test]
    fn learns_unsure_fixes_of_rare_words() {
        let p = pieces();
        assert_eq!(judge(&edit("cooper netties", "Kubernetes"), Some(0.3), &p, &[]), Verdict::Learn);
        assert_eq!(judge(&edit("cooper netties", "Kubernetes"), Some(0.9), &p, &[]), Verdict::Suggest);
        assert_eq!(judge(&edit("chat gpt", "ChatGPT"), None, &p, &[]), Verdict::Suggest);
        assert_eq!(judge(&edit("chris", "Chris"), Some(0.5), &p, &[]), Verdict::Suggest);
    }

    #[test]
    fn ignores_rewordings_grammar_and_risky_rules() {
        let p = pieces();
        assert_eq!(judge(&edit("good", "great"), Some(0.3), &p, &[]), Verdict::Ignore("common word"));
        assert_eq!(judge(&edit("there", "their"), Some(0.3), &p, &[]), Verdict::Ignore("common word"));
        assert_eq!(judge(&edit("tools", "hammers"), Some(0.3), &p, &[]), Verdict::Ignore("reworded"));
        assert_eq!(judge(&edit("dont", "don't"), Some(0.3), &p, &[]), Verdict::Ignore("grammar"));
        assert_eq!(judge(&edit("tool", "tools"), Some(0.3), &p, &[]), Verdict::Ignore("grammar"));
        assert_eq!(judge(&edit("version 2", "version 3"), Some(0.3), &p, &[]), Verdict::Ignore("numbers"));
        let known = [Replacement { from: "cooper netties".into(), to: "Kubernetes".into(), learned: true }];
        assert_eq!(judge(&edit("cooper netties", "Kubernetes"), Some(0.3), &p, &known), Verdict::Ignore("already known"));
        let start = Edit { mid_sentence: false, ..edit("chris", "Chris") };
        assert_eq!(judge(&start, Some(0.3), &p, &[]), Verdict::Ignore("capitalization"));
    }

    #[test]
    fn a_sentence_starts_capital_makes_no_rule() {
        let p = pieces();
        let start = |from: &str, to: &str| Edit { mid_sentence: false, ..edit(from, to) };
        // An everyday word, whatever its capitals: the rule would rewrite
        // every "there".
        assert_eq!(judge(&start("There", "Their"), Some(0.3), &p, &[]), Verdict::Ignore("common word"));
        assert_eq!(judge(&edit("There", "Their"), Some(0.3), &p, &[]), Verdict::Ignore("common word"));
        // Capitalizing one is no name either.
        assert_eq!(judge(&edit("good", "Good"), Some(0.9), &p, &[]), Verdict::Ignore("common word"));
        // The fix isn't a rare word just for starting the sentence.
        assert_eq!(judge(&start("Thier", "Their"), Some(0.9), &p, &[]), Verdict::Ignore("the model was sure"));
        assert_eq!(judge(&start("Thier", "Their"), Some(0.3), &p, &[]), Verdict::Suggest);
        // Mid-sentence, a capital still marks a name.
        assert_eq!(judge(&edit("kris", "Chris"), Some(0.9), &p, &[]), Verdict::Suggest);
    }

    #[test]
    fn finds_the_models_confidence_in_the_changed_words() {
        let heard = vec![
            Heard { word: "We".into(), confidence: 0.99 },
            Heard { word: "use".into(), confidence: 0.97 },
            Heard { word: "cooper".into(), confidence: 0.4 },
            Heard { word: "netties,".into(), confidence: 0.3 },
        ];
        assert_eq!(confidence_of(&edit("cooper netties", "Kubernetes"), &heard), Some(0.3));
        assert_eq!(confidence_of(&edit("kettle", "Kotlin"), &heard), None);
        // A repeated word: the occurrence at the edit's place.
        let heard = vec![
            Heard { word: "and".into(), confidence: 0.98 },
            Heard { word: "so".into(), confidence: 0.9 },
            Heard { word: "and".into(), confidence: 0.33 },
            Heard { word: "not".into(), confidence: 0.99 },
        ];
        let second = Edit { at: 0.5, ..edit("and", "ask") };
        assert_eq!(confidence_of(&second, &heard), Some(0.33));
    }

    #[test]
    fn finds_the_dictation_again_in_its_field() {
        let mut t = Tracker::new("We use cooper netties for this.");
        // First sighting, inside a terminal program's box.
        let screen = "│ > hello there │\n│ We use cooper netties │\n│ for this. │\n╰──────╯ ? for shortcuts";
        assert_eq!(t.locate(screen).as_deref(), Some("We use cooper netties for this."));
        // Edited and rewrapped: found between the same neighbours.
        let screen = "│ > hello there │\n│ We use Kubernetes for │\n│ this. │\n╰──────╯ ? for shortcuts";
        assert_eq!(t.locate(screen).as_deref(), Some("We use Kubernetes for this."));
        // Gone (the message was sent).
        assert_eq!(t.locate("│ > │\n╰──────╯ ? for shortcuts").as_deref(), None);
        // Never seen as inserted: not found.
        let mut t = Tracker::new("Something else");
        assert_eq!(t.locate("Nothing like it here"), None);
        // At the start of an empty field.
        let mut t = Tracker::new("Hello there");
        assert_eq!(t.locate("Hello there").as_deref(), Some("Hello there"));
        assert_eq!(t.locate("Hello their").as_deref(), Some("Hello their"));
    }

    #[test]
    fn sound_keys_match_mishearings() {
        assert_eq!(sound_key("cooper netties"), sound_key("Kubernetes"));
        assert!(sounds_alike("jason", "json"));
        assert!(!sounds_alike("good", "great"));
        assert_eq!(pieces().count("Kubernetes"), 4);
        assert_eq!(pieces().count("there"), 1);
    }
}
