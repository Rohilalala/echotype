//! Dictionary import and export: a .txt of words (one a line) or a .csv of words and "from,to"
//! snippets into the Dictionary and Snippets, and both back out as one CSV that imports again.

use crate::settings::{Replacement, Settings};
use serde::Serialize;
use std::collections::HashSet;

/// At most this many new words and snippets from one file (Wispr Flow imports up to 1,000).
pub const MAX_IMPORT: usize = 1000;

/// What an import adds. The counts are its preview: "Adds 132 words, 4 snippets, skips 9 duplicates".
#[derive(Serialize, Debug, Default, PartialEq)]
pub struct Import {
    pub words: Vec<String>,
    pub snippets: Vec<Replacement>,
    /// Already in the Dictionary or Snippets, or earlier in the file, in any case.
    pub duplicates: usize,
    /// New, but past MAX_IMPORT.
    pub over: usize,
}

/// The rows of a CSV file. A quoted field may hold commas, line breaks and "" for a quote; rows
/// end with LF, CRLF or a lone CR.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let (mut rows, mut row, mut field, mut quoted) = (vec![], vec![], String::new(), false);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            '"' if quoted => quoted = false,
            '"' if field.trim().is_empty() => {
                field.clear(); // spaces before an opening quote are not part of the field
                quoted = true;
            }
            ',' if !quoted => row.push(std::mem::take(&mut field)),
            '\r' | '\n' if !quoted => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A first row naming the columns (this module's own export writes one).
fn is_header(first: &str, second: &str) -> bool {
    matches!(first.to_lowercase().as_str(), "word" | "words" | "term" | "from" | "phrase" | "when you say") && matches!(second.to_lowercase().as_str(), "" | "to" | "replacement" | "type")
}

/// The words and snippets in a file, in order, duplicates included. In a .csv a row with one field
/// is a word and "from,to" a snippet; columns after the second are ignored. In a .txt every line
/// is a word, commas and all. Whitespace runs (a line break inside quotes too) become one space.
pub fn parse(text: &str, csv: bool) -> (Vec<String>, Vec<Replacement>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text); // Excel's UTF-8 mark
    let rows = if csv { csv_rows(text) } else { text.lines().map(|l| vec![l.to_string()]).collect() };
    let (mut words, mut snippets) = (vec![], vec![]);
    for (i, row) in rows.iter().enumerate() {
        let cell = |n: usize| row.get(n).map_or(String::new(), |f| f.split_whitespace().collect::<Vec<_>>().join(" "));
        let (from, to) = (cell(0), cell(1));
        if csv && i == 0 && is_header(&from, &to) {
            continue;
        }
        match (from.is_empty(), to.is_empty()) {
            (false, true) => words.push(from),
            (false, false) => snippets.push(Replacement { from, to }),
            _ => {} // a blank line, or text to type with no phrase to say
        }
    }
    (words, snippets)
}

/// What importing `text` into `s` would add: new words and snippets (by phrase), in any case,
/// up to MAX_IMPORT.
pub fn plan(s: &Settings, text: &str, csv: bool) -> Import {
    let (words, snippets) = parse(text, csv);
    let mut have: HashSet<String> = s.dictionary.iter().map(|w| w.to_lowercase()).collect();
    let mut phrases: HashSet<String> = s.replacements.iter().map(|r| r.from.trim().to_lowercase()).collect();
    let mut out = Import::default();
    for w in words {
        if !have.insert(w.to_lowercase()) {
            out.duplicates += 1;
        } else if out.words.len() >= MAX_IMPORT {
            out.over += 1;
        } else {
            out.words.push(w);
        }
    }
    for r in snippets {
        if !phrases.insert(r.from.to_lowercase()) {
            out.duplicates += 1;
        } else if out.words.len() + out.snippets.len() >= MAX_IMPORT {
            out.over += 1;
        } else {
            out.snippets.push(r);
        }
    }
    out
}

/// Adds the import. A word the user once removed was ignored for suggestions; importing it undoes that.
pub fn apply(s: &mut Settings, i: &Import) {
    let added: HashSet<String> = i.words.iter().map(|w| w.to_lowercase()).collect();
    s.ignored_words.retain(|w| !added.contains(&w.to_lowercase()));
    s.dictionary.extend(i.words.iter().cloned());
    s.replacements.extend(i.snippets.iter().cloned());
}

fn field(f: &str) -> String {
    if f.contains([',', '"', '\n', '\r']) || f.trim() != f { format!("\"{}\"", f.replace('"', "\"\"")) } else { f.to_string() }
}

