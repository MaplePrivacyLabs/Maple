//! Spell checker for the composer. A Hunspell-compatible en_US dictionary
//! is embedded in the binary and parsed once on a background thread, so
//! startup and the UI thread never wait for it.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};

use spellbook::Dictionary;
use unicode_segmentation::UnicodeSegmentation;

static DICTIONARY: OnceLock<Dictionary> = OnceLock::new();

/// Bumped when the dictionary or the user word list becomes available, so
/// an input checked before the load can tell its ranges are stale.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Current dictionary generation; compare with the value saved at the
/// last check to see if a re-check is needed.
pub fn generation() -> (u32, bool) {
    (GENERATION.load(Ordering::Acquire), enabled())
}

fn enabled() -> bool {
    super::i18n::current_locale().language.as_str() == "en"
}

/// Words the user added with "Add to dictionary". Persisted one per line
/// in `dictionary.txt` next to the settings file.
static USER_WORDS: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// The host supplies dictionary words after its store opens. The GUI never
/// opens a dictionary sidecar or SQLite connection.
pub fn install_user_words(words: impl IntoIterator<Item = String>) {
    let mut words: HashSet<String> = words.into_iter().collect();
    words.extend(crate::settings::pending_dictionary_words());
    *USER_WORDS.write().unwrap_or_else(|e| e.into_inner()) = Some(words);
    GENERATION.fetch_add(1, Ordering::Release);
}

pub fn clear_user_words() {
    *USER_WORDS.write().unwrap_or_else(|e| e.into_inner()) = None;
    GENERATION.fetch_add(1, Ordering::Release);
}

fn load_user_words() -> HashSet<String> {
    HashSet::new()
}

/// Remember `word` for this user and write the list in the background.
pub fn add_word(word: &str) {
    let word = word.trim().trim_matches('\'').to_owned();
    if word.is_empty() {
        return;
    }
    let mut guard = USER_WORDS.write().unwrap_or_else(|e| e.into_inner());
    let words = guard.get_or_insert_with(load_user_words);
    if !words.insert(word.clone()) {
        return;
    }
    drop(guard);
    crate::settings::queue_dictionary_word(word);
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Whether the user list accepts `word` as typed, or in lower case, or
/// stripped of surrounding apostrophes. `words` is the list snapshot the
/// caller already holds, so a pass over a text locks it once.
fn is_user_word(words: &HashSet<String>, word: &str) -> bool {
    let bare = word.trim_matches('\'');
    if words.contains(word) || (bare != word && words.contains(bare)) {
        return true;
    }
    let lower = word.to_lowercase();
    let lower_bare = lower.trim_matches('\'');
    words.contains(&lower) || (lower_bare != lower && words.contains(lower_bare))
}

/// Start the dictionary parse on a background thread. Safe to call more
/// than once; only the first call does work.
pub fn preload() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("spell-dict".into())
            .spawn(|| {
                // The user list is small, but it is a file read, so it
                // stays off the UI thread with the dictionary parse.
                let user_words = load_user_words();
                {
                    let mut guard = USER_WORDS.write().unwrap_or_else(|e| e.into_inner());
                    if guard.is_none() {
                        *guard = Some(user_words);
                    }
                }
                let aff = include_str!("../../assets/dict/en_US.aff");
                let dic = include_str!("../../assets/dict/en_US.dic");
                let started = std::time::Instant::now();
                match Dictionary::new(aff, dic) {
                    Ok(mut dictionary) => {
                        for word in include_str!("../../assets/dict/extra.txt")
                            .lines()
                            .map(str::trim)
                            .filter(|line| !line.is_empty() && !line.starts_with('#'))
                        {
                            if let Err(error) = dictionary.add(word) {
                                log::warn!("Cannot add {word:?} to the spell dictionary: {error}");
                            }
                        }
                        let _ = DICTIONARY.set(dictionary);
                        GENERATION.fetch_add(1, Ordering::AcqRel);
                        log::debug!("Spell dictionary loaded in {:?}", started.elapsed());
                    }
                    Err(error) => log::warn!("Cannot load the spell dictionary: {error}"),
                }
            })
            .ok();
    });
}

fn dictionary() -> Option<&'static Dictionary> {
    DICTIONARY.get()
}

#[cfg(test)]
fn is_ready() -> bool {
    dictionary().is_some()
}

/// Words the checker skips: anything with a digit, an ALL-CAPS token
/// (acronyms, env vars), and a word next to path or URL punctuation.
fn should_check(text: &str, range: &Range<usize>, word: &str) -> bool {
    if word.chars().any(|ch| ch.is_ascii_digit()) {
        return false;
    }
    if word.chars().filter(|ch| ch.is_alphabetic()).count() < 2 {
        return false;
    }
    if word.chars().all(|ch| !ch.is_lowercase()) {
        return false;
    }
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    let glue = |ch: Option<char>| {
        matches!(
            ch,
            Some('/' | '\\' | '.' | ':' | '@' | '#' | '_' | '-' | '`' | '=' | '~')
        )
    };
    !(glue(before) || glue(after))
}

