//! Local, deterministic clean-up of raw transcripts: filler removal, spoken
//! line-break commands, personal dictionary, snippets, and spacing.

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};

use crate::config::{Replacement, Snippet, TextConfig};

static FILLERS: Lazy<Regex> = Lazy::new(|| {
    // Leading comma/space handling keeps "So, um, I think" -> "So, I think".
    Regex::new(r"(?i)(?:,\s*)?\b(u+h+m*|u+m+|e+r+m+|e+h+|a+h+|h+m+|m+h*m+)\b[,.]?").unwrap()
});
static MULTI_SPACE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t]{2,}").unwrap());
static SPACE_BEFORE_PUNCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+[,.;:!?]").unwrap());
static DOUBLE_COMMA: Lazy<Regex> = Lazy::new(|| Regex::new(r",\s*([,.;:!?])").unwrap());
static NEW_PARAGRAPH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\s*\bnew paragraph\b[,.]?\s*").unwrap());
static NEW_LINE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\s*\bnew line\b[,.]?\s*").unwrap());

pub struct TextProcessor {
    remove_fillers: bool,
    voice_commands: bool,
    replacements: Vec<(Regex, String)>,
    snippets: Vec<(Regex, String)>,
}

/// Private-use characters, which no speech model writes: one stands in for
/// each snippet's expansion while the text around it is tidied, so the
/// expansion goes in exactly as written.
const STAND_INS: std::ops::RangeInclusive<char> = '\u{E000}'..='\u{F8FF}';

fn stand_in(snippet: usize) -> Option<char> {
    let c = char::from_u32(*STAND_INS.start() as u32 + u32::try_from(snippet).ok()?)?;
    STAND_INS.contains(&c).then_some(c)
}

fn phrase_regex(phrase: &str) -> Option<Regex> {
    let words: Vec<String> = phrase
        .split_whitespace()
        .map(|w| regex::escape(w.trim_matches(|c: char| c.is_ascii_punctuation())))
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return None;
    }
    // Words may be separated by whitespace and/or punctuation the model added
    // ("chat, gpt"). Boundaries are Unicode-aware.
    let body = words.join(r"[\s,.-]+");
    RegexBuilder::new(&format!(r"\b{body}\b"))
        .case_insensitive(true)
        .build()
        .ok()
}

impl TextProcessor {
    pub fn new(cfg: &TextConfig) -> Self {
        let compile = |items: Vec<(&str, &str)>| -> Vec<(Regex, String)> {
            let mut out: Vec<(Regex, String, usize)> = items
                .into_iter()
                .filter_map(|(from, to)| phrase_regex(from).map(|r| (r, to.to_string(), from.len())))
                .collect();
            // Longest phrases first so "new york times" wins over "new york".
            out.sort_by_key(|item| std::cmp::Reverse(item.2));
            out.into_iter().map(|(r, t, _)| (r, t)).collect()
        };
        TextProcessor {
            remove_fillers: cfg.remove_fillers,
            voice_commands: cfg.voice_commands,
            replacements: compile(
                cfg.dictionary
                    .iter()
                    .map(|Replacement { from, to, .. }| (from.as_str(), to.as_str()))
                    .collect(),
            ),
            snippets: compile(
                cfg.snippets
                    .iter()
                    .map(|Snippet { trigger, expansion }| (trigger.as_str(), expansion.as_str()))
                    .collect(),
            ),
        }
    }

    /// `english`: the dictation is in English. The fillers removed are
    /// English ones, and words in other languages ("um" in Portuguese and
    /// German).
    pub fn process(&self, raw: &str, english: bool) -> String {
        let mut text = raw.trim().replace(|c: char| STAND_INS.contains(&c), "");
        if text.is_empty() {
            return text;
        }

        if self.remove_fillers && english {
            text = remove_fillers(&text);
        }

        for (re, to) in &self.replacements {
            text = replace_words(re, &text, to);
        }

        // Snippets: if the whole utterance is just the cue, emit the expansion
        // verbatim (no added punctuation); otherwise expand inline.
        for (i, (re, expansion)) in self.snippets.iter().enumerate() {
            let bare = text.trim_matches(|c: char| c.is_ascii_punctuation() || c.is_whitespace());
            if re.find(bare).is_some_and(|m| m.start() == 0 && m.end() == bare.len()) {
                return expansion.clone();
            }
            let with = stand_in(i).map_or_else(|| expansion.clone(), String::from);
            text = re.replace_all(&text, regex::NoExpand(&with)).into_owned();
        }

        if self.voice_commands {
            text = NEW_PARAGRAPH.replace_all(&text, "\n\n").into_owned();
            text = NEW_LINE.replace_all(&text, "\n").into_owned();
        }

        text = tidy(&text);
        if self.voice_commands {
            text = capitalize_after_breaks(&text);
        }
        if text.contains(|c: char| STAND_INS.contains(&c)) {
            for (i, (_, expansion)) in self.snippets.iter().enumerate() {
                if let Some(c) = stand_in(i) {
                    text = text.replace(c, expansion);
                }
            }
        }
        text
    }
}