/// The Dictionary and Snippets as one CSV: a header, the words one a row, then "from,to" rows.
/// With a UTF-8 mark so Excel shows names with accents as written.
pub fn to_csv(s: &Settings) -> String {
    let mut out = String::from("\u{feff}word,replacement\r\n");
    for w in &s.dictionary {
        out += &format!("{}\r\n", field(w));
    }
    for r in s.replacements.iter().filter(|r| !r.from.trim().is_empty()) {
        out += &format!("{},{}\r\n", field(&r.from), field(&r.to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(from: &str, to: &str) -> Replacement {
        Replacement { from: from.into(), to: to.into() }
    }

    #[test]
    fn a_txt_file_is_one_word_a_line() {
        let (w, r) = parse("\u{feff}Arnav\r\n\r\n  Xoxoday  \nKubernetes, the tool\n\n", false);
        assert_eq!(w, ["Arnav", "Xoxoday", "Kubernetes, the tool"], "BOM, CRLF, blank lines and spaces go; commas stay");
        assert!(r.is_empty());
    }

    #[test]
    fn csv_words_snippets_and_quotes() {
        let text = "\u{feff}Arnav\r\nmy email,me@example.com\r\n\"Smith, John\"\r\n\"say \"\"hi\"\"\",\"Hello, \"\"friend\"\"\"\r\n  \"spaced\" ,x\r\n\r\n,orphan\r\nMidhun,\r\na,b,c";
        let (w, r) = parse(text, true);
        assert_eq!(w, ["Arnav", "Smith, John", "Midhun"]);
        assert_eq!(r, [rep("my email", "me@example.com"), rep("say \"hi\"", "Hello, \"friend\""), rep("spaced", "x"), rep("a", "b")]);
    }

    #[test]
    fn csv_line_breaks_inside_quotes_and_lone_cr() {
        let (w, r) = parse("\"sign\r\noff\",\"Thanks,\nAditya\"\rLast", true);
        assert_eq!(r, [rep("sign off", "Thanks, Aditya")], "a line break inside quotes is a space");
        assert_eq!(w, ["Last"], "an old Mac CR ends a row");
        assert_eq!(parse("\"unclosed, quote", true).0, ["unclosed, quote"], "a missing closing quote keeps the rest");
        assert_eq!(parse("", true), (vec![], vec![]));
    }

    #[test]
    fn a_header_row_is_skipped_only_first() {
        assert_eq!(parse("Word\nArnav", true).0, ["Arnav"]);
        assert_eq!(parse("From,To\nbrb,be right back", true).1, [rep("brb", "be right back")]);
        assert_eq!(parse("Arnav\nword", true).0, ["Arnav", "word"], "later on it is a word");
        assert_eq!(parse("word\nArnav", false).0, ["word", "Arnav"], "a .txt has no header");
    }

    #[test]
    fn plan_skips_duplicates_in_any_case_and_counts_them() {
        let s = Settings { dictionary: vec!["Arnav".into()], replacements: vec![rep("My Email", "a@b.c")], ..Default::default() };
        let p = plan(&s, "arnav\nXoxoday\nXOXODAY\nmy email,x@y.z\nbrb,be right back\nBRB,other\nÉmile\némile", true);
        assert_eq!(p.words, ["Xoxoday", "Émile"]);
        assert_eq!(p.snippets, [rep("brb", "be right back")]);
        assert_eq!((p.duplicates, p.over), (5, 0), "arnav, XOXODAY, my email, BRB, émile");
    }

    #[test]
    fn plan_caps_an_import_at_1000() {
        let text: String = (0..1005).map(|i| format!("word{i}\n")).chain(["a,b\n".into()]).collect();
        let p = plan(&Settings::default(), &text, true);
        assert_eq!((p.words.len(), p.snippets.len(), p.over), (MAX_IMPORT, 0, 6), "the snippet is past the cap too");
        assert_eq!(p.words.last().unwrap(), "word999");
    }

    #[test]
    fn apply_adds_and_unignores() {
        let mut s = Settings { dictionary: vec!["Arnav".into()], ignored_words: vec!["xoxoday".into(), "Other".into()], ..Default::default() };
        let p = plan(&s, "Xoxoday\nhi,hello", true);
        apply(&mut s, &p);
        assert_eq!(s.dictionary, ["Arnav", "Xoxoday"]);
        assert_eq!(s.replacements, [rep("hi", "hello")]);
        assert_eq!(s.ignored_words, ["Other"]);
        assert_eq!(plan(&s, "Xoxoday\nhi,hello", true), Import { duplicates: 2, ..Default::default() }, "a second import adds nothing");
    }

    #[test]
    fn export_imports_back_unchanged() {
        let s = Settings {
            dictionary: vec!["Arnav".into(), "Smith, John".into(), "word".into(), "say \"hi\"".into()],
            replacements: vec![rep("my email", "me@example.com"), rep("sign-off", "Thanks, \"A\""), rep("  ", "unfinished row")],
            ..Default::default()
        };
        let csv = to_csv(&s);
        assert!(csv.starts_with("\u{feff}word,replacement\r\nArnav\r\n\"Smith, John\"\r\n"));
        let (w, r) = parse(&csv, true);
        assert_eq!(w, s.dictionary, "a word named \"word\" survives: the header is written");
        assert_eq!(r, s.replacements[..2]);
    }
}
