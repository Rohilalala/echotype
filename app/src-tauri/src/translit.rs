//! Indian scripts in English letters, the way people type them in WhatsApp ("mera meeting 5 baje
//! hai", "vanakkam, eppadi irukkeenga"), for Settings `script` "latin". Whisper ignores a
//! romanised prompt for Tamil, Telugu, Kannada, Malayalam, Gujarati and Punjabi (stt::ROMANISED
//! has why), and now and then for Hindi, Marathi and Bengali too, so their text is converted here
//! after speech-to-text. Casual, not ISO 15919: no diacritics; long vowels doubled inside a word
//! (aa, ee, oo) and single at its end ("mera", "paani", "ji"); Bengali writes both short ("ami").
//! Every block follows Devanagari's layout, so one table by offset covers all eight scripts.
//! Latin, digits and punctuation pass through, so the English words of mixed speech stay as said.

use crate::settings::Settings;

/// The user's languages written in one of these scripts get converted.
pub const LANGS: &[&str] = &["hi", "mr", "bn", "gu", "pa", "ta", "te", "kn", "ml"];

/// English letters for a language whose script this module reads.
pub fn active(s: &Settings) -> bool {
    s.script == "latin" && s.languages.iter().any(|l| LANGS.contains(&l.as_str()))
}

/// The transcript in English letters when the user wants them, sentences starting with a capital.
pub fn apply(s: &Settings, text: &str) -> String {
    if active(s) { convert(text, true) } else { text.to_string() }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Script { Deva, Beng, Guru, Gujr, Taml, Telu, Knda, Mlym }
use Script::*;

fn block(c: char) -> Option<(Script, u32)> {
    let u = c as u32;
    let (sc, base) = match u {
        0x0900..=0x097F => (Deva, 0x0900),
        0x0980..=0x09FF => (Beng, 0x0980),
        0x0A00..=0x0A7F => (Guru, 0x0A00),
        0x0A80..=0x0AFF => (Gujr, 0x0A80),
        0x0B80..=0x0BFF => (Taml, 0x0B80),
        0x0C00..=0x0C7F => (Telu, 0x0C00),
        0x0C80..=0x0CFF => (Knda, 0x0C80),
        0x0D00..=0x0D7F => (Mlym, 0x0D00),
        _ => return None,
    };
    Some((sc, u - base))
}

/// Scripts whose words drop the inherent "a" people don't say (कमल is kamal, not kamala).
fn north(sc: Script) -> bool {
    matches!(sc, Deva | Beng | Guru | Gujr)
}

fn joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

/// Offsets 0x15..=0x39, Devanagari's order: क ख ग घ ङ च छ ज झ ञ ट ठ ड ढ ण त थ द ध न ऩ प फ ब भ म य र ऱ ल ळ ऴ व श ष स ह.
const CONS: [&str; 37] = [
    "k", "kh", "g", "gh", "n", "ch", "ch", "j", "jh", "n", "t", "th", "d", "dh", "n", "t", "th", "d", "dh", "n", "n", "p", "ph", "b", "bh", "m", "y", "r", "r", "l", "l", "zh", "v", "sh", "sh", "s", "h",
];
/// The nukta letters at 0x58..=0x5F (क़ ख़ ग़ ज़ ड़ ढ़ फ़ य़), and what a base letter becomes with ़.
fn nukta(sc: Script, i: usize) -> &'static str {
    let rr = sc != Deva; // ড় and ੜ are r in Bengali and Punjabi; Hindi types ड़ as d ("bada", "ladka")
    ["q", "kh", "g", "z", if rr { "r" } else { "d" }, if rr { "rh" } else { "dh" }, "f", "y"][i]
}
const NUKTA_BASE: [u32; 8] = [0x15, 0x16, 0x17, 0x1C, 0x21, 0x22, 0x2B, 0x2F];
const NASALS: [u32; 6] = [0x19, 0x1E, 0x23, 0x28, 0x29, 0x2E];

#[derive(Clone, Copy, PartialEq, Debug)]
enum V { Inherent, Sign(u32), Dead, Dropped }