/// Removes "um", "uh" and the like, except where the same letters are part
/// of a word or a unit: joined by a hyphen ("uh-huh", "mm-hmm") or after a
/// number ("5 mm"). A filler that started a sentence hands its capital to
/// the word after it ("Um, the next" -> "The next").
fn remove_fillers(text: &str) -> String {
    fn push(out: &mut String, chunk: &str, capitalize: &mut bool) {
        let start = chunk.len() - chunk.trim_start().len();
        if *capitalize && start < chunk.len() {
            out.push_str(&chunk[..start]);
            out.push_str(&capitalize_first(&chunk[start..]));
            *capitalize = false;
        } else {
            out.push_str(chunk);
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut capitalize = false;
    for caps in FILLERS.captures_iter(text) {
        let (Some(all), Some(word)) = (caps.get(0), caps.get(1)) else { continue };
        let (before, after) = (&text[..word.start()], &text[word.end()..]);
        let hyphenated = before.ends_with('-') || after.starts_with('-');
        let after_number = before.trim_end().ends_with(|c: char| c.is_ascii_digit());
        if hyphenated || after_number {
            continue;
        }
        push(&mut out, &text[last..all.start()], &mut capitalize);
        capitalize |= word.as_str().starts_with(char::is_uppercase);
        last = all.end();
    }
    push(&mut out, &text[last..], &mut capitalize);
    out
}

/// Replaces whole-word matches, except inside a contraction ("don" in
/// "don't"): the regex crate has no lookaround to say so.
fn replace_words(re: &Regex, text: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in re.find_iter(text) {
        let apostrophe = |c: char| c == '\'' || c == '’';
        let mut after = text[m.end()..].chars();
        let mut before = text[..m.start()].chars().rev();
        let joined_after = after.next().is_some_and(apostrophe) && after.next().is_some_and(char::is_alphabetic);
        let joined_before = before.next().is_some_and(apostrophe) && before.next().is_some_and(char::is_alphabetic);
        if joined_after || joined_before {
            continue;
        }
        out.push_str(&text[last..m.start()]);
        out.push_str(to);
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

fn tidy(text: &str) -> String {
    let mut t = MULTI_SPACE.replace_all(text, " ").into_owned();
    t = close_up_punctuation(&t);
    t = DOUBLE_COMMA.replace_all(&t, "$1").into_owned();
    // Not `lines()`: a break that ends the text ("... New line.") stays.
    let t = t.split('\n').map(str::trim).collect::<Vec<_>>().join("\n");
    let t = t.trim_matches(|c: char| c == ' ' || c == ',').to_string();
    capitalize_first(&t)
}

/// Drops the space before punctuation ("word ." -> "word."), but not before
/// a word that starts with it (".env", ".NET").
fn close_up_punctuation(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for m in SPACE_BEFORE_PUNCT.find_iter(text) {
        if text[m.end()..].starts_with(char::is_alphanumeric) {
            continue;
        }
        out.push_str(&text[last..m.start()]);
        out.push_str(&text[m.end() - 1..m.end()]);
        last = m.end();
    }
    out.push_str(&text[last..]);
    out
}

/// Capitalizes the first letter, unless the first word is written its own
/// way ("iPhone", "x86") or is an address or a file name.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_lowercase() && !keeps_its_case(s) => c.to_uppercase().collect::<String>() + chars.as_str(),
        _ => s.to_string(),
    }
}

fn keeps_its_case(s: &str) -> bool {
    let word = s.split_whitespace().next().unwrap_or("").trim_end_matches([',', '.', ';', ':', '!', '?']);
    word.chars().skip(1).any(|c| c.is_uppercase() || c.is_numeric())
        || word.contains(['@', '_', '/', '\\'])
        || word.split(['.', ':']).filter(|part| !part.is_empty()).count() > 1
}

fn capitalize_after_breaks(s: &str) -> String {
    s.split('\n').map(capitalize_first).collect::<Vec<_>>().join("\n")
}

/// Joins text onto whatever came before. Adds a trailing space when asked
/// (and the text doesn't end in a line break).
pub fn finalize(text: &str, trailing_space: bool) -> String {
    if trailing_space && !text.is_empty() && !text.ends_with('\n') {
        format!("{text} ")
    } else {
        text.to_string()
    }
}

pub fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The text around the cursor in the field a dictation goes into.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Surroundings {
    pub before: String,
    /// Text the dictation replaces.
    pub selected: String,
    pub after: String,
}

/// Skipped when looking back for the word the cursor continues from.
const SKIPPABLE: &str = "—`,-/:;()[]{} ";
/// Closing punctuation that a space may follow.
const CLOSING: &str = "!%),.:;>?]}";
/// Operators that a space follows only when one comes before them ("a - ").
const OPERATORS: &str = "\\/|-&*+=~";
/// Full-width punctuation, which never takes a space after it.
const CJK_PUNCT: &str = "。、，．！？；：…‥「」『』（）〔〕【】《》〈〉〝〞〟・";

fn letter_or_digit(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric()
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}' | '\u{FF66}'..='\u{FF9F}' | '\u{20000}'..='\u{2FFFF}')
}

/// The cursor sits mid-sentence: the last word before it (skipping commas,
/// dashes, brackets and spaces) is a letter or digit, not a full stop.
fn continues_sentence(before_line: &str) -> bool {
    for c in before_line.trim_end_matches([' ', '\u{a0}']).chars().rev() {
        if letter_or_digit(c) {
            return true;
        }
        if !SKIPPABLE.contains(c) {
            return false;
        }
    }
    false
}

/// The sentence carries on after the cursor (lowercase text or a comma).
fn sentence_carries_on(after_line: &str) -> bool {
    for c in after_line.trim_start_matches([' ', '\u{a0}']).chars() {
        if c.is_alphabetic() && !c.is_uppercase() {
            return true;
        }
        if "!%,.:;>?".contains(c) {
            return true;
        }
        if !SKIPPABLE.contains(c) {
            return false;
        }
    }
    false
}

fn needs_leading_space(before_line: &str) -> bool {
    let mut rev = before_line.chars().rev();
    let Some(last) = rev.next() else { return false };
    if is_cjk(last) {
        return false;
    }
    letter_or_digit(last) || CLOSING.contains(last) || (OPERATORS.contains(last) && matches!(rev.next(), None | Some(' ')))
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match (chars.next(), s.chars().nth(1)) {
        (Some(c), Some(_)) => c.to_lowercase().collect::<String>() + chars.as_str(),
        _ => s.to_string(),
    }
}

fn strip_final_stop(s: &str) -> String {
    s.trim_end_matches(['.', '!', '?']).to_string()
}

/// Whether the first word must keep its capital: "I", acronyms and names
/// with inner capitals, dictionary words, and words the field itself
/// capitalizes mid-sentence (names already written there).
fn starts_with_name(text: &str, names: &[String], field: &Surroundings) -> bool {
    let first = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '’');
    if first.is_empty() || !first.starts_with(char::is_uppercase) {
        return false;
    }
    if first == "I" || first.starts_with("I'") || first.starts_with("I’") {
        return true;
    }
    if first.chars().skip(1).any(char::is_uppercase) {
        return true;
    }
    if names.iter().any(|n| n.split_whitespace().next() == Some(first)) {
        return true;
    }
    [field.before.as_str(), field.after.as_str()].iter().any(|ctx| {
        ctx.match_indices(first).any(|(i, _)| {
            let whole_word = !ctx[..i].ends_with(char::is_alphanumeric)
                && !ctx[i + first.len()..].starts_with(char::is_alphanumeric);
            // Capitalized because it's a name, not because a sentence starts.
            let prev = ctx[..i].trim_end().chars().next_back();
            whole_word && prev.is_some_and(|c| letter_or_digit(c) || c == ',')
        })
    })
}

