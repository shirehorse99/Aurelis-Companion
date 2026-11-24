//! Port of `ChannelCurationKeys.kt` — deterministic ordering keys for curation sort.
use super::channel_name_keys;

pub fn family_key(name: &str) -> String {
    channel_name_keys::normalize_for_match(&channel_name_keys::strip_timeshift_marker(name))
}

pub fn brand_key(name: &str) -> String {
    let stripped = channel_name_keys::strip_timeshift_marker(name);
    let all_words = channel_name_keys::normalize_words_for_match(&stripped);
    let significant: Vec<String> = all_words.iter().filter(|w| w.len() >= 3).cloned().collect();
    let words = if significant.is_empty() { &all_words } else { &significant };
    if words.is_empty() { return String::new(); }
    words.iter().take(2).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_key_strips_timeshift() {
        assert_eq!(family_key("ITV +1"), "itv1");
        assert_eq!(family_key("ITV"), "itv1");
        assert_eq!(family_key("ITV1 +1"), "itv1");
    }

    #[test]
    fn brand_key_two_words() {
        assert_eq!(brand_key("Sky Sports Football"), "skysports");
        assert_eq!(brand_key("Sky Sports 1"), "skysports");
    }

    #[test]
    fn brand_key_single_word() {
        assert_eq!(brand_key("CNN"), "cnn");
    }

    #[test]
    fn brand_key_short_words_fallback() {
        assert_eq!(brand_key("E4"), "e4");
    }
}
