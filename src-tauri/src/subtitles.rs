//! srt/ass -> vtt conversion and subtitle matching (WP3).
//!
//! Pure functions only (no I/O), so everything here is unit-tested below.

use crate::types::{SubtitleFormat, SubtitleTrack};

// ------------------------------------------------------------------ decoding

/// Decode subtitle bytes: UTF-16 (by BOM) or UTF-8; otherwise cp1252/latin1.
/// A leading BOM is removed.
pub fn decode(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && (bytes[..2] == [0xFF, 0xFE] || bytes[..2] == [0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    };
    s.strip_prefix('\u{FEFF}').map(str::to_string).unwrap_or(s)
}

fn cp1252(b: u8) -> char {
    const HI: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    if (0x80..0xA0).contains(&b) {
        char::from_u32(HI[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}')
    } else {
        b as char
    }
}

/// Bytes of any supported format -> WebVTT text.
pub fn to_vtt(bytes: &[u8], format: SubtitleFormat) -> String {
    let text = decode(bytes);
    match format {
        SubtitleFormat::Vtt => text,
        SubtitleFormat::Srt => srt_to_vtt(&text),
        SubtitleFormat::Ass | SubtitleFormat::Ssa => ass_to_vtt(&text),
    }
}

pub fn format_from_name(name: &str) -> Option<SubtitleFormat> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "srt" => SubtitleFormat::Srt,
        "vtt" => SubtitleFormat::Vtt,
        "ass" => SubtitleFormat::Ass,
        "ssa" => SubtitleFormat::Ssa,
        _ => return None,
    })
}

// ------------------------------------------------------------------ SRT

/// Normalize a timestamp to `HH:MM:SS.mmm` (accepts `,` or `.`, 1-digit hours,
/// short fractions, or `MM:SS`).
fn norm_ts(ts: &str) -> String {
    let ts = ts.trim().replace(',', ".");
    let (hms, frac) = match ts.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (ts.clone(), String::new()),
    };
    let parts: Vec<&str> = hms.split(':').collect();
    let (h, m, s) = match parts.as_slice() {
        [h, m, s] => (*h, *m, *s),
        [m, s] => ("0", *m, *s),
        _ => return ts,
    };
    let num = |x: &str| x.trim().parse::<u64>().unwrap_or(0);
    let mut ms: String = frac.chars().filter(|c| c.is_ascii_digit()).take(3).collect();
    while ms.len() < 3 {
        ms.push('0');
    }
    format!("{:02}:{:02}:{:02}.{}", num(h), num(m), num(s), ms)
}

/// `00:00:01,000 --> 00:00:02,000 X1:...` -> VTT timing line.
fn convert_timing(line: &str) -> Option<String> {
    let (a, rest) = line.split_once("-->")?;
    let rest = rest.trim();
    let (b, settings) = match rest.split_once(char::is_whitespace) {
        Some((b, s)) => (b, s.trim()),
        None => (rest, ""),
    };
    // SRT "X1: Y1:" coordinates are not VTT settings; drop them.
    let settings = if settings.contains("X1") { "" } else { settings };
    let mut out = format!("{} --> {}", norm_ts(a), norm_ts(b));
    if !settings.is_empty() {
        out.push(' ');
        out.push_str(settings);
    }
    Some(out)
}

