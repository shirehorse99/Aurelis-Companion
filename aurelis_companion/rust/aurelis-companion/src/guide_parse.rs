//! The parsed-guide artefact: a guide's programmes as the shared Rust XMLTV parser reads them,
//! stored gzip'd beside the guide body so a television can take them instead of parsing XML.
//!
//! Format (gzip of UTF-8 lines): `MAGIC`; a header `{"logic","sourceId","bodySha256"}`; one
//! `{"icon":{"channelId","url"}}` per channel icon (sorted by channel id, so the file is
//! deterministic); one line per programme (the parser's `EntryOut` JSON, untouched); and a trailer
//! `{"end":{"entries":N,"icons":M}}`. A file without the trailer, or whose counts differ, is
//! incomplete, and `read_parsed` says so.

#[cfg(test)]
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const MAGIC: &str = "AURELIS-PARSED-GUIDE/1";

/// The least free memory a parse is attempted with; a big guide needs six times its own size.
pub const MIN_PARSE_FREE_MB: u64 = 1024;

/// Whether a guide body of `body_bytes` can be parsed with `free_ram_mb` free: it needs
/// `max(1024, 6 x the body's MB)`. The `Err` names both figures.
pub fn parse_budget_ok(body_bytes: u64, free_ram_mb: u64) -> Result<(), String> {
    let body_mb = body_bytes.div_ceil(1024 * 1024);
    let need = MIN_PARSE_FREE_MB.max(body_mb.saturating_mul(6));
    if free_ram_mb >= need {
        Ok(())
    } else {
        Err(format!("needs {need} MB free, {free_ram_mb} MB free"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedSummary {
    pub logic: String,
    pub source_id: String,
    pub body_sha256: String,
    pub entries: u64,
    pub icons: u64,
    pub parse_ms: u64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn emit(w: &mut impl Write, v: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *w, v).map_err(|e| e.to_string())?;
    w.write_all(b"\n").map_err(|e| e.to_string())
}

/// Parses the stored guide body and writes the artefact to `out.tmp`, then renames it over `out`.
/// A body with no programmes is an `Err` and nothing is written.
pub fn write_parsed(body: &Path, source_id: &str, out: &Path) -> Result<ParsedSummary, String> {
    write_parsed_hooked(body, source_id, out, &|| {})
}

/// The SHA-256 of a file, in hex.
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(hex(&h.finalize()))
}

/// `write_parsed`, calling `after_read` once the body has been read and before anything is
/// written (a test pauses there to let another body land mid-parse).
pub fn write_parsed_hooked(body: &Path, source_id: &str, out: &Path, after_read: &dyn Fn()) -> Result<ParsedSummary, String> {
    let bytes = std::fs::read(body).map_err(|e| format!("cannot read the guide body: {e}"))?;
    let xml = String::from_utf8_lossy(&bytes);
    let sha = hex(&Sha256::digest(&bytes));
    after_read();
    let started = Instant::now();
    let parsed = xmltv_parser::parse_xmltv(&xml, source_id);
    let parse_ms = started.elapsed().as_millis() as u64;
    if parsed.entries.is_empty() {
        return Err("the guide parsed to no programmes".to_string());
    }
    let mut icons: Vec<(&String, &String)> = parsed.channel_icons.iter().collect();
    icons.sort();

    let mut tmp_name = out.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let written = (|| -> Result<(), String> {
        let f = std::fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        let mut w = BufWriter::new(GzEncoder::new(f, Compression::default()));
        writeln!(w, "{MAGIC}").map_err(|e| e.to_string())?;
        emit(&mut w, &json!({"logic": xmltv_parser::LOGIC_VERSION, "sourceId": source_id, "bodySha256": sha}))?;
        for (id, url) in &icons {
            emit(&mut w, &json!({"icon": {"channelId": id, "url": url}}))?;
        }
        for e in &parsed.entries {
            emit(&mut w, &serde_json::to_value(e).map_err(|e| e.to_string())?)?;
        }
        emit(&mut w, &json!({"end": {"entries": parsed.entries.len(), "icons": icons.len()}}))?;
        let gz = w.into_inner().map_err(|e| e.to_string())?;
        gz.finish().map_err(|e| e.to_string())?.sync_all().map_err(|e| e.to_string())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write the parsed guide: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, out) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot move the parsed guide into place: {e}"));
    }
    Ok(ParsedSummary {
        logic: xmltv_parser::LOGIC_VERSION.to_string(),
        source_id: source_id.to_string(),
        body_sha256: sha,
        entries: parsed.entries.len() as u64,
        icons: icons.len() as u64,
        parse_ms,
    })
}

/// Reads an artefact back: the summary and every icon and programme record in file order.
/// Fails, naming why, when the file is not one of ours or was cut short.
#[cfg(test)]
pub fn read_parsed(path: &Path) -> Result<(ParsedSummary, Vec<Value>), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("cannot open the parsed guide: {e}"))?;
    let mut lines = BufReader::new(GzDecoder::new(f)).lines();
    let cut = |e: std::io::Error| format!("the parsed guide is cut short ({e})");
    match lines.next() {
        Some(Ok(l)) if l == MAGIC => {}
        Some(Ok(_)) => return Err("not a parsed guide (wrong magic line)".to_string()),
        Some(Err(e)) => return Err(cut(e)),
        None => return Err("the parsed guide is cut short (empty)".to_string()),
    }
    let header: Value = match lines.next() {
        Some(Ok(l)) => serde_json::from_str(&l).map_err(|e| format!("bad parsed-guide header: {e}"))?,
        Some(Err(e)) => return Err(cut(e)),
        None => return Err("the parsed guide is cut short (no header)".to_string()),
    };
    let text = |k: &str| header[k].as_str().map(str::to_string).ok_or_else(|| format!("parsed-guide header lacks {k}"));
    let (logic, source_id, body_sha256) = (text("logic")?, text("sourceId")?, text("bodySha256")?);
    let (mut records, mut entries, mut icons) = (Vec::new(), 0u64, 0u64);
    for l in lines {
        let l = l.map_err(cut)?;
        let v: Value = serde_json::from_str(&l).map_err(|e| format!("the parsed guide is cut short (bad record: {e})"))?;
        if let Some(end) = v.get("end") {
            let (we, wi) = (end["entries"].as_u64(), end["icons"].as_u64());
            if we != Some(entries) || wi != Some(icons) {
                return Err(format!("the parsed guide is cut short (trailer says {we:?} programmes, {wi:?} icons; read {entries}, {icons})"));
            }
            return Ok((ParsedSummary { logic, source_id, body_sha256, entries, icons, parse_ms: 0 }, records));
        }
        if v.get("icon").is_some() {
            icons += 1;
        } else {
            entries += 1;
        }
        records.push(v);
    }
    Err("the parsed guide is cut short (no trailer)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::household::TempDir;
    use std::io::Write;

    const XML: &str = r#"<tv>
      <channel id="bbc1"><icon src="http://logo.test/bbc1.png"/></channel>
      <channel id="itv1"><icon src="http://logo.test/itv1.png"/></channel>
      <programme start="20260101120000 +0000" stop="20260101130000 +0000" channel="bbc1"><title>One</title></programme>
      <programme start="20260101130000 +0000" stop="20260101140000 +0000" channel="bbc1"><title>Two</title><desc>d</desc></programme>
      <programme start="20260101120000 +0000" stop="20260101133000 +0000" channel="itv1"><title>Three</title></programme>
    </tv>"#;

    fn written(xml: &str) -> (TempDir, std::path::PathBuf, Result<ParsedSummary, String>) {
        let d = TempDir::new();
        let body = d.0.join("body.xml");
        std::fs::File::create(&body).unwrap().write_all(xml.as_bytes()).unwrap();
        let out = d.0.join("body.parsed");
        let r = write_parsed(&body, "src1", &out);
        (d, out, r)
    }

    #[test]
    fn writes_every_programme_and_icon() {
        let (_d, out, r) = written(XML);
        let s = r.unwrap();
        assert_eq!((s.entries, s.icons), (3, 2));
        let (h, recs) = read_parsed(&out).unwrap();
        assert_eq!(recs.len(), 5);
        assert_eq!(h.logic, xmltv_parser::LOGIC_VERSION);
        assert_eq!(h.source_id, "src1");
        assert_eq!(recs[0]["icon"]["channelId"], "bbc1");
        assert_eq!(recs[1]["icon"]["channelId"], "itv1");
        assert_eq!(recs[2]["title"], "One");
        assert_eq!(recs[4]["title"], "Three");
    }

    #[test]
    fn same_records_as_the_parser() {
        let (_d, out, r) = written(XML);
        r.unwrap();
        let (_, recs) = read_parsed(&out).unwrap();
        let parsed = xmltv_parser::parse_xmltv(XML, "src1");
        for (i, e) in parsed.entries.iter().enumerate() {
            assert_eq!(recs[2 + i], serde_json::to_value(e).unwrap());
        }
    }

    #[test]
    fn trailer_counts_every_record() {
        let (_d, out, r) = written(XML);
        r.unwrap();
        let bytes = std::fs::read(&out).unwrap();
        std::fs::write(&out, &bytes[..bytes.len() - 40]).unwrap();
        let e = read_parsed(&out).unwrap_err();
        assert!(e.contains("cut short"), "{e}");
    }

    #[test]
    fn not_a_guide_says_so() {
        let (_d, out, r) = written("<html>");
        assert!(r.unwrap_err().contains("no programmes"));
        assert!(!out.exists());
    }

    #[test]
    fn too_big_to_parse_says_why() {
        let e = parse_budget_ok(2_000_000_000, 4_000).unwrap_err();
        assert!(e.contains("4000 MB free") && e.contains("11448 MB free"), "{e}");
        assert!(parse_budget_ok(2_000_000_000, 12_000).is_ok());
        // A small guide still needs the floor.
        assert!(parse_budget_ok(1000, 1023).unwrap_err().contains("1024"));
        assert!(parse_budget_ok(1000, 1024).is_ok());
    }
}
