//! Screen context: names, identifiers and file names near the caret of the app in front, for the
//! speech model's prompt (stt::prompt), and spoken file tags in AI code editors. Read through
//! Accessibility as a dictation starts; kept for that dictation only, never saved or sent.

use std::time::Duration;

/// What the screen showed for one dictation (Settings::screen).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Screen {
    /// The bundle id of the app in front as recording started ("" when unknown).
    pub app: String,
    /// tokens() of the text around its caret.
    pub words: Vec<String>,
}

/// How much of the text either side of the caret is read (UTF-16 units, as Accessibility counts).
const BEFORE: usize = 600;
const AFTER: usize = 200;
/// The prompt's share for screen words: whisper's whole prompt is about 220 tokens, and the
/// dictionary comes first.
pub const MAX_TOKENS: usize = 40;
pub const MAX_CHARS: usize = 200;
/// The words just before the caret go in whatever they look like: the sentence being written.
const LAST_WORDS: usize = 6;
/// Longer is a hash, a key or minified code, not a word anyone says.
const MAX_LEN: usize = 32;

/// Extensions that make "name.ext" a file name, not a member access like "user.name".
// ponytail: a fixed list; add an extension when someone tags a file it misses.
const EXTENSIONS: [&str; 52] = [
    "c", "cc", "cfg", "cjs", "conf", "cpp", "cs", "css", "csv", "dart", "env", "ex", "exs", "go", "gradle", "h", "hpp", "html", "ini", "ipynb",
    "java", "js", "json", "jsx", "kt", "lock", "lua", "m", "md", "mdx", "mjs", "mm", "php", "plist", "proto", "py", "rb", "rs", "scss", "sh",
    "sql", "svelte", "swift", "toml", "ts", "tsx", "txt", "vue", "xml", "yaml", "yml", "zig",
];

/// tokens() of the text around the caret of the app in front, or none. An Electron app woken
/// as the recording started (platform::wake_accessibility) takes about a second to answer, so
/// a miss is asked again once.
pub fn read() -> Vec<String> {
    let ask = || crate::platform::caret_text(BEFORE, AFTER);
    ask().or_else(|| {
        std::thread::sleep(Duration::from_millis(800));
        ask()
    })
    .map_or_else(Vec::new, |(before, after)| tokens(&before, &after))
}

pub fn is_file(w: &str) -> bool {
    w.rsplit_once('.').is_some_and(|(stem, ext)| stem.chars().count() >= 2 && stem.chars().any(char::is_alphabetic) && EXTENSIONS.contains(&ext.to_lowercase().as_str()))
}

/// The words of `text` in order, each with whether it starts a sentence (the first of a line or
/// after . ! ?), so a capital there is no sign of a name. Emails and URLs are dropped; a path or a
/// call splits into its parts ("src/auth.ts:12" → src, auth.ts, 12); a dot joins only a file name.
fn words(text: &str, mut start: bool) -> Vec<(String, bool)> {
    let mut out = vec![];
    for (n, line) in text.split('\n').enumerate() {
        start |= n > 0;
        for chunk in line.split_whitespace() {
            let first = start;
            start = chunk.trim_end_matches(['"', '\'', ')', ']', '”', '’']).ends_with(['.', '!', '?']);
            let bare = chunk.trim_start_matches(|c: char| !c.is_alphanumeric());
            if chunk.contains("://") || bare.starts_with("www.") || bare.contains('@') {
                continue;
            }
            for piece in chunk.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))) {
                let piece = piece.trim_matches(['.', '-']);
                if is_file(piece) {
                    out.push((piece.to_string(), first));
                } else {
                    out.extend(piece.split('.').map(|p| p.trim_matches('-')).filter(|p| !p.is_empty()).map(|p| (p.to_string(), first)));
                }
            }
        }
    }
    out
}