/// SRT-style inline tags -> VTT tags; strips `{\an8}` (returns whether found)
/// and any other `{\...}` override blocks.
fn convert_inline_tags(text: &str) -> (String, bool) {
    let mut top = false;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find('}') else {
            out.push_str(after);
            rest = "";
            break;
        };
        let block = &after[1..end];
        match block {
            "i" | "b" | "u" => out.push_str(&format!("<{block}>")),
            "/i" | "/b" | "/u" => out.push_str(&format!("<{block}>")),
            b if b.starts_with('\\') => {
                let (s, t) = ass_override(b);
                top |= t;
                out.push_str(&s);
            }
            other => {
                // Not a tag: keep literal text.
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    (out, top)
}

/// Convert the inside of an ASS override block (`\i1\an8\pos(1,2)`) to VTT tags.
/// Returns (tags, is_top_aligned).
fn ass_override(block: &str) -> (String, bool) {
    let mut out = String::new();
    let mut top = false;
    for tag in block.split('\\').map(str::trim).filter(|t| !t.is_empty()) {
        for k in ["i", "b", "u"] {
            if let Some(v) = tag.strip_prefix(k) {
                if v.is_empty() || v == "0" {
                    out.push_str(&format!("</{k}>"));
                } else if v.chars().all(|c| c.is_ascii_digit()) {
                    out.push_str(&format!("<{k}>"));
                }
            }
        }
        if let Some(v) = tag.strip_prefix("an") {
            if matches!(v, "7" | "8" | "9") {
                top = true;
            }
        }
    }
    (out, top)
}

pub fn srt_to_vtt(text: &str) -> String {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::from("WEBVTT\n\n");
    let mut block: Vec<&str> = Vec::new();
    let mut cues: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.split('\n').map(str::trim).collect();
    for (i, line) in lines.iter().enumerate() {
        if !line.is_empty() {
            block.push(line);
        }
        if line.is_empty() || i + 1 == lines.len() {
            if let Some(cue) = convert_srt_block(&block) {
                cues.push(cue);
            }
            block.clear();
        }
    }
    out.push_str(&cues.join("\n\n"));
    if !cues.is_empty() {
        out.push('\n');
    }
    out
}

fn convert_srt_block(block: &[&str]) -> Option<String> {
    let ti = block.iter().position(|l| l.contains("-->"))?;
    let mut timing = convert_timing(block[ti])?;
    let mut lines: Vec<String> = Vec::new();
    // Cue number(s) before the timing line can stay as the cue identifier.
    let ident: Vec<&str> = block[..ti].to_vec();
    let mut any_top = false;
    for l in &block[ti + 1..] {
        let (t, top) = convert_inline_tags(l);
        any_top |= top;
        // "-->" is not allowed inside VTT cue text.
        lines.push(t.replace("-->", "->"));
    }
    if any_top {
        timing.push_str(" line:5%");
    }
    let mut cue = String::new();
    if let Some(id) = ident.last() {
        if !id.contains("-->") {
            cue.push_str(id);
            cue.push('\n');
        }
    }
    cue.push_str(&timing);
    for l in lines {
        cue.push('\n');
        cue.push_str(&l);
    }
    Some(cue)
}

// ------------------------------------------------------------------ ASS / SSA

/// `H:MM:SS.cc` -> `HH:MM:SS.mmm` (via norm_ts; centiseconds are right-padded).
fn ass_ts(ts: &str) -> String {
    norm_ts(ts)
}

fn ass_text(raw: &str) -> (String, bool) {
    let (mut s, top) = convert_inline_tags(raw);
    s = s.replace("\\N", "\n").replace("\\n", "\n").replace("\\h", " ");
    let s = s
        .split('\n')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .replace("-->", "->");
    (s.trim().to_string(), top)
}

pub fn ass_to_vtt(text: &str) -> String {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let default_fmt = ["layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text"];
    let mut fields: Vec<String> = default_fmt.iter().map(|s| s.to_string()).collect();
    let mut in_events = false;
    let mut cues: Vec<(String, String, String)> = Vec::new(); // (start, end, text)
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = line.strip_prefix("Format:") {
            fields = f.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
            continue;
        }
        let Some(d) = line.strip_prefix("Dialogue:") else { continue };
        let vals: Vec<&str> = d.trim_start().splitn(fields.len(), ',').collect();
        if vals.len() < fields.len() {
            continue;
        }
        let get = |name: &str| fields.iter().position(|f| f == name).map(|i| vals[i]);
        let (Some(start), Some(end)) = (get("start"), get("end")) else { continue };
        let raw = vals[fields.len() - 1];
        if raw.contains("\\p1") {
            continue; // vector drawing, not text
        }
        let (mut t, top) = ass_text(raw);
        if t.is_empty() {
            continue;
        }
        let mut timing = format!("{} --> {}", ass_ts(start), ass_ts(end));
        if top {
            timing.push_str(" line:5%");
        }
        t = t.replace("\n\n", "\n");
        cues.push((ass_ts(start), timing, t));
    }
    cues.sort_by(|a, b| a.0.cmp(&b.0)); // stable; zero-padded strings sort correctly
    let mut out = String::from("WEBVTT\n\n");
    let body: Vec<String> = cues
        .into_iter()
        .enumerate()
        .map(|(i, (_, timing, t))| format!("{}\n{}\n{}", i + 1, timing, t))
        .collect();
    out.push_str(&body.join("\n\n"));
    if !body.is_empty() {
        out.push('\n');
    }
    out
}

// ------------------------------------------------------------------ matching

/// (code, English name, aliases)
const LANGS: &[(&str, &str, &[&str])] = &[
    ("en", "English", &["eng", "english"]),
    ("tr", "Turkish", &["tur", "turkish", "turkce", "türkçe"]),
    ("es", "Spanish", &["spa", "spanish", "espanol", "español", "latino"]),
    ("fr", "French", &["fre", "fra", "french", "francais", "français"]),
    ("de", "German", &["ger", "deu", "german", "deutsch"]),
    ("it", "Italian", &["ita", "italian", "italiano"]),
    ("pt", "Portuguese", &["por", "portuguese", "português", "brazilian"]),
    ("ru", "Russian", &["rus", "russian"]),
    ("ja", "Japanese", &["jpn", "japanese"]),
    ("ko", "Korean", &["kor", "korean"]),
    ("zh", "Chinese", &["chi", "zho", "chinese", "chs", "cht"]),
    ("ar", "Arabic", &["ara", "arabic"]),
    ("nl", "Dutch", &["dut", "nld", "dutch"]),
    ("sv", "Swedish", &["swe", "swedish"]),
    ("no", "Norwegian", &["nor", "norwegian"]),
    ("da", "Danish", &["dan", "danish"]),
    ("fi", "Finnish", &["fin", "finnish"]),
    ("pl", "Polish", &["pol", "polish"]),
    ("cs", "Czech", &["cze", "ces", "czech"]),
    ("hu", "Hungarian", &["hun", "hungarian"]),
    ("ro", "Romanian", &["rum", "ron", "romanian"]),
    ("el", "Greek", &["gre", "ell", "greek"]),
    ("he", "Hebrew", &["heb", "hebrew"]),
    ("hi", "Hindi", &["hin", "hindi"]),
    ("id", "Indonesian", &["ind", "indonesian"]),
    ("vi", "Vietnamese", &["vie", "vietnamese"]),
    ("th", "Thai", &["tha", "thai"]),
    ("uk", "Ukrainian", &["ukr", "ukrainian"]),
    ("fa", "Persian", &["per", "fas", "persian", "farsi"]),
    ("bg", "Bulgarian", &["bul", "bulgarian"]),
    ("hr", "Croatian", &["hrv", "croatian"]),
    ("sr", "Serbian", &["srp", "serbian"]),
];

fn split_path(p: &str) -> (Vec<String>, String, String) {
    let comps: Vec<String> = p.split('/').filter(|c| !c.is_empty()).map(|c| c.to_lowercase()).collect();
    let (dir, base) = match comps.split_last() {
        Some((b, d)) => (d.to_vec(), b.clone()),
        None => (Vec::new(), String::new()),
    };
    let stem = base.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(base);
    (dir, stem, p.to_string())
}

fn stem_of(p: &str) -> String {
    split_path(p).1
}

/// Language code for a token. Two-letter codes only when `allow_short`.
fn lang_of_token(tok: &str, allow_short: bool) -> Option<&'static str> {
    let t = tok.to_lowercase();
    for (code, name, aliases) in LANGS {
        if (allow_short && t == *code) || aliases.contains(&t.as_str()) || t == name.to_lowercase() {
            return Some(code);
        }
    }
    None
}

