use jni::{EnvUnowned};
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::Env;
use quick_xml::events::Event;
use quick_xml::Reader;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct WeatherWarningOut {
    pub level: String, // "YELLOW", "AMBER", "RED"
    pub type_str: String,
    pub region: String,
    pub headline: String,
    pub valid_text: Option<String>,
    pub link: Option<String>,
}

struct RawItem {
    title: String,
    description: Option<String>,
    link: Option<String>,
}

fn jstring_to_string(env: &mut Env, s: &JString) -> String {
    env.get_string(s).map(|s| s.into()).unwrap_or_default()
}

static TITLE_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(Yellow|Amber|Red)\s+warning\s+of\s+(.+?)\s+affecting\s+(.+?)\s*$").unwrap()
});
static VALID_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)valid from [^.\n]*?(?:to|until)[^.\n]*").unwrap()
});

pub fn parse_weather_warnings(xml: &str) -> Vec<WeatherWarningOut> {
    let items = parse_rss_items(xml);

    let mut warnings = Vec::new();

    for item in items {
        if let Some(caps) = TITLE_REGEX.captures(&item.title) {
            let level_str = caps.get(1).map(|m| m.as_str().to_lowercase()).unwrap_or_default();
            let level = match level_str.as_str() {
                "red" => "RED",
                "amber" => "AMBER",
                _ => "YELLOW",
            };
            let type_str = caps.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            let region = caps.get(3).map(|m| m.as_str().trim().to_string()).unwrap_or_default();
            let headline = item.title.trim().to_string();

            let valid_text = item.description.as_ref().and_then(|desc| {
                VALID_REGEX.find(desc).map(|m| m.as_str().trim().to_string())
            });

            warnings.push(WeatherWarningOut {
                level: level.to_string(),
                type_str,
                region,
                headline,
                valid_text,
                link: item.link,
            });
        }
    }

    warnings
}

