//! "Learn words from my corrections": for a minute after a paste, the field it went into is read
//! back (Accessibility) and compared with what was pasted. Names and terms the user re-spelled
//! there ("Midan" → "Midhun") are offered for the Dictionary; nothing is added until Keep.

use crate::settings::Settings;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// Fields longer than this (a document, a long thread) are never read.
pub const MAX_FIELD: usize = 20_000;
const TICK: Duration = Duration::from_secs(1);
const WATCH_FOR: Duration = Duration::from_secs(60);
/// A correction spans at most this many words on either side ("our nav" → "Arnav").
const MAX_RUN: usize = 3;
/// macOS's word list: lower-case entries are ordinary words, so "Meeting" is never learned.
const WORDS: &str = "/usr/share/dict/words";

/// Bumped by every paste and every new dictation: a watch whose number is gone stops.
static GEN: AtomicU64 = AtomicU64::new(0);

/// A dictation starts: the watch on the last paste reads the field once more and settles.
pub fn interrupt() {
    GEN.fetch_add(1, Ordering::SeqCst);
}

/// Lower-case letters and digits only: "Midhun," and "midhun" are the same word.
fn key(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// "(Xoxoday)." → "Xoxoday".
fn bare(w: &str) -> &str {
    w.trim_matches(|c: char| !c.is_alphanumeric())
}

/// Shaped like a name or a term: 3+ characters with a capital or a digit, only letters, digits,
/// apostrophes and hyphens (no URL, email, path or snake_case).
// ponytail: camelCase passes ("iPhone", but also "getUser"); filter it if a code editor shows up.
fn plausible(w: &str) -> bool {
    w.chars().count() >= 3
        && w.chars().any(char::is_alphabetic)
        && w.chars().any(|c| c.is_uppercase() || c.is_ascii_digit())
        && w.chars().all(|c| c.is_alphanumeric() || matches!(c, '\'' | '’' | '-'))
}

/// A re-spelling, not a different word: close in letters ("ournav"/"arnav", "gptfor"/"gpt4").
fn similar(from: &str, to: &str) -> bool {
    from != to && crate::cleanup::levenshtein(from, to) <= (from.chars().count().max(to.chars().count()) / 2).max(2)
}

/// What the user re-spelled in `field` since `pasted` went in: the new forms, exact case. None
/// when the pasted text can't be found there any more (sent, deleted, rewritten, another field).
pub fn corrections(pasted: &str, field: &str) -> Option<Vec<String>> {
    if pasted.trim().is_empty() {
        return None;
    }
    if field.contains(pasted.trim()) {
        return Some(vec![]);
    }
    let words = |t: &str| -> Vec<(String, String)> { t.split_whitespace().map(|w| (bare(w).to_string(), key(w))).filter(|(_, k)| !k.is_empty()).collect() };
    let (a, b) = (words(pasted), words(field));
    let (n, m) = (a.len(), b.len());
    // ponytail: O(n·m) LCS table; past 4M cells (a long dictation in a long field) it is skipped.
    if n == 0 || n * m > 4_000_000 {
        return None;
    }
    let at = |i: usize, j: usize| i * (m + 1) + j;
    let mut l = vec![0u16; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[at(i, j)] = if a[i].1 == b[j].1 { l[at(i + 1, j + 1)] + 1 } else { l[at(i + 1, j)].max(l[at(i, j + 1)]) };
        }
    }
    // Most of what was pasted must still be there, or this is not a correction of it.
    if (l[0] as usize) * 2 < n {
        return None;
    }
    let mut pairs = vec![];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i].1 == b[j].1 {
            pairs.push((i, j));
            (i, j) = (i + 1, j + 1);
        } else if l[at(i + 1, j)] >= l[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let mut out: Vec<String> = vec![];
    let mut add = |w: &str| {
        if plausible(w) && !out.iter().any(|o| o == w) {
            out.push(w.to_string());
        }
    };
    // Same word, new capitals ("xoxoday" → "Xoxoday").
    for &(i, j) in &pairs {
        if a[i].0 != b[j].0 {
            add(&b[j].0);
        }
    }
    // The runs between matched words: a pasted run replaced by a field run is a correction.
    // At either end the field run also holds the text around the paste (or what was typed
    // after it), so only the 1..3 words next to the paste are tried there.
    let mut bounds = vec![(0, 0)];
    bounds.extend(pairs.iter().map(|&(i, j)| (i + 1, j + 1)));
    let ends: Vec<(usize, usize)> = pairs.iter().copied().chain([(n, m)]).collect();
    for (k, (&(i0, j0), &(i1, j1))) in bounds.iter().zip(&ends).enumerate() {
        let (from, to) = (&a[i0..i1], &b[j0..j1]);
        if from.is_empty() || to.is_empty() || from.len() > MAX_RUN {
            continue;
        }
        let fk: String = from.iter().map(|w| w.1.as_str()).collect();
        let (lead, trail) = (k == 0, k == pairs.len());
        let sizes = 1..=to.len().min(MAX_RUN);
        let spans: Vec<&[(String, String)]> = if lead {
            sizes.map(|c| &to[to.len() - c..]).collect()
        } else if trail {
            sizes.map(|c| &to[..c]).collect()
        } else if to.len() <= MAX_RUN {
            vec![to]
        } else {
            vec![]
        };
        let tk = |t: &[(String, String)]| t.iter().map(|w| w.1.as_str()).collect::<String>();
        let best = spans.into_iter().filter(|t| similar(&fk, &tk(t))).min_by_key(|t| crate::cleanup::levenshtein(&fk, &tk(t)));
        for w in best.into_iter().flatten() {
            if !from.iter().any(|f| f.1 == w.1) {
                add(&w.0);
            }
        }
    }
    Some(out)
}