/// Guess the language of a subtitle file from its name.
pub fn detect_lang(video_path: &str, sub_path: &str) -> Option<String> {
    let vstem = stem_of(video_path);
    let sstem = stem_of(sub_path);
    // basename.<lang> (e.g. movie.en, movie.eng.forced)
    if let Some(rest) = sstem.strip_prefix(&format!("{vstem}.")) {
        for tok in rest.split(['.', '_', '-', ' ']) {
            if let Some(c) = lang_of_token(tok, true) {
                return Some(c.into());
            }
        }
    }
    // Last dot-component may be a short code: whatever.en
    if let Some((_, last)) = sstem.rsplit_once('.') {
        if let Some(c) = lang_of_token(last, true) {
            return Some(c.into());
        }
    }
    // Any long token (e.g. "2_English", "eng.forced").
    let toks: Vec<&str> = sstem.split(['.', '_', '-', ' ', '[', ']', '(', ')']).collect();
    for tok in toks.iter().rev() {
        if let Some(c) = lang_of_token(tok, false) {
            return Some(c.into());
        }
    }
    // A bare 2-letter stem ("en.srt").
    lang_of_token(&sstem, true).map(Into::into)
}

pub fn lang_name(code: &str) -> Option<&'static str> {
    LANGS.iter().find(|(c, _, _)| *c == code).map(|(_, n, _)| *n)
}