#[no_mangle]
pub extern "system" fn Java_com_aurelis_tv_domain_home_MetOfficeNativeBridge_nativeParse(
    mut env: EnvUnowned,
    _class: JClass,
    xml: JString,
) -> jstring {
    env.with_env(|env| -> std::result::Result<jstring, jni::errors::Error> {
        Ok({
        let xml_str = jstring_to_string(env, &xml);
        let warnings = parse_weather_warnings(&xml_str);
        let out_json = serde_json::to_string(&warnings).unwrap_or_else(|_| "[]".to_string());
        env.new_string(out_json).unwrap_or_default().into_raw()
        })
    }).resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

fn parse_rss_items(xml: &str) -> Vec<RawItem> {
    let mut reader = Reader::from_str(xml);
    // Not trim_text: it trims every text event, and quick-xml 0.42 splits text at each `&amp;`, so the
    // spaces either side of a reference were lost ("Highlands & Eilean Siar" read "HighlandsEilean Siar").
    // Each field is trimmed whole when the item ends instead.

    let mut buf = Vec::new();
    let mut items = Vec::new();

    let mut in_item = false;
    let mut current_tag = String::new();
    let mut title = String::new();
    let mut description = String::new();
    let mut link = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let name = e.name().as_ref().to_lowercase();
                if name == "item" {
                    in_item = true;
                    title.clear();
                    description.clear();
                    link.clear();
                } else if in_item {
                    current_tag = name;
                }
            }
            Ok(Event::Text(e)) => {
                if in_item {
                    let raw = e.into_inner().into_owned();
                    let text = match quick_xml::escape::unescape(&raw) {
                        Ok(s) => s.into_owned(),
                        Err(_) => raw,
                    };
                    match current_tag.as_str() {
                        "title" => title.push_str(&text),
                        "description" => description.push_str(&text),
                        "link" => link.push_str(&text),
                        _ => {}
                    }
                }
            }
            // quick-xml 0.42 reports `&amp;`, `&#193;` etc. as their own events, never inside Event::Text.
            Ok(Event::GeneralRef(r)) => {
                if in_item {
                    let name = r.as_ref().to_string();
                    let text = match resolve_reference(&name) {
                        Some(c) => c.to_string(),
                        None => format!("&{name};"),
                    };
                    match current_tag.as_str() {
                        "title" => title.push_str(&text),
                        "description" => description.push_str(&text),
                        "link" => link.push_str(&text),
                        _ => {}
                    }
                }
            }
            Ok(Event::CData(e)) => {
                if in_item {
                    let raw = e.into_inner().into_owned();
                    match current_tag.as_str() {
                        "title" => title.push_str(&raw),
                        "description" => description.push_str(&raw),
                        "link" => link.push_str(&raw),
                        _ => {}
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name().as_ref().to_lowercase();
                if name == "item" {
                    let (title, description, link) = (title.trim(), description.trim(), link.trim());
                    if !title.is_empty() {
                        items.push(RawItem {
                            title: title.to_string(),
                            description: if description.is_empty() { None } else { Some(description.to_string()) },
                            link: if link.is_empty() { None } else { Some(link.to_string()) },
                        });
                    }
                    in_item = false;
                }
                current_tag.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rss_items() {
        let xml = r#"
            <rss version="2.0">
                <channel>
                    <title>Met Office Warnings</title>
                    <item>
                        <title>Yellow warning of wind affecting East of England</title>
                        <description>valid from 1200 to 2300</description>
                        <link>http://example.com/1</link>
                    </item>
                </channel>
            </rss>
        "#;
        let items = parse_rss_items(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Yellow warning of wind affecting East of England");
        assert_eq!(items[0].description.as_deref(), Some("valid from 1200 to 2300"));
        assert_eq!(items[0].link.as_deref(), Some("http://example.com/1"));
    }

    #[test]
    fn test_parse_weather_warnings() {
        let xml = r#"
            <rss version="2.0">
                <channel>
                    <item>
                        <title>Yellow warning of wind affecting East of England</title>
                        <description>valid from 12:00 on Mon until 23:00 on Tue. Some other description...</description>
                        <link>http://example.com/1</link>
                    </item>
                    <item>
                        <title>Amber warning of rain affecting Wales</title>
                        <description><![CDATA[valid from 15:00 on Mon to 18:00 on Mon]]></description>
                    </item>
                </channel>
            </rss>
        "#;
        let warnings = parse_weather_warnings(xml);
        assert_eq!(warnings.len(), 2);

        assert_eq!(warnings[0].level, "YELLOW");
        assert_eq!(warnings[0].type_str, "wind");
        assert_eq!(warnings[0].region, "East of England");
        assert_eq!(warnings[0].valid_text.as_deref(), Some("valid from 12:00 on Mon until 23:00 on Tue"));

        assert_eq!(warnings[1].level, "AMBER");
        assert_eq!(warnings[1].type_str, "rain");
        assert_eq!(warnings[1].region, "Wales");
        assert_eq!(warnings[1].valid_text.as_deref(), Some("valid from 15:00 on Mon to 18:00 on Mon"));
    }

    // Same vector as MetOfficeWarningParserTest's `entities in a warning are kept`.
    #[test]
    fn entities_in_a_warning_are_kept() {
        let xml = r#"<rss version="2.0"><channel><item>
            <title>Yellow warning of wind affecting Highlands &amp; Eilean Siar</title>
            <description>Gusts at Loch &#193;ird. Valid from 0600 Sat to 2100 Sat</description>
            <link>https://example.com/w?a=1&amp;b=2</link>
        </item></channel></rss>"#;
        let items = parse_rss_items(xml);
        assert_eq!(items[0].title, "Yellow warning of wind affecting Highlands & Eilean Siar");
        assert_eq!(items[0].description.as_deref(), Some("Gusts at Loch \u{c1}ird. Valid from 0600 Sat to 2100 Sat"));
        assert_eq!(items[0].link.as_deref(), Some("https://example.com/w?a=1&b=2"));
    }
}


/// The five predefined entities and decimal/hex character references; anything else (an undefined
/// entity, an invalid code point) is None and is kept as written. Same rule as rust/xmltv-parser.
fn resolve_reference(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix('x') {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Forces the linker to include this crate's object file when linked as an rlib.
pub fn _force_link() {
}