/// A letter of a word. Vowels are kept as the offset of the independent vowel (0x05 अ ... 0x14 औ).
enum U { C { off: u32, lat: &'static str, v: V, gem: bool }, Vow(u32), Nasal(u32), Visarga, Raw(char) }

fn has_vowel(u: &U) -> bool {
    matches!(u, U::Vow(_) | U::C { v: V::Inherent | V::Sign(_), .. })
}

fn vowel(sc: Script, v: u32, last: bool) -> &'static str {
    let short = last || sc == Beng;
    match v {
        0x05 if sc == Beng => "o", // Bengali's inherent vowel: "kemon", "nomoskar"
        0x05 | 0x04 => "a",
        0x06 => if short { "a" } else { "aa" },
        0x07 => "i",
        0x08 => if short { "i" } else { "ee" },
        0x09 => "u",
        0x0A => if short { "u" } else { "oo" },
        0x0B | 0x0C | 0x60 => "ri",
        0x0D..=0x0F => "e",
        0x10 => "ai",
        0x11..=0x13 => "o",
        0x14 => "au",
        0x50 => "om",
        _ => "",
    }
}

/// Native letters in `text` to English letters; the rest is copied. `cap`: the text starts a
/// sentence, so its first converted word gets a capital (as do those after . ? !).
pub fn convert(text: &str, mut cap: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match block(c) {
            Some((_, off @ 0x66..=0x6F)) => {
                out.push(char::from(b'0' + (off - 0x66) as u8));
                cap = false;
                i += 1;
            }
            Some((Deva, 0x64 | 0x65)) => {
                out.push('.'); // । and ॥, which every one of these scripts uses
                cap = true;
                i += 1;
            }
            Some((sc, _)) => {
                let start = i;
                while i < chars.len() && (joiner(chars[i]) || block(chars[i]).is_some_and(|(s, o)| s == sc && !(0x64..=0x6F).contains(&o))) {
                    i += 1;
                }
                let w = word(sc, &chars[start..i]);
                let mut wc = w.chars();
                match wc.next() {
                    Some(f) if cap => out.extend(f.to_uppercase().chain(wc)),
                    _ => out.push_str(&w),
                }
                cap = cap && w.is_empty();
            }
            None => {
                out.push(c);
                cap = if c.is_alphanumeric() { false } else { cap || c == '\n' || ".!?".contains(c) };
                i += 1;
            }
        }
    }
    out
}

