//! Learning from his fixes.
//!
//! The speech model cannot be retrained while it runs, but his edits say exactly what it gets
//! wrong. When he changes "Hux" to "Huck's" in the box before sending, the pair is remembered: the
//! right word joins the vocabulary Whisper is prompted with, and the wrong one is replaced in
//! everything recognised afterwards. Decided with him 2026-09-28: every fix is remembered without
//! asking, while Learning is on.
//!
//! What is *not* learned, because a rule made from it would do harm everywhere else: rewrites
//! longer than a few words, a change of a common word ("the" to "a" is a style choice, not a
//! mishearing), punctuation, and capitalising the start of a sentence.

use serde::{Deserialize, Serialize};

/// One remembered fix: whenever `from` is heard, write `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    /// The words as recognised, lower case, without punctuation: "hux".
    pub from: String,
    /// What he wrote instead: "Huck's".
    pub to: String,
}

/// The longest run of words, either side, that counts as one mishearing.
const MAX_WORDS: usize = 3;
/// Past this the comparison is skipped: a text this long was rewritten, not corrected.
const MAX_TOKENS: usize = 1500;

/// Words too common to ever become a rule on their own.
const COMMON: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "if", "of", "to", "in", "on", "at", "by", "for", "with",
    "from", "as", "is", "are", "was", "were", "be", "been", "am", "it", "its", "it's", "this",
    "that", "these", "those", "i", "you", "he", "she", "we", "they", "me", "him", "her", "us",
    "them", "my", "your", "his", "our", "their", "there", "here", "then", "than", "so", "not",
    "no", "yes", "do", "does", "did", "have", "has", "had", "will", "would", "can", "could",
    "should", "just", "like", "what", "when", "where", "who", "why", "how", "all", "some", "one",
    "two", "too", "also", "up", "down", "out", "about", "into", "over", "now", "very", "really",
    "okay", "ok", "oh", "um", "uh", "well", "get", "got", "go", "going", "know", "think", "want",
    "see", "say", "said", "make", "way", "thing", "things", "four", "for", "to", "two", "hear",
    "here", "right", "write", "which", "while", "with", "our", "are", "your", "you're", "they're",
];

/// A word as compared: its letters, digits and inner apostrophes, nothing around them.
fn core(token: &str) -> &str {
    token.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'').trim_matches('\'')
}

fn key(token: &str) -> String {
    core(token).to_lowercase()
}