const SUB_DIRS: &[&str] = &["subs", "sub", "subtitles", "subtitle"];

/// Rank a subtitle against a video (lower is better):
/// 0 exact basename, 1 basename.<lang> (or Subs/<basename>/...), 2 same folder,
/// 3 inside a Subs/Subtitles folder, 4 anything else.
pub fn rank(video_path: &str, sub_path: &str) -> u8 {
    let (vdir, vstem, _) = split_path(video_path);
    let (sdir, sstem, _) = split_path(sub_path);
    if sstem == vstem {
        return 0;
    }
    if sstem.starts_with(&format!("{vstem}.")) || sdir.last() == Some(&vstem) {
        return 1;
    }
    if sdir == vdir {
        return 2;
    }
    if sdir.iter().any(|d| SUB_DIRS.contains(&d.as_str())) {
        return 3;
    }
    4
}

/// True if the subtitle clearly belongs to a *different* video (rank 0/1 against it).
fn belongs_to_other(sub_path: &str, video_path: &str, others: &[&str]) -> bool {
    others
        .iter()
        .filter(|o| **o != video_path && stem_of(o) != stem_of(video_path))
        .any(|o| rank(o, sub_path) <= 1)
        && rank(video_path, sub_path) > 1
}

pub fn track_id(key: &str, idx: usize) -> String {
    format!("t:{key}:{idx}")
}

/// Parse "t:<key>:<idx>".
pub fn parse_track_id(id: &str) -> Option<(String, usize)> {
    let rest = id.strip_prefix("t:")?;
    let (key, idx) = rest.rsplit_once(':')?;
    if key.len() != 40 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((key.to_ascii_lowercase(), idx.parse().ok()?))
}