fn word(sc: Script, chars: &[char]) -> String {
    // Malayalam's old chillu (ന്‍ = ന + ് + ZWJ) is a letter without a vowel, unlike a bare final ്.
    let joined = chars.last().is_some_and(|&c| joiner(c));
    let cs: Vec<(u32, char)> = chars.iter().filter(|c| !joiner(**c)).map(|&c| (block(c).map_or(0, |b| b.1), c)).collect();
    let off = |i: usize| cs.get(i).map(|c| c.0);
    let (mut us, mut gem, mut i) = (Vec::<U>::new(), false, 0);
    while i < cs.len() {
        let (o, ch) = cs[i];
        i += 1;
        let cons = match (sc, o) {
            (Mlym, 0x7A..=0x7F) => Some((["n", "n", "r", "l", "l", "k"][(o - 0x7A) as usize], true)), // chillu ൺ ൻ ർ ൽ ൾ ൿ
            (Beng, 0x4E) => Some(("t", true)),                                                     // ৎ
            (Deva | Beng | Guru, 0x58..=0x5F) => Some((nukta(sc, (o - 0x58) as usize), false)),
            (_, 0x15..=0x39) => Some((CONS[(o - 0x15) as usize], false)),
            _ => None,
        };
        if let Some((mut lat, dead)) = cons {
            if off(i) == Some(0x3C) {
                lat = match (sc, o) {
                    (Guru, 0x38) => "sh", // ਸ਼
                    _ => NUKTA_BASE.iter().position(|&b| b == o).map_or(lat, |n| nukta(sc, n)),
                };
                i += 1;
            }
            let v = match off(i) {
                Some(0x4D) => {
                    i += 1;
                    V::Dead
                }
                Some(m @ 0x3E..=0x4C) => {
                    i += 1;
                    let mut v = m - 0x38;
                    // ொ typed as ெ + ா (and ௌ as ெ + ௗ), as Tamil and Malayalam allow
                    match off(i) {
                        Some(0x3E) if matches!(v, 0x0E | 0x0F) => (v, i) = (v + 4, i + 1),
                        Some(0x57) if matches!(v, 0x0E | 0x0F) => (v, i) = (0x14, i + 1),
                        _ => {}
                    }
                    V::Sign(v)
                }
                Some(0x57) if sc == Mlym => {
                    i += 1;
                    V::Sign(0x14) // ൗ, Malayalam's au
                }
                _ if dead => V::Dead,
                _ => V::Inherent,
            };
            us.push(U::C { off: o, lat, v, gem: std::mem::take(&mut gem) });
            continue;
        }
        match (sc, o) {
            (Guru, 0x71) => gem = true,                  // ੱ addak doubles the next letter
            (Guru, 0x70) | (_, 0x02) => us.push(U::Nasal(0x02)), // ੰ tippi, ं anusvara
            (_, 0x01) => us.push(U::Nasal(0x01)),
            (_, 0x03) => us.push(U::Visarga),
            (_, 0x04..=0x14 | 0x50 | 0x60) => us.push(U::Vow(o)),
            (_, m @ 0x3E..=0x4C) => us.push(U::Vow(m - 0x38)), // a vowel sign with no letter before it
            (_, 0x3C | 0x3D | 0x4D | 0x51..=0x57) => {}       // stray nukta, avagraha, virama, accents
            _ => us.push(U::Raw(ch)),
        }
    }
    let n = us.len();
    if north(sc) {
        // The final "a" goes (kamal, sampark), unless the word is one letter (na) or ends in a
        // cluster into r, y or v (mitra, satya).
        let cluster = matches!(us[n.saturating_sub(2)], U::C { v: V::Dead, .. }) && matches!(us[n - 1], U::C { off: 0x2F | 0x30 | 0x35, .. });
        if n > 1 && !cluster {
            if let U::C { v: v @ V::Inherent, .. } = &mut us[n - 1] {
                *v = V::Dropped;
            }
        }
        // And an "a" between a vowel and a consonant with a vowel, right to left (karna, samajhna).
        // ponytail: Ohala's one rule; compounds and some loans come out with one "a" too many or few.
        for i in (1..n.saturating_sub(1)).rev() {
            if matches!(us[i + 1], U::C { v: V::Inherent | V::Sign(_), .. }) && has_vowel(&us[i - 1]) {
                if let U::C { v: v @ V::Inherent, .. } = &mut us[i] {
                    *v = V::Dropped;
                }
            }
        }
    }
    let lats: Vec<&str> = (0..n).map(|i| letter(sc, &us, i)).collect();
    let mut out = String::new();
    for (i, u) in us.iter().enumerate() {
        let last = i + 1 == n;
        let final_nasal = i + 2 == n && matches!(us[n - 1], U::Nasal(_));
        match u {
            U::C { v, gem, .. } => {
                let lat = lats[i];
                if *gem && lat.len() == 1 {
                    out.push_str(lat); // ਪੱਕਾ pakka
                }
                // A doubled ch or th is typed cch, tth (accha, katthi), not chch.
                let doubled = *v == V::Dead && lat.len() > 1 && lats.get(i + 1).is_some_and(|nx| nx.len() > 1 && nx.starts_with(&lat[..1]));
                out.push_str(if doubled { &lat[..1] } else { lat });
                match *v {
                    V::Inherent => out.push_str(vowel(sc, 0x05, false)),
                    V::Sign(0x0F) if sc == Deva && final_nasal => out.push_str("ei"), // में mein, करें karein
                    V::Sign(x) => out.push_str(vowel(sc, x, last || (final_nasal && x == 0x08 && north(sc)))), // नहीं nahin
                    V::Dead if sc == Mlym && last && !joined && cs.last().is_some_and(|c| c.0 == 0x4D) => out.push('u'), // ഉണ്ട് is said undu
                    _ => {}
                }
            }
            U::Vow(x) => out.push_str(vowel(sc, *x, false)),
            U::Nasal(0x01) => out.push('n'),
            U::Nasal(_) => {
                let labial = matches!(us.get(i + 1), Some(U::C { off: 0x2A..=0x2E, .. }));
                out.push_str(match sc {
                    _ if labial => "m",
                    Beng => "ng", // বাংলা bangla
                    Mlym => "m",
                    Telu | Knda if last => "m", // నమస్కారం namaskaaram
                    _ => "n",
                });
            }
            U::Visarga => out.push('h'),
            U::Raw(c) => out.push(*c),
        }
    }
    out
}