/// The fixes his edit implies, comparing what was recognised with what he sent.
pub fn learn(recognised: &str, sent: &str) -> Vec<Fix> {
    let a: Vec<&str> = recognised.split_whitespace().collect();
    let b: Vec<&str> = sent.split_whitespace().collect();
    if a.is_empty() || b.is_empty() || a.len() > MAX_TOKENS || b.len() > MAX_TOKENS {
        return Vec::new();
    }
    // Compared by letters in their case, so "github" -> "GitHub" is seen; punctuation is not.
    let ca: Vec<&str> = a.iter().map(|t| core(t)).collect();
    let cb: Vec<&str> = b.iter().map(|t| core(t)).collect();

    // Longest common subsequence, then walk it to find the stretches that differ.
    let (n, m) = (ca.len(), cb.len());
    let mut lcs = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if ca[i] == cb[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut fixes = Vec::new();
    let (mut i, mut j) = (0, 0);
    let (mut gi, mut gj) = (0, 0);
    loop {
        let at_end = i == n && j == m;
        if at_end || (i < n && j < m && ca[i] == cb[j]) {
            if let Some(fix) = fix_from(&ca[gi..i], &cb[gj..j]) {
                if !fixes.contains(&fix) {
                    fixes.push(fix);
                }
            }
            if at_end {
                break;
            }
            i += 1;
            j += 1;
            gi = i;
            gj = j;
        } else if j == m || (i < n && lcs[i + 1][j] >= lcs[i][j + 1]) {
            i += 1;
        } else {
            j += 1;
        }
    }
    fixes
}

/// One differing stretch, if it is the kind of thing worth remembering.
fn fix_from(heard: &[&str], wrote: &[&str]) -> Option<Fix> {
    let heard: Vec<&str> = heard.iter().copied().filter(|w| !w.is_empty()).collect();
    let wrote: Vec<&str> = wrote.iter().copied().filter(|w| !w.is_empty()).collect();
    if heard.is_empty() || wrote.is_empty() || heard.len() > MAX_WORDS || wrote.len() > MAX_WORDS {
        return None;
    }
    let from = heard.join(" ").to_lowercase();
    let to = wrote.join(" ");
    if heard.len() == 1 && COMMON.contains(&from.as_str()) {
        return None;
    }
    // Only a capital letter added to the first word: the start of a sentence, not a name.
    let mut lowered_first = to.clone();
    if let Some(first) = lowered_first.get_mut(0..1) {
        first.make_ascii_lowercase();
    }
    if lowered_first == heard.join(" ") || to == heard.join(" ") {
        return None;
    }
    Some(Fix { from, to })
}

/// Apply every remembered fix to freshly recognised text, keeping the punctuation around it.
pub fn apply(text: &str, fixes: &[Fix]) -> String {
    if fixes.is_empty() {
        return text.to_string();
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let keys: Vec<String> = tokens.iter().map(|t| key(t)).collect();
    // Longest first, so "hux play" wins over "hux".
    let mut rules: Vec<(Vec<&str>, &str)> = fixes
        .iter()
        .map(|f| (f.from.split_whitespace().collect::<Vec<_>>(), f.to.as_str()))
        .filter(|(from, _)| !from.is_empty())
        .collect();
    rules.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    'words: while i < tokens.len() {
        for (from, to) in &rules {
            let end = i + from.len();
            if end <= tokens.len() && keys[i..end].iter().zip(from).all(|(k, f)| k == f) {
                let first = tokens[i];
                let last = tokens[end - 1];
                let lead = &first[..first.find(core(first)).unwrap_or(0)];
                let tail_core = core(last);
                let tail_at = last.rfind(tail_core).map(|p| p + tail_core.len()).unwrap_or(last.len());
                out.push(format!("{lead}{to}{}", &last[tail_at..]));
                i = end;
                continue 'words;
            }
        }
        out.push(tokens[i].to_string());
        i += 1;
    }
    out.join(" ")
}

/// Add a fix, replacing any older one for the same heard words. Returns whether anything changed.
pub fn remember(fixes: &mut Vec<Fix>, fix: Fix) -> bool {
    match fixes.iter_mut().find(|f| f.from == fix.from) {
        Some(existing) if existing.to == fix.to => false,
        Some(existing) => {
            existing.to = fix.to;
            true
        }
        None => {
            fixes.push(fix);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(from: &str, to: &str) -> Fix {
        Fix { from: from.into(), to: to.into() }
    }

    #[test]
    fn a_misheard_name_is_learned() {
        assert_eq!(
            learn("I am Hux voice to text.", "I am Huck's voice to text."),
            vec![fix("hux", "Huck's")]
        );
    }

    #[test]
    fn a_misheard_phrase_of_a_few_words_is_learned() {
        assert_eq!(
            learn("open hyper frames now", "open HyperFrames now"),
            vec![fix("hyper frames", "HyperFrames")]
        );
    }

    #[test]
    fn a_new_capital_in_a_name_is_learned() {
        assert_eq!(learn("push it to github", "push it to GitHub"), vec![fix("github", "GitHub")]);
    }

    #[test]
    fn style_edits_are_not_learned() {
        assert!(learn("the cat sat", "a cat sat").is_empty(), "common words are never rules");
        assert!(learn("so we went", "So we went").is_empty(), "sentence-start capital");
        assert!(learn("hello world", "hello, world!").is_empty(), "punctuation only");
        assert!(learn("same text", "same text").is_empty());
        assert!(
            learn("please send it today", "could you kindly forward the whole thing over by Friday")
                .is_empty(),
            "a rewrite is not a mishearing"
        );
    }

    #[test]
    fn words_he_only_added_or_removed_teach_nothing() {
        assert!(learn("send the file", "send the file now").is_empty());
        assert!(learn("send the file um now", "send the file now").is_empty());
    }

    #[test]
    fn fixes_are_applied_with_the_punctuation_kept() {
        let fixes = vec![fix("hux", "Huck's"), fix("hyper frames", "HyperFrames")];
        assert_eq!(apply("Hux, meet hyper frames.", &fixes), "Huck's, meet HyperFrames.");
        assert_eq!(apply("(hux)", &fixes), "(Huck's)");
        assert_eq!(apply("nothing here", &fixes), "nothing here");
        assert_eq!(apply("huxley", &fixes), "huxley", "whole words only");
    }

    #[test]
    fn remembering_again_replaces_the_old_answer() {
        let mut fixes = vec![fix("hux", "Hucks")];
        assert!(remember(&mut fixes, fix("hux", "Huck's")));
        assert!(!remember(&mut fixes, fix("hux", "Huck's")));
        assert_eq!(fixes, vec![fix("hux", "Huck's")]);
    }
}