/// An ordinary word with a capital ("Meeting", "THE"), by `list` (one word per line). Mixed case
/// and digits ("iPhone", "GPT4") never are.
fn everyday(w: &str, list: &str) -> bool {
    let rest: String = w.chars().skip(1).collect();
    let shaped = !w.chars().any(|c| c.is_ascii_digit()) && (rest == rest.to_lowercase() || w == w.to_uppercase());
    let low = w.to_lowercase();
    shaped && list.lines().any(|l| l == low)
}

/// The words not yet in the Dictionary, waiting, or ignored (any case), each once.
fn fresh(words: &[String], s: &Settings) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for w in words {
        let k = w.to_lowercase();
        let known = |v: &[String]| v.iter().any(|x| x.trim().to_lowercase() == k);
        if !known(&s.dictionary) && !known(&s.learned) && !known(&s.ignored_words) && !known(&out) {
            out.push(w.clone());
        }
    }
    out
}

/// Called after `pasted` was pasted with ⌘V: watches the focused field of the app in front until
/// focus leaves it, the next dictation starts, or a minute passes, then offers what it learned.
pub fn watch(app: &AppHandle, pasted: String) {
    let Some(pid) = crate::platform::front_pid() else { return };
    let id = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    std::thread::spawn(move || {
        let started = Instant::now();
        let (mut last, mut found) = (None::<String>, None::<Vec<String>>);
        loop {
            std::thread::sleep(TICK);
            let ended = GEN.load(Ordering::SeqCst) != id || started.elapsed() > WATCH_FOR;
            // Another app, a password or non-text field, or a huge one: focus left the paste.
            let Some(v) = crate::platform::focused_text(pid, MAX_FIELD) else { break };
            if last.as_ref() != Some(&v) {
                // Keep the last reading that still holds the paste: a sent message empties the field.
                if let Some(c) = corrections(&pasted, &v) {
                    found = Some(c);
                }
                last = Some(v);
            }
            if ended {
                break;
            }
        }
        if let Some(words) = found.filter(|w| !w.is_empty()) {
            offer(&app, id, words);
        }
    });
}