/// Byte ranges of the words in `text` that the dictionary rejects. Empty
/// until the dictionary is loaded. Text inside backtick code spans is
/// skipped.
///
/// This runs on the UI thread after every keystroke, so it avoids work
/// per word: the user list is locked once per pass, a token is copied
/// only when it holds a curly quote, and the lower-case form is built
/// only for a word the dictionary already rejected.
pub fn misspelled_ranges(text: &str) -> Vec<Range<usize>> {
    if !enabled() {
        return Vec::new();
    }
    let Some(dictionary) = dictionary() else {
        return Vec::new();
    };
    let user_words = USER_WORDS.read().unwrap_or_else(|e| e.into_inner());
    let user_words = user_words.as_ref();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut straightened = String::new();
    for (start, token) in text.split_word_bound_indices() {
        if token.starts_with('`') {
            in_code = !in_code;
            continue;
        }
        if in_code || !token.chars().any(|ch| ch.is_alphabetic()) {
            continue;
        }
        let range = start..start + token.len();
        // Segmentation keeps a trailing apostrophe-s attached ("it's"), but
        // curly quotes make the checker fall back to the raw token.
        let word: &str = if token.contains('\u{2019}') {
            straightened.clear();
            straightened.extend(
                token
                    .chars()
                    .map(|ch| if ch == '\u{2019}' { '\'' } else { ch }),
            );
            &straightened
        } else {
            token
        };
        if !should_check(text, &range, word) {
            continue;
        }
        if dictionary.check(word) {
            continue;
        }
        let bare = word.trim_matches('\'');
        if bare != word && dictionary.check(bare) {
            continue;
        }
        if user_words.is_some_and(|words| is_user_word(words, word)) {
            continue;
        }
        out.push(range);
    }
    out
}

/// Words longer than this get no suggestions: the ngram pass scales with
/// word length (60-70 ms at 35+ chars) and such tokens are rarely typos.
const MAX_SUGGEST_LEN: usize = 20;

/// Up to `limit` replacement candidates for a misspelled word.
///
/// Performance: this runs synchronously on the UI thread and costs
/// 5-20 ms for a typical typo, because spellbook's ngram pass scans
/// every stem in the dictionary. That is accepted on purpose: it runs
/// once per right-click, when the user has already paused, and the
/// alternative (open the menu empty and fill it in later) reads as a
/// bug. If it ever shows in a profile, precompute suggestions in the
/// background for each newly flagged word after `misspelled_ranges`
/// instead of making this call async.
pub fn suggestions(word: &str, limit: usize) -> Vec<String> {
    if !enabled() {
        return Vec::new();
    }
    let Some(dictionary) = dictionary() else {
        return Vec::new();
    };
    if word.chars().count() > MAX_SUGGEST_LEN {
        return Vec::new();
    }
    let mut out = Vec::new();
    dictionary.suggest(&word.replace('\u{2019}', "'"), &mut out);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load() {
        preload();
        while !is_ready() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn spanish_disables_english_dictionary_and_switching_back_restores_it() {
        let _guard = crate::ui::i18n::TestLocalizationGuard::enable();
        load();
        crate::ui::i18n::apply_preference(Some("en"));
        let english_generation = generation();
        assert!(!misspelled_ranges("chekc").is_empty());
        crate::ui::i18n::apply_preference(Some("es"));
        assert_ne!(generation(), english_generation);
        assert!(misspelled_ranges("mañana chekc").is_empty());
        assert!(suggestions("mañana", 4).is_empty());
        crate::ui::i18n::apply_preference(Some("en"));
        assert!(!misspelled_ranges("chekc").is_empty());
    }

    #[test]
    fn flags_misspelled_words_only() {
        load();
        let text = "Please chekc the README at ./src/main.rs and `foo_bar` v2";
        let ranges = misspelled_ranges(text);
        let words: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(words, vec!["chekc"]);
    }

    #[test]
    fn accepts_contractions_and_capitalized_words() {
        load();
        assert!(misspelled_ranges("It's fine. Don't worry, Ben.").is_empty());
        assert!(misspelled_ranges("It\u{2019}s fine").is_empty());
    }

    #[test]
    fn accepts_bundled_extra_words() {
        load();
        assert!(misspelled_ranges("Maple talks to OpenSecret over gpui").is_empty());
    }

    #[test]
    fn user_words_match_case_insensitively_and_without_quotes() {
        let words: HashSet<String> = ["maple", "Ben"].map(String::from).into_iter().collect();
        assert!(is_user_word(&words, "maple"));
        assert!(is_user_word(&words, "Maple"));
        assert!(is_user_word(&words, "'maple'"));
        assert!(is_user_word(&words, "Ben"));
        assert!(!is_user_word(&words, "ben"));
        assert!(!is_user_word(&words, "oak"));
    }

    #[test]
    fn suggests_replacements() {
        load();
        let suggestions = suggestions("chekc", 3);
        assert!(suggestions.iter().any(|s| s == "check"), "{suggestions:?}");
    }
}
