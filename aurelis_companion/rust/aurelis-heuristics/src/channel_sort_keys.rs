//! Port of `ChannelSortKeys.kt` — LCN extraction and sort keys.
use super::locale_channel_plan;
use regex::Regex;
use std::sync::LazyLock;

static LEADING_NUMBER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:ch(?:annel)?\s*)?(\d{1,4})\b").unwrap()
});
static EMBEDDED_CH_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bch\s*(\d{1,4})\b").unwrap()
});
static TRAILING_DIAL_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s(\d{3,4})\s*(?:\+\+.*)?$").unwrap()
});

const NUMERIC_PRIMARY_COUNTRIES: &[&str] = &[
    "US", "GB", "HK", "CA", "DE", "ES", "FR", "IT", "PT", "NL", "AU", "IE", "NZ",
];

pub fn dial_number_from_playlist_or_name(channel_name: &str, playlist_channel_id: Option<&str>) -> Option<i32> {
    if let Some(id) = playlist_channel_id {
        let trimmed = id.trim();
        if let Ok(n) = trimmed.parse::<i32>() {
            if (1..=9_999).contains(&n) { return Some(n); }
        }
    }
    let name = channel_name.trim();
    if let Some(caps) = LEADING_NUMBER_REGEX.captures(name) {
        if let Ok(n) = caps[1].parse::<i32>() {
            if (1..=9_999).contains(&n) { return Some(n); }
        }
    }
    if let Some(caps) = EMBEDDED_CH_REGEX.captures(name) {
        if let Ok(n) = caps[1].parse::<i32>() {
            if (1..=9_999).contains(&n) { return Some(n); }
        }
    }
    if let Some(caps) = TRAILING_DIAL_REGEX.captures(name) {
        if let Ok(n) = caps[1].parse::<i32>() {
            if (1..=9_999).contains(&n) { return Some(n); }
        }
    }
    None
}

pub fn extract_lcn(channel_name: &str, playlist_channel_id: Option<&str>, country_code: Option<&str>) -> Option<i32> {
    if let Some(cc) = country_code.filter(|cc| cc.eq_ignore_ascii_case("GB")) {
        if let Some(dial @ 424..=426) = locale_channel_plan::rank_for(Some(cc), channel_name, None) {
            return Some(dial);
        }
    }
    if let Some(d) = dial_number_from_playlist_or_name(channel_name, playlist_channel_id) {
        return Some(d);
    }
    if let Some(cc) = country_code {
        if !cc.trim().is_empty() {
            if let Some(r) = locale_channel_plan::rank_for(Some(cc), channel_name, None) {
                return Some(r);
            }
        }
    }
    None
}

pub fn lcn_sort_key(channel_name: &str, playlist_channel_id: Option<&str>, country_code: Option<&str>) -> i32 {
    extract_lcn(channel_name, playlist_channel_id, country_code).unwrap_or(i32::MAX)
}

pub fn uses_numeric_primary_ordering(country_code: Option<&str>) -> bool {
    country_code
        .map(|cc| NUMERIC_PRIMARY_COUNTRIES.contains(&cc.to_uppercase().as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dial_from_playlist_id() {
        assert_eq!(dial_number_from_playlist_or_name("BBC One", Some("101")), Some(101));
    }

    #[test]
    fn dial_from_name_leading() {
        assert_eq!(dial_number_from_playlist_or_name("101 BBC One", None), Some(101));
    }

    #[test]
    fn dial_from_name_embedded_ch() {
        assert_eq!(dial_number_from_playlist_or_name("Sky Ch 401", None), Some(401));
    }

    #[test]
    fn dial_from_name_trailing() {
        assert_eq!(dial_number_from_playlist_or_name("Fox Sports 501", None), Some(501));
    }

    #[test]
    fn assigned_uk_sport_dial_takes_priority_over_playlist_metadata() {
        assert_eq!(extract_lcn("Racing TV", Some("1801"), Some("GB")), Some(424));
        assert_eq!(extract_lcn("DAZN 1", Some("1802"), Some("GB")), Some(425));
        assert_eq!(extract_lcn("Rally TV", Some("1803"), Some("GB")), Some(426));
        assert_eq!(extract_lcn("DAZN 1", Some("1802"), Some("DE")), Some(1802));
    }

    #[test]
    fn lcn_sort_key_unknown() {
        assert_eq!(lcn_sort_key("Unknown Channel", None, None), i32::MAX);
    }

    #[test]
    fn uses_numeric_primary_ordering_gb() {
        assert!(uses_numeric_primary_ordering(Some("GB")));
        assert!(!uses_numeric_primary_ordering(Some("XX")));
        assert!(!uses_numeric_primary_ordering(None));
    }
}