/// A word worth spelling the way the screen does: a file name, an identifier (snake_case,
/// camelCase), letters with digits, an acronym, or a capitalised word inside a sentence.
fn salient(w: &str, starts_sentence: bool) -> bool {
    let upper = |c: &char| c.is_uppercase();
    let first_upper = w.chars().next().is_some_and(|c| c.is_uppercase());
    let lower = w.chars().any(char::is_lowercase);
    is_file(w)
        || w.contains('_')
        || w.chars().any(|c| c.is_ascii_digit())
        || (w.chars().skip(1).any(|c| upper(&c)) && lower)
        || (!lower && w.chars().filter(upper).count() >= 2)
        || (first_upper && !starts_sentence)
}

/// The names and identifiers in the text around the caret (`before` and `after` it) for the
/// speech model's prompt: salient() words plus the last few before the caret, each once, at most
/// MAX_TOKENS of them in MAX_CHARS, the nearest kept. They come back in reading order: whisper
/// takes its prompt as the text before the clip, and the clip "ask Xoxoday about the Kumustahan
/// rollout" came back right with the screen's words in their order, and as "Xoxoda" or with a
/// lower-case "asks" with them nearest first.
pub fn tokens(before: &str, after: &str) -> Vec<String> {
    let ends_sentence = before.trim_end().is_empty() || before.trim_end().ends_with(['.', '!', '?', '\n']);
    let b = words(before, true);
    let a = words(after, ends_sentence && !before.ends_with(char::is_alphanumeric));
    let usable = |w: &str| (2..=MAX_LEN).contains(&w.chars().count()) && w.chars().any(char::is_alphabetic);
    // (distance from the caret in words, after it or not, place in the text, word)
    let mut found: Vec<(usize, bool, usize, &str)> = vec![];
    for (d, (w, s)) in b.iter().rev().enumerate() {
        let recent = d < LAST_WORDS && w.chars().filter(|c| c.is_alphabetic()).count() >= 4; // not "the", "and"...
        if usable(w) && (recent || salient(w, *s)) {
            found.push((d, false, b.len() - d, w));
        }
    }
    for (d, (w, s)) in a.iter().enumerate() {
        if usable(w) && salient(w, *s) {
            found.push((d, true, b.len() + 1 + d, w));
        }
    }
    found.sort_by_key(|&(d, after, _, _)| (d, after));
    let (mut kept, mut seen, mut chars) = (vec![], vec![], 0);
    for (_, _, at, w) in found {
        let key = w.to_lowercase();
        let len = w.chars().count() + 2; // and its ", "
        if seen.contains(&key) || chars + len > MAX_CHARS {
            continue;
        }
        seen.push(key);
        chars += len;
        kept.push((at, w.to_string()));
        if kept.len() == MAX_TOKENS {
            break;
        }
    }
    kept.sort();
    kept.into_iter().map(|(_, w)| w).collect()
}

/// AI code editors whose chat takes an "@file" tag: Cursor, Windsurf, VS Code.
const TAGGING_APPS: [&str; 4] = ["com.todesktop.230313mzl4w4u92", "com.exafunction.windsurf", "com.microsoft.VSCode", "com.microsoft.VSCodeInsiders"];

pub fn tags_files(app: &str) -> bool {
    TAGGING_APPS.contains(&app)
}

