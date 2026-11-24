use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jstring};
use jni::Env;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct ParsedSidesOut {
    pub home: String,
    pub away: String,
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_repository_OddsNativeBridge_nativeParseSides(
    mut env: EnvUnowned,
    _class: JClass,
    title: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let title_str = jstring_to_string(env, &title);

        if let Some((home, away)) = parse_sides(&title_str) {
            let out = ParsedSidesOut { home, away };
            let json = serde_json::to_string(&out).unwrap_or_default();
            env.new_string(json).unwrap_or_default().into_raw()
        } else {
            std::ptr::null_mut()
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_repository_OddsNativeBridge_nativeCleanName(
    mut env: EnvUnowned,
    _class: JClass,
    name: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let name_str = jstring_to_string(env, &name);

        let cleaned = clean_name(&name_str);
        env.new_string(cleaned).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_data_repository_OddsNativeBridge_nativeNamesMatch(
    mut env: EnvUnowned,
    _class: JClass,
    odds_name: JString,
    fixture_name: JString,
) -> jboolean {
    env.with_env(|env| -> std::result::Result<jboolean, jni::errors::Error> {
        Ok({
        let o_str: String = match env.get_string(&odds_name) {
            Ok(s) => s.into(),
            Err(_) => return Ok(false),
        };
        let f_str: String = match env.get_string(&fixture_name) {
            Ok(s) => s.into(),
            Err(_) => return Ok(false),
        };

        if names_match(&o_str, &f_str) {
            true
        } else {
            false
        }
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

fn parse_sides(title: &str) -> Option<(String, String)> {
    // ASCII-only lowering keeps every byte offset of `title`, so `idx` below can slice the
    // original. Full `to_lowercase()` does not: Turkish "İ" lowers to two chars (one byte
    // longer), which shifted the cut ("İstanbul Başakşehir v Galatasaray" gave "alatasaray")
    // and, with two of them before the separator, sliced mid-character -- a panic, and with
    // `panic = "abort"` the whole app. The separators are ASCII, so nothing else changes.
    let lower = title.to_ascii_lowercase();
    let separators = [" vs. ", " vs ", " v ", " @ ", " at ", " - ", " / "];
    
    let mut chosen_sep = None;
    for &sep in &separators {
        if lower.contains(sep) {
            chosen_sep = Some(sep);
            break;
        }
    }
    
    let sep = chosen_sep?;
    let idx = lower.find(sep)?;
    
    let home = title[0..idx].trim().to_lowercase();
    let away = title[idx + sep.len()..].trim().to_lowercase();
    
    if home.is_empty() || away.is_empty() {
        None
    } else {
        Some((home, away))
    }
}

static RE_TOKENS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\b(fc|united|utd|city|town|rovers|albion|athletic|county|wanderers|hotspur|spurs|real|club|de|cf|sc)\b").unwrap()
});
static RE_CHARS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"[^a-z0-9\s]").unwrap());
static RE_SPACES: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn clean_name(name: &str) -> String {
    let lowered = name.to_lowercase();
    let without_tokens = RE_TOKENS.replace_all(&lowered, "");
    let without_chars = RE_CHARS.replace_all(&without_tokens, "");
    let collapsed_spaces = RE_SPACES.replace_all(&without_chars, " ");
    collapsed_spaces.trim().to_string()
}

fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let s1_chars: Vec<char> = s1.chars().collect();
    let s2_chars: Vec<char> = s2.chars().collect();
    
    let mut dp = vec![0; s2_chars.len() + 1];
    for j in 0..=s2_chars.len() {
        dp[j] = j;
    }
    
    for i in 1..=s1_chars.len() {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=s2_chars.len() {
            let temp = dp[j];
            if s1_chars[i - 1] == s2_chars[j - 1] {
                dp[j] = prev;
            } else {
                dp[j] = std::cmp::min(
                    dp[j - 1],
                    std::cmp::min(dp[j], prev)
                ) + 1;
            }
            prev = temp;
        }
    }
    dp[s2_chars.len()]
}

fn levenshtein_similarity(s1: &str, s2: &str) -> f64 {
    let s1_len = s1.chars().count();
    let s2_len = s2.chars().count();
    let max_len = std::cmp::max(s1_len, s2_len);
    if max_len == 0 {
        return 1.0;
    }
    1.0 - (levenshtein_distance(s1, s2) as f64) / (max_len as f64)
}

fn names_match(odds_name: &str, fixture_name: &str) -> bool {
    let o = odds_name.to_lowercase().trim().to_string();
    let f = fixture_name.to_lowercase().trim().to_string();
    
    // A blank name matches nothing: `contains("")` is true of every string, so a fixture or an
    // outcome with an empty side was "the same team" as whichever one it was held against.
    if o.is_empty() || f.is_empty() {
        return false;
    }
    if o == f || o.contains(&f) || f.contains(&o) {
        return true;
    }
    
    let clean_o = clean_name(&o);
    let clean_f = clean_name(&f);
    if clean_o.is_empty() || clean_f.is_empty() {
        return false;
    }
    if clean_o == clean_f || clean_o.contains(&clean_f) || clean_f.contains(&clean_o) {
        return true;
    }
    
    let normalized_o = clean_o.replace("manchester", "man").replace("utd", "united");
    let normalized_f = clean_f.replace("manchester", "man").replace("utd", "united");
    if normalized_o == normalized_f || normalized_o.contains(&normalized_f) || normalized_f.contains(&normalized_o) {
        return true;
    }
    
    levenshtein_similarity(&clean_o, &clean_f) >= 0.75
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sides_splits_plain_titles() {
        assert_eq!(parse_sides("Arsenal v Chelsea"), Some(("arsenal".into(), "chelsea".into())));
        assert_eq!(parse_sides("Chiefs @ Raiders"), Some(("chiefs".into(), "raiders".into())));
        assert_eq!(parse_sides("Galatasaray VS Fenerbahçe"), Some(("galatasaray".into(), "fenerbahçe".into())));
        assert_eq!(parse_sides("No separator here"), None);
    }

    #[test]
    fn parse_sides_keeps_the_cut_where_lowercasing_changes_length() {
        // "İ" lowers to "i̇" (one byte longer). Mirrors OddsApiRepository.parseSides, which
        // finds the separator in the original title.
        assert_eq!(
            parse_sides("İstanbul Başakşehir v Galatasaray"),
            Some(("i̇stanbul başakşehir".into(), "galatasaray".into())),
        );
        // Two of them used to slice inside "ö" and abort the process.
        assert_eq!(parse_sides("İİ v Göztepe"), Some(("i̇i̇".into(), "göztepe".into())));
    }

    #[test]
    fn names_match_tolerates_suffixes() {
        assert!(names_match("Manchester United", "Man Utd"));
        assert!(names_match("Galatasaray", "galatasaray sk"));
        assert!(!names_match("Arsenal", "Chelsea"));
        // Twin: OddsApiRepositoryTest `a blank name matches nothing`.
        assert!(!names_match("Arsenal", ""));
        assert!(!names_match("  ", "Chelsea"));
        assert!(!names_match("", ""));
    }
}