/// Fits a dictation into the text around the cursor: a leading space only
/// after a word or closing punctuation, a lowercase start when it continues
/// a sentence, no final full stop when the sentence carries on, and a
/// trailing space only where the next character wants one. At the end of a
/// line, `trailing_space` decides.
pub fn fit_to_field(text: &str, field: &Surroundings, trailing_space: bool, names: &[String]) -> String {
    let text = text.trim_matches(|c: char| c == ' ' || c == '\u{a0}');
    if text.is_empty() {
        return String::new();
    }
    let keep_case = starts_with_name(text, names, field);
    let mut out = text.to_string();

    if !field.selected.is_empty() {
        let sel = field.selected.trim();
        if !keep_case && sel.chars().next().is_some_and(|c| c.is_alphabetic() && c.is_lowercase()) {
            out = lowercase_first(&out);
        }
        if !sel.chars().next_back().is_some_and(|c| CLOSING.contains(c)) {
            out = strip_final_stop(&out);
        }
        if field.selected.starts_with(' ') {
            out.insert(0, ' ');
        }
        if field.selected.ends_with(' ') {
            out.push(' ');
        }
        return out;
    }

    let before_line = field.before.rsplit('\n').next().unwrap_or("");
    let after_line = field.after.split('\n').next().unwrap_or("");
    if !keep_case && continues_sentence(before_line) {
        out = lowercase_first(&out);
    }
    if sentence_carries_on(after_line) {
        out = strip_final_stop(&out);
    }
    if needs_leading_space(before_line) {
        out.insert(0, ' ');
    }
    let mut next = after_line.chars();
    let space_after = match next.next() {
        None => trailing_space,
        Some(c) => letter_or_digit(c) || (OPERATORS.contains(c) && next.next() == Some(' ')),
    };
    if space_after && !out.ends_with('\n') && !out.ends_with(|c: char| CJK_PUNCT.contains(c)) {
        out.push(' ');
    }
    out
}