fn norm(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Spoken file tags for an AI code editor's chat: "tag <file>" or "at <file>" becomes "@<file>"
/// when the words name a file seen on screen (`seen`, as spelled there: "user service dot ts" or
/// "user service" for user_service.ts) or are a file name themselves ("auth dot ts", "auth.ts").
/// Anything else, "meet at noon", stays words.
pub fn tag_files(text: &str, seen: &[String]) -> String {
    let files: Vec<&String> = seen.iter().filter(|w| is_file(w)).collect();
    text.split('\n').map(|line| tag_line(line, &files)).collect::<Vec<_>>().join("\n")
}

fn tag_line(line: &str, files: &[&String]) -> String {
    let ws: Vec<&str> = line.split(' ').collect();
    let (mut out, mut i) = (vec![], 0);
    while i < ws.len() {
        let trigger = matches!(ws[i].to_lowercase().as_str(), "tag" | "at");
        match trigger.then(|| (1..=4).rev().find_map(|k| tag(ws.get(i + 1..=i + k)?, files))).flatten() {
            Some((file, k)) => {
                out.push(file);
                i += k + 1;
            }
            _ => {
                out.push(ws[i].to_string());
                i += 1;
            }
        }
    }
    out.join(" ")
}

/// The "@file" the words after a trigger name, with the punctuation after them, and how many words it took.
fn tag(words: &[&str], files: &[&String]) -> Option<(String, usize)> {
    let (last, inner) = words.split_last()?;
    let name = last.trim_end_matches([',', '.', ';', ':', '!', '?']);
    let tail = &last[name.len()..];
    let mut said: Vec<&str> = inner.to_vec();
    said.push(name);
    if said.iter().any(|w| w.is_empty() || !w.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))) {
        return None;
    }
    let spoken = norm(&said.iter().filter(|w| !w.eq_ignore_ascii_case("dot")).copied().collect::<String>());
    let stem = |f: &str| f.rsplit_once('.').map_or(f.to_string(), |(s, _)| s.to_string());
    let by_name = files.iter().find(|f| norm(f) == spoken);
    let by_stem = || {
        let hits: Vec<_> = files.iter().filter(|f| norm(&stem(f)) == spoken).collect();
        (hits.len() == 1 && spoken.chars().count() >= 3).then(|| hits[0])
    };
    if let Some(f) = by_name.or_else(by_stem) {
        return Some((format!("@{f}{tail}"), words.len()));
    }
    let file = match said[..] {
        [one] if is_file(one) => one.to_string(),
        [stem, dot, ext] if dot.eq_ignore_ascii_case("dot") && is_file(&format!("{stem}.{ext}")) => format!("{stem}.{}", ext.to_lowercase()),
        _ => return None,
    };
    Some((format!("@{file}{tail}"), words.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_gives_its_identifiers_and_file_names() {
        let before = "import { useState } from 'react';\nimport { fetchUser } from './api/user_service.ts';\n\nconst MAX_RETRIES = 3;\nfunction loadProfile(userId) {\n  const res = await fetchUser(userId, ";
        let t = tokens(before, ");\n  return res.data;\n}");
        for w in ["useState", "fetchUser", "user_service.ts", "MAX_RETRIES", "loadProfile", "userId"] {
            assert!(t.iter().any(|x| x == w), "{w} in {t:?}");
        }
        assert_eq!(t.iter().filter(|x| *x == "fetchUser").count(), 1, "each once");
        assert!(!t.iter().any(|x| x == "api" || x == "res.data"), "a path's folder and a member access are not names: {t:?}");
        assert!(t.iter().any(|x| x == "await"), "but the words just before the caret go in: {t:?}");
    }

    #[test]
    fn prose_gives_its_names_not_its_sentence_starts() {
        let t = tokens("Hi Arnav,\nThanks for the update. We should ask Xoxoday about the Kumustahan rollout before Friday. The plan looks", " good to me.");
        for w in ["Arnav", "Xoxoday", "Kumustahan", "Friday"] {
            assert!(t.iter().any(|x| x == w), "{w} in {t:?}");
        }
        for w in ["Hi", "Thanks", "We", "The", "good"] {
            assert!(!t.iter().any(|x| x == w), "{w} is not a name: {t:?}");
        }
        assert_eq!(t[t.len() - 2..], ["plan", "looks"], "the words before the caret, in reading order");
        assert_eq!(t[..3], ["Arnav", "Xoxoday", "Kumustahan"]);
        assert!(!t.iter().any(|x| x == "the" || x == "to"), "short words are not worth a place: {t:?}");
    }

    #[test]
    fn mixed_text_keeps_acronyms_and_words_with_digits() {
        let t = tokens("Deploy the HMO fix to uat. GPT-4 and M3 builds, see PR #354 and main.rs", "");
        for w in ["HMO", "GPT-4", "M3", "PR", "main.rs"] {
            assert!(t.iter().any(|x| x == w), "{w} in {t:?}");
        }
        assert!(!t.iter().any(|x| x == "354"), "a bare number is no word: {t:?}");
    }

    #[test]
    fn emails_and_urls_are_dropped() {
        let t = tokens("Mail aditya@example.com or see https://github.com/Rohilalala/Echotype and www.Remotify.ph today, Midhun", "");
        assert!(t.iter().all(|w| !w.contains('@') && !w.contains("github") && !w.contains("Rohilalala") && !w.contains("Remotify")), "{t:?}");
        assert!(t.iter().any(|w| w == "Midhun"), "{t:?}");
        assert!(tokens("Tag @auth.ts please", "").contains(&"auth.ts".to_string()), "a tag is a file, not an email");
    }

    #[test]
    fn dedupes_and_caps() {
        assert_eq!(tokens("Arnav said arnav and ARNAV", ""), ["said", "ARNAV"], "case-insensitive, nearest spelling kept");
        let many: String = (0..100).map(|i| format!("Name{i}x ")).collect();
        let t = tokens(&many, "");
        assert!(t.len() <= MAX_TOKENS && t.iter().map(|w| w.len() + 2).sum::<usize>() <= MAX_CHARS, "{} tokens", t.len());
        assert_eq!(t.last().unwrap(), "Name99x", "the nearest survive the cap");
        assert!(!t.contains(&"Name0x".to_string()));
        let long = "a".repeat(40) + "B9";
        assert!(tokens(&format!("see {long} here"), "").iter().all(|w| w != &long), "a hash is not a word");
        assert!(tokens("", "").is_empty() && tokens("   \n ", "").is_empty());
    }

    #[test]
    fn after_the_caret_a_capital_mid_sentence_is_a_name() {
        assert_eq!(tokens("", "Meeting with Priyanshu"), ["Priyanshu"], "the first word after an empty field starts a sentence");
        assert_eq!(tokens("Talk to", " Anshika soon"), ["Talk", "Anshika"]);
        assert_eq!(tokens("Talk to", "\nHello there"), ["Talk"], "a new line starts a sentence");
    }

    #[test]
    fn file_names_need_a_known_extension() {
        assert!(is_file("auth.ts") && is_file("README.md") && is_file("user_service.TS") && is_file("Cargo.lock"));
        assert!(!is_file("user.name") && !is_file("e.g") && !is_file("1.2.3") && !is_file(".env") && !is_file("a.rs"));
    }

    #[test]
    fn tag_or_at_a_file_becomes_an_at_tag() {
        let seen: Vec<String> = ["user_service.ts", "README.md", "Kumustahan", "index.ts", "index.tsx"].map(String::from).to_vec();
        let cases = [
            ("Tag auth dot ts and fix it", "@auth.ts and fix it"),
            ("look at auth.ts.", "look @auth.ts."),
            ("Look at user service dot ts, then tag readme.", "Look @user_service.ts, then @README.md."),
            ("tag user service please", "@user_service.ts please"),
            ("Tag config dot TS", "@config.ts"),
            ("meet at noon", "meet at noon"),
            ("look at index", "look at index"),
            ("tag Kumustahan", "tag Kumustahan"),
            ("at five dot thirty", "at five dot thirty"),
            ("attack auth dot ts", "attack auth dot ts"),
            ("tag", "tag"),
            ("first line\ntag main dot rs", "first line\n@main.rs"),
        ];
        for (said, typed) in cases {
            assert_eq!(tag_files(said, &seen), typed, "{said:?}");
        }
        assert!(tags_files("com.todesktop.230313mzl4w4u92") && tags_files("com.exafunction.windsurf") && tags_files("com.microsoft.VSCode"));
        assert!(!tags_files("com.apple.TextEdit") && !tags_files(""));
    }
}