/// Queues the new words for Keep or Undo (Settings.learned) and says so on the pill, unless a
/// dictation has started since.
fn offer(app: &AppHandle, id: u64, words: Vec<String>) {
    let list = std::fs::read_to_string(WORDS).unwrap_or_default();
    let words: Vec<String> = words.into_iter().filter(|w| !everyday(w, &list)).collect();
    let s = crate::current_settings(app);
    if !s.learn_words || fresh(&words, &s).is_empty() {
        return;
    }
    let mut added = vec![];
    let res = crate::commit_settings(app, |cur| {
        let mut new = cur.clone();
        added = fresh(&words, cur);
        new.learned.extend(added.iter().cloned());
        new
    });
    if res.is_err() || added.is_empty() || GEN.load(Ordering::SeqCst) != id {
        return;
    }
    let names = match added.as_slice() {
        [one] => format!("{one}. Keep it?"),
        [a, b] => format!("{a} and {b}. Keep them?"),
        more => format!("{} words. Keep them?", more.len()),
    };
    crate::emit_state(app, crate::hint(format!("Learned {names}"), Some("dictionary")));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned(pasted: &str, field: &str) -> Vec<String> {
        corrections(pasted, field).expect("the paste is still there")
    }

    #[test]
    fn one_misheard_name_fixed() {
        assert_eq!(learned("Ask Midan about the invoice.", "Ask Midhun about the invoice."), ["Midhun"]);
        // Several words heard for one name, and one word for several.
        assert_eq!(learned("Send it to our nav today.", "Send it to Arnav today."), ["Arnav"]);
        assert_eq!(learned("The store is on zoxo day now.", "The store is on Xoxoday now."), ["Xoxoday"]);
        assert_eq!(learned("We use GPT for for this.", "We use GPT4 for this."), ["GPT4"]);
    }

    #[test]
    fn several_words_fixed_in_one_paste() {
        assert_eq!(learned("Midan and our nav set up zoxo day for the team.", "Midhun and Arnav set up Xoxoday for the team."), ["Midhun", "Arnav", "Xoxoday"]);
    }

    #[test]
    fn untouched_or_punctuation_and_spacing_only_teach_nothing() {
        assert_eq!(learned("see you at 5", "Hi! see you at 5"), Vec::<String>::new());
        assert_eq!(learned("ok so the plan is set", "Ok, so the plan is set."), Vec::<String>::new());
        assert_eq!(learned("the plan is set", "the   plan\nis set!"), Vec::<String>::new());
    }

    #[test]
    fn a_new_capital_on_the_same_word_is_learned_but_lower_case_never() {
        assert_eq!(learned("we moved to xoxoday", "we moved to Xoxoday"), ["Xoxoday"]);
        assert_eq!(learned("buy an iphone", "buy an iPhone"), ["iPhone"]);
        assert_eq!(learned("Ask Midan now", "Ask midhun now"), Vec::<String>::new(), "lower case is not a name");
    }

    #[test]
    fn a_reworded_phrase_is_not_a_spelling() {
        assert_eq!(learned("Let's meet on Monday at noon.", "Let's meet on Thursday at noon."), Vec::<String>::new());
        assert_eq!(learned("Tell the team it shipped.", "Tell Priyanshu it shipped."), Vec::<String>::new());
    }

    #[test]
    fn urls_emails_and_code_are_never_words() {
        // Each is close enough to count as a re-spelling; its shape rules it out.
        assert_eq!(learned("write to aditya at gmail now please", "write to aditya@Gmail now please"), Vec::<String>::new());
        assert_eq!(learned("the file is at docs slash Setup today", "the file is at docs/Setup today"), Vec::<String>::new());
        assert_eq!(learned("the site is example dot Com now", "the site is example.Com now"), Vec::<String>::new());
        assert_eq!(learned("set my var to one", "set MY_VAR2 to one"), Vec::<String>::new());
        assert_eq!(learned("ping Al now", "ping AL now"), Vec::<String>::new(), "too short");
    }

    #[test]
    fn text_moved_around_teaches_nothing() {
        let pasted = "Call Arnav tomorrow. Then email Midhun about Xoxoday.";
        assert_eq!(learned(pasted, "Then email Midhun about Xoxoday. Call Arnav tomorrow."), Vec::<String>::new());
    }

    #[test]
    fn typing_on_after_the_paste_is_not_a_correction() {
        assert_eq!(learned("Ask Midhun about it", "Ask Midhun about it and also Kubernetes Rollout plans"), Vec::<String>::new());
        // A fix at the end followed by more typing still counts.
        assert_eq!(learned("Ask about it, Midan", "Ask about it, Midhun and then Priyanshu"), ["Midhun"]);
        // And a fix at the start after text that was already there.
        assert_eq!(learned("Midan will call you.", "Hello Priyanshu, Midhun will call you."), ["Midhun"]);
    }

    #[test]
    fn a_paste_deleted_sent_or_rewritten_is_gone() {
        assert_eq!(corrections("Ask Midan about the invoice.", ""), None);
        assert_eq!(corrections("Ask Midan about the invoice.", "Something else entirely, Midhun."), None);
        assert_eq!(corrections("our nav", "Arnav"), None, "nothing left to anchor a short paste");
        assert_eq!(corrections("", "Arnav"), None);
    }

    #[test]
    fn ordinary_capitalised_words_and_known_ones_are_dropped() {
        let list = "meeting\nthe\nhello\nParis\n";
        assert!(everyday("Meeting", list) && everyday("THE", list));
        assert!(!everyday("Midhun", list) && !everyday("iPhone", list) && !everyday("Paris", list) && !everyday("GPT4", list));
        let s = Settings { dictionary: vec!["Arnav".into()], learned: vec!["Xoxoday".into()], ignored_words: vec!["midhun".into()], ..Default::default() };
        let w = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(fresh(&w(&["arnav", "XOXODAY", "Midhun", "Priyanshu", "priyanshu"]), &s), ["Priyanshu"]);
    }

    #[test]
    fn a_huge_field_is_not_aligned() {
        let pasted = "word ".repeat(500);
        let field = format!("{} Midhun", "other ".repeat(9000));
        assert_eq!(corrections(&pasted, &field), None);
    }
}