/// A coding agent running in the focused terminal.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Droid,
    Pi,
    OpenCode,
}

/// Splits a long paste so a terminal coding agent shows it as text rather
/// than collapsing it into a "[Pasted text]" placeholder. The pieces join
/// back into exactly `text`.
pub fn split_for_agent(text: &str, agent: Agent) -> Vec<String> {
    let chars = text.chars().count();
    let breaks = text.matches('\n').count();
    // Whether the agent shows the paste whole, and the pieces it gets
    // otherwise. The JavaScript agents count UTF-16 units (an emoji is
    // two), hence pieces a little under their limits.
    let (whole, max_chars, max_breaks) = match agent {
        Agent::ClaudeCode => (chars <= 600 && breaks < 3, 600, 2),
        Agent::Codex => (chars <= 1000, 1000, usize::MAX),
        // Collapses above 400 characters or 8 lines.
        Agent::Droid => (chars <= 400 && breaks < 8, 350, 7),
        // Collapses above 1000 characters or 10 lines.
        Agent::Pi => (chars <= 1000 && breaks < 10, 900, 9),
        // Collapses above 150 characters or from 3 lines.
        Agent::OpenCode => (chars <= 150 && breaks < 2, 140, 1),
    };
    if whole { vec![text.to_string()] } else { split_at_lines(text, max_chars, max_breaks) }
}