/// A consonant as said where it stands. Tamil writes k/g, ch/s/j, t/d, th/dh and p/b with one
/// letter each: unvoiced at a word's start and doubled, voiced after a nasal, and k, ch, t, th
/// between vowels are said g, s, d, dh (magan, pesu, eppadi, adhu).
// ponytail: the common spoken rule; loans ("bas", "kaapi") follow it too, a word list if it matters.
fn letter(sc: Script, us: &[U], i: usize) -> &'static str {
    let U::C { off, lat, v, .. } = us[i] else { return "" };
    let prev = i.checked_sub(1).map(|p| &us[p]);
    let after = |set: &[u32]| matches!(prev, Some(U::C { off: p, v: V::Dead, .. }) if set.contains(p));
    let next_is = |o: u32| matches!(us.get(i + 1), Some(U::C { off, .. }) if *off == o);
    let dravidian = matches!(sc, Taml | Telu | Knda | Mlym);
    match (sc, off) {
        (Taml, 0x15 | 0x1A | 0x1F | 0x24 | 0x2A) => {
            let k = [0x15, 0x1A, 0x1F, 0x24, 0x2A].iter().position(|&x| x == off).unwrap_or(0);
            if v == V::Dead || after(&[off]) {
                ["k", "ch", "t", "th", "p"][k]
            } else if after(&NASALS) {
                ["g", "j", "d", "dh", "b"][k]
            } else if prev.is_some_and(has_vowel) {
                ["g", "s", "d", "dh", "p"][k]
            } else {
                ["k", "s", "t", "th", "p"][k]
            }
        }
        (Taml, 0x31) if after(&[0x29]) => "dr", // நன்றி nandri
        (Taml, 0x31) if v == V::Dead && next_is(0x31) => "t", // காற்று kaatru
        (Mlym, 0x1F) if after(&[0x23]) => "d", // ണ്ട nd
        (Taml | Mlym, 0x19) if after(&[0x19]) => "g", // ങ്ങ ng
        (_, 0x1E) if dravidian && v != V::Dead => "nj", // ഞാൻ njaan
        (_, 0x24 | 0x25) if dravidian => "th", // the dental t: thambi, thumba, enthu
        _ => lat,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(pairs: &[(&str, &str)]) {
        for (native, want) in pairs {
            assert_eq!(convert(native, false), *want, "{native}");
        }
    }

    #[test]
    fn hindi_and_marathi() {
        check(&[
            ("नमस्ते, आप कैसे हैं?", "namaste, aap kaise hain?"),
            ("मैं ठीक हूँ, धन्यवाद।", "main theek hoon, dhanyavaad."),
            ("मेरा meeting 5 बजे है", "mera meeting 5 baje hai"),
            ("कमल", "kamal"),
            ("करना समझना बोलना", "karna samajhna bolna"),
            ("एक दो तीन चार पाँच", "ek do teen chaar paanch"),
            ("नहीं, घर में", "nahin, ghar mein"),
            ("अच्छा, पानी", "accha, paani"),
            ("मित्र राम का", "mitra raam ka"),
            ("ज़िंदगी फ़ोन बड़ा", "zindagi fon bada"),
            ("नमस्कार, तुम्ही कसे आहात?", "namaskaar, tumhi kase aahaat?"),
            ("मीटिंग", "meeting"),
        ]);
    }

    #[test]
    fn bengali() {
        check(&[
            ("নমস্কার, আপনি কেমন আছেন?", "nomoskar, apni kemon achen?"),
            ("আমি ভালো আছি", "ami bhalo achi"),
            ("ধন্যবাদ", "dhonyobad"),
            ("বাংলা", "bangla"),
            ("আমার নাম", "amar nam"),
            ("করবো", "korbo"),
            ("এক দুই তিন", "ek dui tin"),
            ("বাড়ি সময়", "bari somoy"),
        ]);
    }

    #[test]
    fn gujarati_and_punjabi() {
        check(&[
            ("કેમ છો?", "kem cho?"),
            ("નમસ્તે", "namaste"),
            ("આભાર", "aabhaar"),
            ("મારું નામ", "maarun naam"),
            ("એક બે ત્રણ", "ek be tran"),
            ("હું મજામાં છું", "hun majaamaan chun"),
            ("આવજો", "aavjo"),
            ("ગુજરાતી", "gujraati"),
            ("ਸਤ ਸ੍ਰੀ ਅਕਾਲ", "sat sri akaal"),
            ("ਕਿਵੇਂ ਹੋ?", "kiven ho?"),
            ("ਪੱਕਾ", "pakka"),
            ("ਪੰਜਾਬ", "panjaab"),
            ("ਇੱਕ ਦੋ ਤਿੰਨ", "ikk do tinn"),
            ("ਜ਼ਿੰਦਗੀ ਸ਼ੇਰ", "zindagi sher"),
            ("ਮੇਰਾ ਨਾਮ", "mera naam"),
            ("ਕਰਨਾ", "karna"),
        ]);
    }

    #[test]
    fn tamil() {
        check(&[
            ("வணக்கம், எப்படி இருக்கீங்க?", "vanakkam, eppadi irukkeenga?"),
            ("நன்றி", "nandri"),
            ("தமிழ்", "thamizh"),
            ("ரொம்ப நல்லா இருக்கு", "romba nalla irukku"),
            ("ஒன்று இரண்டு மூன்று", "ondru irandu moondru"),
            ("நான் வேண்டாம்", "naan vendaam"),
            ("அது சரி", "adhu sari"),
            ("கொஞ்சம் பேசு", "konjam pesu"),
            ("காற்று மகன்", "kaatru magan"),
            ("அக்கா தம்பி", "akka thambi"),
        ]);
    }

    #[test]
    fn telugu_and_kannada() {
        check(&[
            ("నమస్కారం", "namaskaaram"),
            ("ఎలా ఉన్నారు?", "ela unnaaru?"),
            ("బాగున్నాను, ధన్యవాదాలు", "baagunnaanu, dhanyavaadaalu"),
            ("ఒకటి రెండు మూడు", "okati rendu moodu"),
            ("అంటే", "ante"),
            ("తెలుగు", "thelugu"),
            ("నా పేరు", "na peru"),
            ("ఏమిటి", "emiti"),
            ("ನಮಸ್ಕಾರ", "namaskaara"),
            ("ಹೇಗಿದ್ದೀರಾ?", "hegiddeera?"),
            ("ಚೆನ್ನಾಗಿದ್ದೀನಿ", "chennaagiddeeni"),
            ("ಧನ್ಯವಾದಗಳು", "dhanyavaadagalu"),
            ("ಒಂದು ಎರಡು ಮೂರು", "ondu eradu mooru"),
            ("ತುಂಬಾ", "thumba"),
            ("ಕನ್ನಡ", "kannada"),
            ("ಬೇಕು", "beku"),
        ]);
    }

    #[test]
    fn malayalam() {
        check(&[
            ("നമസ്കാരം", "namaskaaram"),
            ("സുഖമാണോ?", "sukhamaano?"),
            ("ഞാൻ", "njaan"),
            ("നന്ദി", "nandi"),
            ("ഒന്ന് രണ്ട് മൂന്ന്", "onnu randu moonnu"),
            ("ഉണ്ട്", "undu"),
            ("എന്ത്", "enthu"),
            ("മലയാളം", "malayaalam"),
            ("ന്\u{200D}", "n"), // the old chillu: no u
        ]);
    }

    #[test]
    fn digits_punctuation_and_joiners() {
        check(&[
            ("५ बजे, १० मिनट।", "5 baje, 10 minat."),
            ("௫ ౬ ೭ ൮ ৯ ੩ ૪", "5 6 7 8 9 3 4"),
            ("क्\u{200D}ष", "ksh"),
            ("क\u{200C}ल", "kal"),
            ("हम॥", "ham."),
        ]);
    }

    #[test]
    fn anusvara_before_p_b_m_is_m() {
        check(&[("संपर्क", "sampark"), ("अंबर", "ambar"), ("कंगन", "kangan"), ("ಸಂಬಳ", "sambala")]);
    }

    #[test]
    fn latin_and_other_scripts_are_untouched() {
        for t in ["Hello, how are you? I'm fine.", "Привет, как дела?", "السلام علیکم", "日本語", "Namaste, aap kaise hain?", ""] {
            assert_eq!(convert(t, true), t);
            assert_eq!(convert(&convert(t, true), true), t, "idempotent");
        }
        let once = convert("मेरा meeting 5 बजे है। வணக்கம்!", true);
        assert_eq!(convert(&once, true), once);
    }

    #[test]
    fn the_settings_samples_are_what_gets_typed() {
        // main.js SCRIPT_SAMPLE
        for (native, latin) in [
            ("નમસ્તે, કેમ છો?", "Namaste, kem cho?"),
            ("ਸਤ ਸ੍ਰੀ ਅਕਾਲ, ਕਿਵੇਂ ਹੋ?", "Sat sri akaal, kiven ho?"),
            ("வணக்கம், எப்படி இருக்கீங்க?", "Vanakkam, eppadi irukkeenga?"),
            ("నమస్కారం, ఎలా ఉన్నారు?", "Namaskaaram, ela unnaaru?"),
            ("ನಮಸ್ಕಾರ, ಹೇಗಿದ್ದೀರಾ?", "Namaskaara, hegiddeera?"),
            ("നമസ്കാരം, സുഖമാണോ?", "Namaskaaram, sukhamaano?"),
        ] {
            assert_eq!(convert(native, true), latin);
        }
    }

    #[test]
    fn mixed_scripts_and_capitals() {
        assert_eq!(convert("मेरा meeting 5 बजे है। வணக்கம், Arnav! ಹೇಗಿದ್ದೀರಾ?", true), "Mera meeting 5 baje hai. Vanakkam, Arnav! Hegiddeera?");
        assert_eq!(convert("ok नमस्ते", true), "ok namaste", "a sentence started in English keeps going");
    }

    /// Speech in each script through the real model, then here: English letters only.
    /// ECHOTYPE_TEST_MODEL=<large-v3-turbo> ECHOTYPE_TEST_CLIPS=<folder of <code>.wav, 16 kHz mono,
    /// e.g. `say -v Vani -o ta.aiff "…"`> `cargo test --release translit_speech -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn translit_speech() {
        use crate::stt;
        let (Ok(model), Ok(dir)) = (std::env::var("ECHOTYPE_TEST_MODEL"), std::env::var("ECHOTYPE_TEST_CLIPS")) else { return eprintln!("skipped") };
        for code in LANGS {
            let Ok(wav) = std::fs::read(std::path::Path::new(&dir).join(format!("{code}.wav"))) else { continue };
            let pcm = crate::audio::decode(wav, Some("wav")).unwrap();
            let mut s = Settings { languages: vec![code.to_string()], script: "latin".into(), ..Default::default() };
            let raw = stt::transcribe_local(std::path::Path::new(&model), &pcm, &stt::languages(&s), &stt::prompt(&s), None).unwrap();
            let out = apply(&s, &raw);
            s.script = "native".into();
            let native = stt::transcribe_local(std::path::Path::new(&model), &pcm, &stt::languages(&s), &stt::prompt(&s), None).unwrap();
            eprintln!("{code} native: {native}\n{code} whisper (latin mode): {raw}\n{code} typed: {out}\n");
            assert!(out.chars().all(|c| !c.is_alphabetic() || c.is_ascii()) && out.chars().any(char::is_alphabetic), "{code}: {out:?}");
        }
        stt::unload(); // ggml-metal asserts at exit while a model is resident
    }

    #[test]
    fn only_in_english_letters_for_these_languages() {
        let mut s = Settings::default();
        s.languages = vec!["en".into(), "ta".into()];
        assert_eq!(apply(&s, "வணக்கம்"), "வணக்கம்", "native script asked for");
        s.script = "latin".into();
        assert_eq!(apply(&s, "வணக்கம்"), "Vanakkam");
        s.languages = vec!["en".into(), "ur".into()];
        assert_eq!(apply(&s, "सब"), "सब", "Urdu is not converted, so nothing is");
    }
}