/// Build the ordered track list for `video_path` from the torrent's subtitle
/// files `(index, path)`. `all_videos` lists every video in the torrent so subs
/// that belong to another episode are left out. Returns (tracks, default id).
pub fn build_tracks(
    key: &str,
    video_path: &str,
    all_videos: &[&str],
    subs: &[(usize, &str)],
) -> (Vec<SubtitleTrack>, Option<String>) {
    struct C {
        idx: usize,
        rank: u8,
        lang: Option<String>,
        path: String,
        format: SubtitleFormat,
    }
    let mut cands: Vec<C> = subs
        .iter()
        .filter_map(|(idx, path)| {
            let format = format_from_name(path)?;
            if belongs_to_other(path, video_path, all_videos) {
                return None;
            }
            Some(C {
                idx: *idx,
                rank: rank(video_path, path),
                lang: detect_lang(video_path, path),
                path: path.to_string(),
                format,
            })
        })
        .collect();
    cands.sort_by(|a, b| {
        (a.rank, a.lang.as_deref() != Some("en"), a.path.to_lowercase())
            .cmp(&(b.rank, b.lang.as_deref() != Some("en"), b.path.to_lowercase()))
    });

    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    let tracks: Vec<SubtitleTrack> = cands
        .into_iter()
        .map(|c| {
            let base = c
                .lang
                .as_deref()
                .and_then(lang_name)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    let b = c.path.rsplit('/').next().unwrap_or(&c.path);
                    b.rsplit_once('.').map(|(s, _)| s).unwrap_or(b).to_string()
                });
            let n = seen.entry(base.clone()).or_insert(0);
            *n += 1;
            let label = if *n == 1 { base } else { format!("{base} {n}") };
            SubtitleTrack { id: track_id(key, c.idx), label, lang: c.lang, format: c.format }
        })
        .collect();
    let default = tracks.first().map(|t| t.id.clone());
    (tracks, default)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn srt_basic_bom_and_commas() {
        let srt = "\u{FEFF}1\r\n00:00:01,000 --> 00:00:02,500\r\nHello\r\nWorld\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000\r\nBye\r\n";
        let vtt = srt_to_vtt(srt);
        assert_eq!(
            vtt,
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello\nWorld\n\n2\n00:00:03.000 --> 00:00:04.000\nBye\n"
        );
    }

    #[test]
    fn srt_bom_bytes_via_to_vtt() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\nHi\n");
        let vtt = to_vtt(&bytes, SubtitleFormat::Srt);
        assert!(vtt.starts_with("WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.000\nHi"));
        assert!(!vtt.contains('\u{FEFF}'));
    }

    #[test]
    fn srt_tags() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n{\\i1}italic{\\i0} and {\\b1}bold{\\b} {i}x{/i} {\\u1}u{\\u0}\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("<i>italic</i> and <b>bold</b> <i>x</i> <u>u</u>"), "{vtt}");
    }

    #[test]
    fn srt_an8_top() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n{\\an8}Top text\n\n2\n00:00:03,000 --> 00:00:04,000\nBottom\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000 line:5%\nTop text"), "{vtt}");
        assert!(vtt.contains("00:00:03.000 --> 00:00:04.000\nBottom"));
        assert!(!vtt.contains("an8"));
    }

    #[test]
    fn srt_odd_timestamps_and_extra_blank_lines() {
        let srt = "\n\n1\n0:00:01,5 --> 0:00:02,25\n  padded line  \n\n\n\n2\n00:00:03,000-->00:00:04,000\nx";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("00:00:01.500 --> 00:00:02.250\npadded line"), "{vtt}");
        assert!(vtt.contains("00:00:03.000 --> 00:00:04.000\nx"), "{vtt}");
    }

    #[test]
    fn decode_cp1252_fallback() {
        // "café – ok" in cp1252 (é=0xE9, en dash=0x96)
        let bytes = b"caf\xE9 \x96 ok";
        assert_eq!(decode(bytes), "café – ok");
    }

    #[test]
    fn decode_utf16le() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "Hi ş".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode(&bytes), "Hi ş");
    }

    #[test]
    fn vtt_passthrough() {
        let v = "WEBVTT\n\n00:01.000 --> 00:02.000\nhi\n";
        assert_eq!(to_vtt(v.as_bytes(), SubtitleFormat::Vtt), v);
    }

    #[test]
    fn ass_dialogue() {
        let ass = "[Script Info]\nTitle: x\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:05.50,0:00:07.00,Default,,0,0,0,,Second, with comma\nDialogue: 0,0:00:01.00,0:00:02.20,Default,,0,0,0,,{\\i1}Hello{\\i0}\\Nworld\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,{\\an8\\pos(10,10)}Sign\nComment: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,ignored\nDialogue: 0,0:00:08.00,0:00:09.00,Default,,0,0,0,,{\\p1}m 0 0 l 1 1{\\p0}\n";
        let vtt = ass_to_vtt(ass);
        assert_eq!(
            vtt,
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.200\n<i>Hello</i>\nworld\n\n2\n00:00:03.000 --> 00:00:04.000 line:5%\nSign\n\n3\n00:00:05.500 --> 00:00:07.000\nSecond, with comma\n"
        );
    }

    #[test]
    fn ranking_order() {
        let v = "Movie (2020)/Movie.2020.1080p.mp4";
        assert_eq!(rank(v, "Movie (2020)/Movie.2020.1080p.srt"), 0);
        assert_eq!(rank(v, "Movie (2020)/Movie.2020.1080p.en.srt"), 1);
        assert_eq!(rank(v, "Movie (2020)/other.srt"), 2);
        assert_eq!(rank(v, "Movie (2020)/Subs/English.srt"), 3);
        assert_eq!(rank(v, "Extras/foo.srt"), 4);
        assert_eq!(rank(v, "Movie (2020)/Subs/Movie.2020.1080p/2_English.srt"), 1);
    }

    #[test]
    fn lang_detection() {
        let v = "a/Movie.mp4";
        assert_eq!(detect_lang(v, "a/Movie.en.srt").as_deref(), Some("en"));
        assert_eq!(detect_lang(v, "a/Movie.tr.forced.srt").as_deref(), Some("tr"));
        assert_eq!(detect_lang(v, "a/Subs/2_English.srt").as_deref(), Some("en"));
        assert_eq!(detect_lang(v, "a/Subs/Spanish.srt").as_deref(), Some("es"));
        assert_eq!(detect_lang(v, "a/random.srt"), None);
        // "it" inside a word must not be detected as Italian
        assert_eq!(detect_lang(v, "a/do_it_now.srt"), None);
    }

    #[test]
    fn build_tracks_default_prefers_rank_then_en() {
        let v = "Show/Movie.mp4";
        let subs = [
            (5, "Show/Subs/French.srt"),
            (3, "Show/Movie.tr.srt"),
            (4, "Show/Movie.en.srt"),
            (6, "Show/Subs/English.srt"),
            (7, "Show/Subs/English.srt"),
            (8, "Show/readme.txt"),
        ];
        let (tracks, def) = build_tracks(KEY, v, &[v], &subs);
        let labels: Vec<&str> = tracks.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["English", "Turkish", "English 2", "English 3", "French"]);
        assert_eq!(def, Some(track_id(KEY, 4)));
        assert_eq!(tracks[0].format, SubtitleFormat::Srt);
    }

    #[test]
    fn build_tracks_excludes_other_episodes() {
        let e1 = "S/Show.S01E01.mkv";
        let e2 = "S/Show.S01E02.mkv";
        let subs = [(10, "S/Show.S01E02.srt"), (11, "S/Show.S01E01.srt"), (12, "S/Subs/Show.S01E02/English.srt")];
        let (tracks, def) = build_tracks(KEY, e1, &[e1, e2], &subs);
        assert_eq!(tracks.len(), 1);
        assert_eq!(def, Some(track_id(KEY, 11)));
    }

    #[test]
    fn track_id_roundtrip() {
        let id = track_id(KEY, 42);
        assert_eq!(parse_track_id(&id), Some((KEY.to_string(), 42)));
        assert_eq!(parse_track_id("t:zz:1"), None);
        assert_eq!(parse_track_id("x"), None);
    }
}