/// Pieces of at most `max_chars` characters and `max_breaks` line breaks,
/// cut at line breaks (a break that ends a piece starts the next one) and
/// inside a line only when it doesn't fit in a piece of its own.
fn split_at_lines(text: &str, max_chars: usize, max_breaks: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let (mut chars, mut breaks) = (0, 0);
    for (i, line) in text.split('\n').enumerate() {
        let brk = usize::from(i > 0);
        let len = line.chars().count();
        if !cur.is_empty() && (chars + brk + len > max_chars || breaks + brk > max_breaks) {
            pieces.push(std::mem::take(&mut cur));
            (chars, breaks) = (0, 0);
        }
        if brk == 1 {
            cur.push('\n');
            chars += 1;
            breaks += 1;
        }
        let mut rest = line;
        while chars + rest.chars().count() > max_chars {
            let room = max_chars.saturating_sub(chars);
            let cut = rest.char_indices().nth(room).map_or(rest.len(), |(b, _)| b);
            cur.push_str(&rest[..cut]);
            rest = &rest[cut..];
            pieces.push(std::mem::take(&mut cur));
            (chars, breaks) = (0, 0);
        }
        cur.push_str(rest);
        chars += rest.chars().count();
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(cfg: TextConfig) -> TextProcessor {
        TextProcessor::new(&cfg)
    }

    #[test]
    fn removes_fillers() {
        let p = proc(TextConfig::default());
        assert_eq!(p.process("Um, so I think, uh, we should go.", true), "So I think we should go.");
        assert_eq!(p.process("uhh hmm okay", true), "Okay");
        // Doesn't eat real words.
        assert_eq!(p.process("The umbrella is here.", true), "The umbrella is here.");
    }

    #[test]
    fn line_commands() {
        let p = proc(TextConfig::default());
        assert_eq!(
            p.process("Dear team, new paragraph thanks for coming. New line Chris", true),
            "Dear team,\n\nThanks for coming.\nChris"
        );
    }

    #[test]
    fn dictionary_and_snippets() {
        let mut cfg = TextConfig::default();
        cfg.dictionary.push(Replacement {
            from: "chat gpt".into(),
            to: "ChatGPT".into(),
            learned: false,
        });
        cfg.dictionary.push(Replacement {
            from: "don".into(),
            to: "Don".into(),
            learned: true,
        });
        cfg.snippets.push(Snippet {
            trigger: "my email".into(),
            expansion: "me@example.xyz".into(),
        });
        let p = proc(cfg);
        assert_eq!(p.process("I asked Chat, GPT about it.", true), "I asked ChatGPT about it.");
        // Not inside contractions.
        assert_eq!(p.process("I don't know don.", true), "I don't know Don.");
        assert_eq!(p.process("My email.", true), "me@example.xyz");
        assert_eq!(p.process("Send it to my email please.", true), "Send it to me@example.xyz please.");
    }

    fn field(before: &str, after: &str) -> Surroundings {
        Surroundings { before: before.into(), selected: String::new(), after: after.into() }
    }

    #[test]
    fn fits_into_the_sentence_around_the_cursor() {
        let fit = |b: &str, a: &str, t: &str| fit_to_field(t, &field(b, a), true, &[]);
        // Start of an empty field: as dictated, plus the end-of-line space.
        assert_eq!(fit("", "", "Hello there."), "Hello there. ");
        // After a word: space before, lowercase start.
        assert_eq!(fit("I think", "", "We should go."), " we should go. ");
        // After a comma too; after a full stop the capital stays.
        assert_eq!(fit("Well,", "", "Maybe."), " maybe. ");
        assert_eq!(fit("Done.", "", "Next one."), " Next one. ");
        // Already a space before: none added.
        assert_eq!(fit("Done. ", "", "Next."), "Next. ");
        // Lowercase text follows: no full stop, and a space before it.
        assert_eq!(fit("I said ", "and left", "Hello."), "hello ");
        // Punctuation follows: no space before it.
        assert_eq!(fit("Say ", ", please", "Hello."), "hello");
        // Only the current line counts.
        assert_eq!(fit("A line\n", "", "New start."), "New start. ");
        // "I", names written in the field and dictionary words keep their capital.
        assert_eq!(fit("and", "", "I agree."), " I agree. ");
        assert_eq!(fit("talked to Chris and", "", "Chris agreed."), " Chris agreed. ");
        assert_eq!(fit_to_field("Bars works.", &field("so", ""), true, &["Bars".into()]), " Bars works. ");
        // Opening brackets take no space.
        assert_eq!(fit("call (", ")", "Maybe"), "maybe");
        // End of line without the trailing-space setting.
        assert_eq!(fit_to_field("Hi.", &field("", ""), false, &[]), "Hi.");
    }

    #[test]
    fn fillers_only_in_english_and_not_inside_words() {
        let p = proc(TextConfig::default());
        // "um" is a word in Portuguese and German.
        assert_eq!(p.process("Eu tenho um carro novo.", false), "Eu tenho um carro novo.");
        assert_eq!(p.process("Ich komme um drei Uhr.", false), "Ich komme um drei Uhr.");
        // Units and hyphenated words.
        assert_eq!(p.process("The gap is 5 mm wide.", true), "The gap is 5 mm wide.");
        assert_eq!(p.process("Uh-huh, that works.", true), "Uh-huh, that works.");
        assert_eq!(p.process("Mm-hmm, I agree.", true), "Mm-hmm, I agree.");
        // A filler that started a sentence hands its capital on.
        assert_eq!(p.process("That's done. Um, the next thing is tests.", true), "That's done. The next thing is tests.");
    }

    #[test]
    fn trailing_line_commands() {
        let p = proc(TextConfig::default());
        assert_eq!(p.process("That's the first item. New line.", true), "That's the first item.\n");
        assert_eq!(p.process("End of section. New paragraph.", true), "End of section.\n\n");
    }

    #[test]
    fn words_written_their_own_way() {
        let mut cfg = TextConfig::default();
        for (from, to) in [("dot env", ".env"), ("dot net", ".NET")] {
            cfg.dictionary.push(Replacement { from: from.into(), to: to.into(), learned: false });
        }
        cfg.snippets.push(Snippet { trigger: "my email".into(), expansion: "me@example.xyz".into() });
        cfg.snippets.push(Snippet { trigger: "my address".into(), expansion: "1 high street\n  springfield".into() });
        let p = proc(cfg);
        // A word that starts with a full stop keeps the space before it.
        assert_eq!(p.process("Open the dot env file and the dot net project.", true), "Open the .env file and the .NET project.");
        assert_eq!(p.process("Wait , what ?", true), "Wait, what?");
        // The first word isn't capitalized when it has its own spelling.
        assert_eq!(p.process("iPhone sales are up.", true), "iPhone sales are up.");
        assert_eq!(p.process("x86 only.", true), "x86 only.");
        assert_eq!(p.process("example.com is down.", true), "example.com is down.");
        assert_eq!(p.process("sales are up.", true), "Sales are up.");
        // Snippets go in as written, wherever they are.
        assert_eq!(p.process("my email please", true), "me@example.xyz please");
        assert_eq!(p.process("Send it to my address please.", true), "Send it to 1 high street\n  springfield please.");
    }

    #[test]
    fn fits_a_replaced_selection() {
        let f = Surroundings { before: "The ".into(), selected: "quick brown".into(), after: " fox".into() };
        assert_eq!(fit_to_field("Slow red.", &f, true, &[]), "slow red");
        let f = Surroundings { before: String::new(), selected: " Old text. ".into(), after: String::new() };
        assert_eq!(fit_to_field("New text.", &f, true, &[]), " New text. ");
    }

    #[test]
    fn splits_long_pastes_for_claude_code() {
        let short = "one\ntwo\nthree";
        assert_eq!(split_for_agent(short, Agent::ClaudeCode), vec![short]);
        let text = "a\nb\nc\nd\ne";
        let pieces = split_for_agent(text, Agent::ClaudeCode);
        assert_eq!(pieces, vec!["a\nb\nc", "\nd\ne"]);
        let long = format!("{}\n{}\n{}", "x".repeat(700), "y".repeat(10), "z".repeat(1300));
        let pieces = split_for_agent(&long, Agent::ClaudeCode);
        assert_eq!(pieces.concat(), long);
        for p in &pieces {
            assert!(p.chars().count() <= 600 && p.matches('\n').count() <= 2, "{p:?}");
        }
        assert_eq!(split_for_agent(&"é".repeat(2500), Agent::Codex).len(), 3);
        assert_eq!(split_for_agent("short\n\n\n\nlines", Agent::Codex).len(), 1);
    }

    #[test]
    fn splits_long_pastes_for_other_agents() {
        let text = (0..40).map(|i| format!("line {i} {}", "w".repeat(i * 7))).collect::<Vec<_>>().join("\n");
        for (agent, max_chars, max_breaks) in [(Agent::Droid, 350, 7), (Agent::Pi, 900, 9), (Agent::OpenCode, 140, 1)] {
            let pieces = split_for_agent(&text, agent);
            assert_eq!(pieces.concat(), text, "{agent:?}");
            assert!(pieces.len() > 1, "{agent:?}");
            for p in &pieces {
                assert!(p.chars().count() <= max_chars && p.matches('\n').count() <= max_breaks, "{agent:?} {p:?}");
            }
        }
        let short = "one\ntwo\nthree four five";
        assert_eq!(split_for_agent(short, Agent::Droid), vec![short]);
        assert_eq!(split_for_agent(short, Agent::Pi), vec![short]);
        assert_eq!(split_for_agent(short, Agent::OpenCode), vec!["one\ntwo", "\nthree four five"]);
        assert_eq!(split_for_agent("a\nb", Agent::OpenCode), vec!["a\nb"]);
    }

    #[test]
    fn finalize_spacing() {
        assert_eq!(finalize("Hello.", true), "Hello. ");
        assert_eq!(finalize("Hello.\n", true), "Hello.\n");
        assert_eq!(finalize("Hello.", false), "Hello.");
    }
}
