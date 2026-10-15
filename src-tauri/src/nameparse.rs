//! Torrent / file name parsing and match confidence (WP6).
//!
//! `parse("The.Matrix.1999.1080p.BluRay.x264-GRP")` -> title "The Matrix", year 1999.
//! Deterministic, offline, unit-tested against a table of real-world messy names.
//! `confidence()` decides whether a search result may be auto-accepted
//! (prefer no match over a wrong match).

use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

macro_rules! re {
    ($p:expr) => {{
        static R: OnceLock<Regex> = OnceLock::new();
        R.get_or_init(|| Regex::new($p).expect("valid regex"))
    }};
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Parsed {
    pub title: String,
    /// The other side of "AKA".
    pub alt_title: Option<String>,
    pub year: Option<u16>,
    pub season: Option<u32>,
    /// Season packs: S01-S05, "Seasons 1-3".
    pub season_end: Option<u32>,
    pub episode: Option<u32>,
    /// Multi-episode: S01E01E02, S01E01-E03.
    pub episode_end: Option<u32>,
    /// "Part 2" (kept in the title).
    pub part: Option<u32>,
    /// Daily shows: YYYY-MM-DD.
    pub air_date: Option<String>,
    /// Disambiguation hint: "(US)", trailing "US"/"UK".
    pub country: Option<String>,
    pub complete: bool,
    /// `[Group] Title - 05` style (absolute episode numbers).
    pub anime: bool,
    /// Trilogy / "1-8 Collection" / box set: no single match.
    pub collection: bool,
    /// "Complete Series" etc.
    pub tv_hint: bool,
    /// Breakdown of what was detected/stripped (shown in tests / match notes).
    pub notes: Vec<String>,
}

impl Parsed {
    pub fn is_tv(&self) -> bool {
        self.season.is_some()
            || self.episode.is_some()
            || self.air_date.is_some()
            || self.anime
            || self.tv_hint
    }
}

// ---------------------------------------------------------------- time

/// Days since 1970-01-01 -> (year, month, day). (Howard Hinnant's civil_from_days.)
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn max_year() -> u16 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, _, _) = civil_from_days((secs / 86_400) as i64);
    (y as u16).max(2025) + 1
}

fn as_year(t: &str) -> Option<u16> {
    if t.len() != 4 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: u16 = t.parse().ok()?;
    (1900..=max_year()).contains(&y).then_some(y)
}

// ---------------------------------------------------------------- normalisation

/// Full-width ASCII, CJK brackets and fancy dashes/quotes -> plain ASCII.
fn fold_width(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF08}' => '(',
            '\u{FF09}' => ')',
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            '【' | '「' | '『' | '〔' | '〖' | '〘' => '[',
            '】' | '」' | '』' | '〕' | '〗' | '〙' => ']',
            '–' | '—' | '‐' | '‑' | '−' => '-',
            '’' | '‘' | '`' | '´' => '\'',
            _ => c,
        })
        .collect()
}

const COUNTRIES: &[&str] = &["US", "UK", "AU", "CA", "NZ", "IE"];

const STRONG_JUNK: &str = r"(?x)^(?:
    \d{3,4}[pi] | [48]k | uhd | fhd | qhd | hdr(?:10\+?|10plus)? | dovi | sdr | 1[02]bit | 8bit |
    blu-?ray | bdrip | brrip | bdremux | bd25 | bd50 | remux | web-?dl | webrip | web-?rip | webdl |
    hdtv | pdtv | sdtv | dsr | dvdrip | dvdscr | dvd[r59]? | hdrip | hdcam | camrip | hdts | telesync | telecine | screener |
    [xh]\.?26[45] | hevc | avc | av1 | xvid | divx | vp9 |
    aac(?:\d(?:\.\d)?)? | e?ac3 | dts(?:-?hd(?:-?ma)?|-?x|-?es)? | truehd | atmos | ddp?\d(?:\.\d)? | ddp | dd\+ | flac | lpcm |
    remastered | rerip | repack | hardsub | multisubs? | dualaudio |
    dublaj | dublajl[ıi] | altyaz[ıi] | altyaz[ıi]l[ıi] | t[üu]rk[çc]e | trdublaj |
    nf | amzn | dsnp | hmax | atvp | pcok | pmtp | itunes |
    rarbg | yts | yify | eztv | ettv | tgx | [26]ch | 8ch
)$";

/// Ambiguous words: only a cut point when the next token is junk too.
const WEAK_JUNK: &str = r"(?x)^(?:
    web | dv | hd | sd | tr | cam | ts | tc | r5 | hq | real | it | dc | limited | internal | proper |
    multi | dual | uncut | uncensored | theatrical | directors | director's | dubbed | subbed | sub | subs |
    dub | esubs? | criterion | extended | unrated | imax | ita | eng | fre | fra | ger | deu | spa | esp |
    rus | jpn | kor | hin | tur | en | hc
)$";

fn is_strong(t: &str) -> bool {
    let r = re!(STRONG_JUNK);
    r.is_match(t) || t.split_once('-').is_some_and(|(a, _)| !a.is_empty() && r.is_match(a))
}
fn is_weak(t: &str) -> bool {
    re!(WEAK_JUNK).is_match(t)
}
fn is_junk(t: &str) -> bool {
    is_strong(t) || is_weak(t)
}

fn num(t: &str) -> Option<u32> {
    (!t.is_empty() && t.len() <= 4 && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse().ok())
        .flatten()
}

/// "1-3" -> (1, 3)
fn range(t: &str) -> Option<(u32, u32)> {
    let c = re!(r"^(\d{1,2})-(\d{1,2})$").captures(t)?;
    let (a, b) = (c[1].parse().ok()?, c[2].parse().ok()?);
    (a < b).then_some((a, b))
}

fn looks_like_site(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("www") || re!(r"(?i)\.(com|org|net|to|me|cc|tv|io|ru|info|xyz|ws|se|lt|mx|nz)\b").is_match(&l)
}

const COLLECTION_WORDS: &[&str] = &[
    "collection", "trilogy", "duology", "quadrilogy", "pentalogy", "hexalogy", "saga",
    "anthology", "boxset",
];

// ---------------------------------------------------------------- parse

pub fn parse(raw: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut s = fold_width(raw.trim());

    // 1. Known file extension.
    let ext = re!(r"(?i)\.(mkv|mp4|m4v|avi|mov|wmv|webm|ts|m2ts|mpg|mpeg|flv|vob|ogv|3gp|iso|torrent|nfo|srt)$");
    if ext.is_match(&s) {
        s = ext.replace(&s, "").into_owned();
    }

    // 2. Site prefixes: "www.Torrenting.com - Title", "Site.org - Title".
    for r in [
        re!(r"(?i)^\s*www\.\S+?\.[a-z]{2,6}\s*-\s*"),
        re!(r"(?i)^\s*[a-z0-9-]+\.(?:com|org|net)\s+-\s+"),
    ] {
        if r.is_match(&s) {
            s = r.replace(&s, "").into_owned();
            p.notes.push("site prefix".into());
        }
    }

    // 3. Bracket groups: read a year / country first, then drop the group.
    let mut leading_group = false;
    let mut bracket_text = String::new();
    {
        let br = re!(r"[\[\(\{]([^\[\]\(\)\{\}]*)[\]\)\}]");
        let mut out = String::new();
        let mut last = 0;
        for m in br.captures_iter(&s) {
            let whole = m.get(0).unwrap();
            let inner = m[1].trim();
            out.push_str(&s[last..whole.start()]);
            last = whole.end();
            bracket_text.push_str(inner);
            bracket_text.push(' ');
            if out.trim().is_empty() && whole.start() == s[..whole.start()].len() && !looks_like_site(inner) && !inner.is_empty() {
                leading_group = true;
            }
            if let Some(y) = as_year(inner) {
                out.push_str(&format!(" {y} "));
            } else if COUNTRIES.contains(&inner) {
                p.country = Some(inner.to_string());
                out.push(' ');
            } else {
                out.push(' ');
            }
        }
        out.push_str(&s[last..]);
        if out.trim().is_empty() {
            out = bracket_text.clone();
        } else if out.len() != s.len() || last > 0 {
            p.notes.push("brackets stripped".into());
        }
        s = out;
    }

    // 4. Separators.
    let s: String = s
        .chars()
        .map(|c| match c {
            '.' | '_' | '[' | ']' | '(' | ')' | '{' | '}' | '|' => ' ',
            _ => c,
        })
        .collect();
    let toks: Vec<&str> = s.split_whitespace().collect();
    if toks.is_empty() {
        return p;
    }
    let low: Vec<String> = toks.iter().map(|t| t.to_lowercase()).collect();
    let n = toks.len();

    let mut cut: Option<usize> = None;
    let mut years: Vec<(usize, u16)> = Vec::new();
    let mut aka: Option<usize> = None;
    let set_cut = |cut: &mut Option<usize>, i: usize| {
        if cut.is_none() {
            *cut = Some(i);
        }
    };
    let sxe = re!(r"^s(\d{1,2})[ ._-]?e(\d{1,3})(?:-?e(\d{1,3})|-(\d{1,3}))*$");
    let s_only = re!(r"^s(\d{1,2})$");
    let s_range = re!(r"^s(\d{1,2})-s?(\d{1,2})$");
    let e_only = re!(r"^e(?:p)?(\d{1,3})$");
    let nxn = re!(r"^(\d{1,2})x(\d{2,3})$");
    let date = re!(r"^((?:19|20)\d\d)-(\d{2})-(\d{2})$");
    let anime_ep = re!(r"^(\d{1,4})(?:v\d)?$");

    let mut i = 0;
    while i < n {
        let t = low[i].as_str();
        let next = low.get(i + 1).map(|x| x.as_str());

        if t == "aka" && i > 0 && cut.is_none() && aka.is_none() {
            aka = Some(i);
            i += 1;
            continue;
        }
        if let Some(c) = sxe.captures(t) {
            p.season = p.season.or(c[1].parse().ok());
            p.episode = p.episode.or(c[2].parse().ok());
            if let Some(e) = c.get(3).or(c.get(4)) {
                p.episode_end = e.as_str().parse().ok();
            }
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if let Some(c) = s_range.captures(t) {
            p.season = c[1].parse().ok();
            p.season_end = c[2].parse().ok();
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if let Some(c) = s_only.captures(t) {
            p.season = p.season.or(c[1].parse().ok());
            set_cut(&mut cut, i);
            // "S01 E05"
            if let Some(c2) = next.and_then(|x| e_only.captures(x)) {
                p.episode = c2[1].parse().ok();
                i += 1;
            }
            i += 1;
            continue;
        }
        if let Some(c) = nxn.captures(t) {
            p.season = p.season.or(c[1].parse().ok());
            p.episode = p.episode.or(c[2].parse().ok());
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if matches!(t, "season" | "seasons" | "sezon" | "saison" | "temporada" | "staffel") {
            if let Some(nx) = next {
                if let Some((a, b)) = range(nx) {
                    p.season = Some(a);
                    p.season_end = Some(b);
                } else if let Some(v) = num(nx).filter(|v| *v < 100) {
                    p.season = Some(v);
                } else {
                    i += 1;
                    continue;
                }
                set_cut(&mut cut, i);
                i += 2;
                // "Season 1 Episode 5" / "Sezon 1 Bölüm 5"
                if let (Some(k), Some(v)) = (low.get(i), low.get(i + 1).and_then(|x| num(x))) {
                    if matches!(k.as_str(), "episode" | "ep" | "bölüm" | "bolum" | "folge") {
                        p.episode = Some(v);
                        i += 2;
                    }
                }
                continue;
            }
        }
        if i > 0 && matches!(t, "episode" | "bölüm" | "bolum") {
            if let Some(v) = next.and_then(num) {
                p.episode = p.episode.or(Some(v));
                set_cut(&mut cut, i);
                i += 2;
                continue;
            }
        }
        if i > 0 {
            if let Some(c) = e_only.captures(t) {
                p.episode = p.episode.or(c[1].parse().ok());
                set_cut(&mut cut, i);
                i += 1;
                continue;
            }
        }
        if i > 0 && t == "complete" {
            p.complete = true;
            if matches!(next, Some("series" | "serie" | "show")) {
                p.tv_hint = true;
            }
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if i > 0 && (COLLECTION_WORDS.contains(&t) || (t == "box" && next == Some("set"))) {
            p.collection = true;
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if i > 0 && cut.is_none() && range(t).is_some() {
            // "Harry Potter 1-8 Collection"
            p.collection = true;
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if let Some(c) = date.captures(t) {
            p.air_date = Some(format!("{}-{}-{}", &c[1], &c[2], &c[3]));
            set_cut(&mut cut, i);
            i += 1;
            continue;
        }
        if let Some(y) = as_year(t) {
            // Daily show: 2024 10 15
            let m = low.get(i + 1).and_then(|x| num(x)).filter(|v| (1..=12).contains(v) && low[i + 1].len() == 2);
            let d = low.get(i + 2).and_then(|x| num(x)).filter(|v| (1..=31).contains(v) && low[i + 2].len() == 2);
            if i > 0 && m.is_some() && d.is_some() {
                p.air_date = Some(format!("{y}-{:02}-{:02}", m.unwrap(), d.unwrap()));
                set_cut(&mut cut, i);
                i += 3;
                continue;
            }
            years.push((i, y));
            i += 1;
            continue;
        }
        if t == "-" && leading_group && i > 0 && cut.is_none() {
            if let Some(c) = next.and_then(|x| anime_ep.captures(x)) {
                p.anime = true;
                p.episode = c[1].parse().ok();
                set_cut(&mut cut, i);
                i += 2;
                continue;
            }
        }
        if matches!(t, "part" | "pt") {
            if let Some(v) = next.and_then(num).filter(|v| *v < 20) {
                p.part = Some(v);
            }
        }
        if i > 0 && is_strong(t) {
            set_cut(&mut cut, i);
        } else if i > 0 && is_weak(t) && next.is_some_and(is_junk) {
            set_cut(&mut cut, i);
        }
        i += 1;
    }

    // Year: the last plausible year before the first marker, never at position 0.
    let limit = cut.unwrap_or(n);
    let mut year_pos = None;
    if let Some(&(pos, y)) = years.iter().rev().find(|(pos, _)| *pos > 0 && *pos < limit) {
        p.year = Some(y);
        year_pos = Some(pos);
        p.notes.push(format!("year {y}"));
    }

    // "Show.Name.105.HDTV": 3-digit SEE only when there is no year and it follows the title.
    if p.year.is_none() && p.season.is_none() && p.episode.is_none() && !p.anime {
        if let Some(pos) = (1..limit).find(|&k| {
            re!(r"^[1-9](?:0[1-9]|[12]\d|30)$").is_match(&low[k]) && (k + 1 == limit || k + 1 == n)
        }) {
            p.season = low[pos][..1].parse().ok();
            p.episode = low[pos][1..].parse().ok();
            cut = Some(pos);
            p.notes.push("3-digit episode".into());
        }
    }

    let end = match (cut, year_pos) {
        (Some(c), Some(y)) => c.min(y),
        (Some(c), None) => c,
        (None, Some(y)) => y,
        (None, None) => n,
    };
    let join = |a: usize, b: usize| -> String {
        let t = toks[a..b].join(" ");
        t.trim_matches(|c: char| c == '-' || c == ':' || c == ',' || c == '+' || c.is_whitespace())
            .to_string()
    };
    match aka {
        Some(k) if k < end => {
            p.title = join(0, k);
            let alt = join(k + 1, end);
            if !alt.is_empty() {
                p.alt_title = Some(alt);
            }
        }
        _ => p.title = join(0, end),
    }
    if p.title.is_empty() && cut.is_none() && year_pos.is_none() {
        p.title = toks.join(" ");
    }
    if p.country.is_none() {
        let words: Vec<&str> = p.title.split(' ').collect();
        if words.len() >= 2 && COUNTRIES.contains(words.last().unwrap()) {
            p.country = Some(words.last().unwrap().to_string());
        }
    }
    if p.is_tv() {
        p.notes.push("tv markers".into());
    }
    if p.collection {
        p.notes.push("collection".into());
    }
    p
}

// ---------------------------------------------------------------- files

/// Sample / extras / trailers: never the main video, never looked up.
pub fn is_extra(path: &str) -> bool {
    let l = path.to_lowercase();
    l.split('/').any(|c| {
        matches!(
            c,
            "sample" | "samples" | "extras" | "extra" | "featurettes" | "featurette" | "trailers"
                | "trailer" | "behind the scenes" | "deleted scenes" | "bonus" | "specials-extras"
        )
    }) || re!(r"(?i)(^|[^a-z])(sample|trailer|featurette)([^a-z]|$)").is_match(l.rsplit('/').next().unwrap_or(&l))
}

// ---------------------------------------------------------------- similarity

fn fold_diacritic(c: char) -> &'static str {
    match c {
        'ç' | 'č' | 'ć' => "c",
        'ğ' => "g",
        'ı' | 'í' | 'ì' | 'î' | 'ï' | 'ī' => "i",
        'ö' | 'ó' | 'ò' | 'ô' | 'õ' | 'ø' | 'ō' => "o",
        'ş' | 'š' | 'ś' => "s",
        'ü' | 'ú' | 'ù' | 'û' | 'ū' => "u",
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ā' | 'ą' => "a",
        'é' | 'è' | 'ê' | 'ë' | 'ē' | 'ę' => "e",
        'ñ' | 'ń' => "n",
        'ý' | 'ÿ' => "y",
        'ž' | 'ź' | 'ż' => "z",
        'ř' => "r",
        'ł' => "l",
        'đ' => "d",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => "",
    }
}

const NUM_WORDS: &[&str] = &[
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve",
];

/// Lowercase, strip diacritics and punctuation, "&" -> "and", small numbers -> words,
/// drop a leading "the"/"a"/"an".
pub fn norm_title(s: &str) -> String {
    let mut out = String::new();
    for c in fold_width(s).to_lowercase().chars() {
        let f = fold_diacritic(c);
        if !f.is_empty() {
            out.push_str(f);
        } else if c == '&' {
            out.push_str(" and ");
        } else if c == '\'' || ('\u{0300}'..='\u{036F}').contains(&c) {
            // "Ocean's" -> "oceans"; combining marks ("İ".to_lowercase() = "i\u{307}")
        } else if c.is_alphanumeric() {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    let mut words: Vec<String> = out
        .split_whitespace()
        .map(|w| match w.parse::<usize>() {
            Ok(v) if v < NUM_WORDS.len() && !w.starts_with('0') || w == "0" => NUM_WORDS[v.min(12)].to_string(),
            _ => w.to_string(),
        })
        .collect();
    if words.len() > 1 && matches!(words[0].as_str(), "the" | "a" | "an") {
        words.remove(0);
    }
    words.join(" ")
}

/// 0..1: max of normalised Levenshtein on the strings and on their sorted tokens.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (na, nb) = (norm_title(a), norm_title(b));
    if na.is_empty() || nb.is_empty() {
        return 0.0;
    }
    if na == nb {
        return 1.0;
    }
    let sorted = |s: &str| {
        let mut v: Vec<&str> = s.split(' ').collect();
        v.sort_unstable();
        v.join(" ")
    };
    strsim::normalized_levenshtein(&na, &nb).max(strsim::normalized_levenshtein(&sorted(&na), &sorted(&nb)))
}

// ---------------------------------------------------------------- confidence

pub const SIM_WITH_YEAR: f64 = 0.85;
pub const SIM_NO_YEAR: f64 = 0.95;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Confidence {
    pub similarity: f64,
    /// None when the name has no year.
    pub year_ok: Option<bool>,
    /// Another candidate is just as strong (remakes, US/UK versions).
    pub ambiguous: bool,
    pub accepted: bool,
    pub reason: String,
}

/// Auto-accept only if similarity >= 0.85 and the year matches (±1), or
/// similarity >= 0.95 with no year known and no equally strong rival.
/// TV: a show's start year often differs from the season's year, so an exact unique
/// title also passes with a mismatched year.
pub fn confidence(
    similarity: f64,
    query_year: Option<u16>,
    cand_year: Option<u16>,
    strong_rivals: usize,
    tv: bool,
) -> Confidence {
    let year_ok = query_year.map(|q| cand_year.is_some_and(|c| (c as i32 - q as i32).abs() <= 1));
    let ambiguous = strong_rivals > 0;
    let sim = (similarity * 100.0).round() / 100.0;
    let (accepted, reason) = if year_ok == Some(true) && similarity >= SIM_WITH_YEAR {
        (true, format!("similarity {sim:.2}, year matches"))
    } else if similarity >= SIM_NO_YEAR && !ambiguous && (year_ok.is_none() || tv) {
        (true, format!("similarity {sim:.2}, unique title"))
    } else if ambiguous && similarity >= SIM_NO_YEAR {
        (false, format!("similarity {sim:.2}, but {} other equally strong result(s)", strong_rivals))
    } else if year_ok == Some(false) {
        let y = cand_year.map(|y| y.to_string()).unwrap_or_else(|| "?".into());
        (false, format!("similarity {sim:.2}; year {} vs {y}", query_year.unwrap_or(0)))
    } else {
        (false, format!("similarity {sim:.2} too low"))
    };
    Confidence { similarity, year_ok, ambiguous, accepted, reason }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    type Row = (&'static str, &'static str, Option<u16>, Option<u32>, Option<u32>);

    #[test]
    fn table() {
        let rows: &[Row] = &[
            ("The.Matrix.1999.1080p.BluRay.x264-GRP", "The Matrix", Some(1999), None, None),
            ("Show.Name.S02E05.720p.WEB", "Show Name", None, Some(2), Some(5)),
            ("Show Name Season 3 Complete", "Show Name", None, Some(3), None),
            ("Sintel.2010.2160p.mkv", "Sintel", Some(2010), None, None),
            ("Big Buck Bunny (2008) 1080p", "Big Buck Bunny", Some(2008), None, None),
            ("Blade.Runner.2049.2017.1080p", "Blade Runner 2049", Some(2017), None, None),
            ("1917.2019.2160p.UHD", "1917", Some(2019), None, None),
            ("The Office US S05 1080p", "The Office US", None, Some(5), None),
            ("Breaking.Bad.5x14.HDTV", "Breaking Bad", None, Some(5), Some(14)),
            ("The Daily Podcast – Season 3 Complete", "The Daily Podcast", None, Some(3), None),
            ("[SubsPlease] Frieren - 05 (1080p) [ABCD1234].mkv", "Frieren", None, None, Some(5)),
            ("The.Matrix.1999.REMASTERED.1080p.BluRay.x264.DTS-HD.MA.5.1-FGT", "The Matrix", Some(1999), None, None),
            ("[YTS.MX] Inception (2010) [1080p]", "Inception", Some(2010), None, None),
            ("Breaking.Bad.S03E07.720p.HDTV.x264-CTU", "Breaking Bad", None, Some(3), Some(7)),
            ("Yüzüklerin.Efendisi.Kralın.Dönüşü.2003.TR.Dublaj", "Yüzüklerin Efendisi Kralın Dönüşü", Some(2003), None, None),
            ("www.Torrenting.com - Interstellar.2014.1080p.BluRay", "Interstellar", Some(2014), None, None),
            ("[ www.site.org ] Dune.2021.2160p.WEB-DL", "Dune", Some(2021), None, None),
            ("【site】Parasite.2019.1080p", "Parasite", Some(2019), None, None),
            ("Show.Name.S01E01.720p.HDTV.x264[eztv]", "Show Name", None, Some(1), Some(1)),
            ("Movie.Name.2012.1080p.BluRay[rarbg]", "Movie Name", Some(2012), None, None),
            ("Anime Show (Hi10) - S01E02", "Anime Show", None, Some(1), Some(2)),
            ("2001.A.Space.Odyssey.1968.1080p", "2001 A Space Odyssey", Some(1968), None, None),
            ("Blade Runner 2049 (2017)", "Blade Runner 2049", Some(2017), None, None),
            ("2012", "2012", None, None, None),
            ("2012 (2009) 1080p", "2012", Some(2009), None, None),
            ("Ocean's.Eleven.2001.720p.BluRay", "Ocean's Eleven", Some(2001), None, None),
            ("Se7en.1995.1080p", "Se7en", Some(1995), None, None),
            ("21.Jump.Street.2012.1080p", "21 Jump Street", Some(2012), None, None),
            ("9-1-1.S05E03.1080p.WEB", "9-1-1", None, Some(5), Some(3)),
            ("Fantastic.4.2015.1080p", "Fantastic 4", Some(2015), None, None),
            ("Rocky.II.1979.1080p", "Rocky II", Some(1979), None, None),
            ("Law.&.Order.S01E01.720p", "Law & Order", None, Some(1), Some(1)),
            ("Show.S01E01E02.720p", "Show", None, Some(1), Some(1)),
            ("Show.S01E01-E03.1080p", "Show", None, Some(1), Some(1)),
            ("Show.Name.105.HDTV.x264", "Show Name", None, Some(1), Some(5)),
            ("Show Name Season 1 Episode 5", "Show Name", None, Some(1), Some(5)),
            ("Dizi Adı Sezon 1 Bölüm 5", "Dizi Adı", None, Some(1), Some(5)),
            ("Show.Name.Ep05.720p", "Show Name", None, None, Some(5)),
            ("Show.Name.E05.720p", "Show Name", None, None, Some(5)),
            ("Show.Name.S01-S05.1080p", "Show Name", None, Some(1), None),
            ("Show.Name.S01.COMPLETE.1080p", "Show Name", None, Some(1), None),
            ("Friends.Complete.Series.1080p.BluRay", "Friends", None, None, None),
            ("Show Name Seasons 1-3", "Show Name", None, Some(1), None),
            ("The.Daily.Show.2024.10.15.Guest.Name.720p.WEB", "The Daily Show", None, None, None),
            ("[Erai-raws] Some Title - 12v2 [1080p]", "Some Title", None, None, Some(12)),
            ("Doctor.Who.(2005).S01E01.720p", "Doctor Who", Some(2005), Some(1), Some(1)),
            ("House of Cards (US) S01 1080p", "House of Cards", None, Some(1), None),
            ("Amelie.AKA.Le.Fabuleux.Destin.d.Amelie.Poulain.2001.1080p", "Amelie", Some(2001), None, None),
            ("The.Lord.of.the.Rings.Trilogy.1080p", "The Lord of the Rings", None, None, None),
            ("Harry.Potter.1-8.Collection.1080p", "Harry Potter", None, None, None),
            ("THE.LION.KING.1994.1080p", "THE LION KING", Some(1994), None, None),
            ("Charlotte's.Web.2006.1080p", "Charlotte's Web", Some(2006), None, None),
            ("Movie.Name.WEB-DL.x264", "Movie Name", None, None, None),
            ("君の名は。.2016.1080p.BluRay", "君の名は。", Some(2016), None, None),
            ("Брат.1997.DVDRip", "Брат", Some(1997), None, None),
            ("Room.237.2012.1080p", "Room 237", Some(2012), None, None),
            ("Movie.Name.2010.MULTi.1080p", "Movie Name", Some(2010), None, None),
            ("Harry.Potter.and.the.Deathly.Hallows.Part.2.2011.1080p", "Harry Potter and the Deathly Hallows Part 2", Some(2011), None, None),
            ("ＦＵＬＬ ＷＩＤＴＨ ２０１９", "FULL WIDTH", Some(2019), None, None),
        ];
        assert!(rows.len() >= 25);
        for (input, title, year, s, e) in rows {
            let p = parse(input);
            assert_eq!(
                (p.title.as_str(), p.year, p.season, p.episode),
                (*title, *year, *s, *e),
                "input {input:?} -> {p:?}"
            );
        }
    }

    #[test]
    fn flags() {
        let p = parse("The Office US S05 1080p");
        assert_eq!(p.country.as_deref(), Some("US"));
        assert!(p.is_tv());
        let p = parse("House of Cards (US) S01");
        assert_eq!(p.country.as_deref(), Some("US"));
        let p = parse("[SubsPlease] Frieren - 05 (1080p) [ABCD1234].mkv");
        assert!(p.anime && p.is_tv());
        let p = parse("Show.S01E01E02.720p");
        assert_eq!(p.episode_end, Some(2));
        let p = parse("Show.S01E01-E03.1080p");
        assert_eq!(p.episode_end, Some(3));
        let p = parse("Show.Name.S01-S05.1080p");
        assert_eq!((p.season, p.season_end), (Some(1), Some(5)));
        let p = parse("Show Name Seasons 1-3");
        assert_eq!((p.season, p.season_end), (Some(1), Some(3)));
        let p = parse("Friends.Complete.Series.1080p.BluRay");
        assert!(p.complete && p.tv_hint && p.is_tv());
        let p = parse("Show.Name.S01.COMPLETE.1080p");
        assert!(p.complete);
        let p = parse("The.Daily.Show.2024.10.15.Guest.Name.720p.WEB");
        assert_eq!(p.air_date.as_deref(), Some("2024-10-15"));
        assert!(p.is_tv());
        let p = parse("The.Daily.Show.2024-10-15.720p");
        assert_eq!(p.air_date.as_deref(), Some("2024-10-15"));
        let p = parse("Amelie.AKA.Le.Fabuleux.Destin.d.Amelie.Poulain.2001.1080p");
        assert_eq!(p.alt_title.as_deref(), Some("Le Fabuleux Destin d Amelie Poulain"));
        assert!(parse("The.Lord.of.the.Rings.Trilogy.1080p").collection);
        assert!(parse("Harry.Potter.1-8.Collection.1080p").collection);
        assert!(!parse("9-1-1.S05E03").collection);
        assert_eq!(parse("Mini.Series.Part.2.720p.HDTV").part, Some(2));
        assert_eq!(parse("Mini.Series.Part.2.720p.HDTV").title, "Mini Series Part 2");
        assert!(!parse("The.Matrix.1999.1080p").is_tv());
        // Must not panic / produce anything weird.
        let p = parse("ubuntu-24.04.1-desktop-amd64.iso");
        assert!(!p.title.is_empty());
        assert_eq!(parse("").title, "");
        assert_eq!(parse("S01E05.mkv").episode, Some(5));
        assert_eq!(parse("[OnlyBrackets]").title, "OnlyBrackets");
    }

    #[test]
    fn extras() {
        assert!(is_extra("Movie/Sample/movie-sample.mkv"));
        assert!(is_extra("Movie/movie.sample.mkv"));
        assert!(is_extra("Movie/Featurettes/Making Of.mkv"));
        assert!(is_extra("Movie/Movie.Trailer.mp4"));
        assert!(is_extra("Movie/Extras/x.mkv"));
        assert!(!is_extra("Movie/Movie.2010.1080p.mkv"));
        assert!(!is_extra("Sampler.mkv"));
    }

    #[test]
    fn normalisation_and_similarity() {
        assert_eq!(norm_title("Yüzüklerin Efendisi: Kralın Dönüşü"), "yuzuklerin efendisi kralin donusu");
        assert_eq!(norm_title("The Matrix"), "matrix");
        assert_eq!(norm_title("Law & Order"), "law and order");
        assert_eq!(norm_title("Ocean's Eleven"), "oceans eleven");
        assert_eq!(norm_title("Fantastic 4"), "fantastic four");
        assert_eq!(norm_title("ＴＨＥ　ＭＡＴＲＩＸ"), "matrix");
        assert_eq!(similarity("Fantastic 4", "Fantastic Four"), 1.0);
        assert_eq!(similarity("YÜZÜKLERİN EFENDİSİ", "yuzuklerin efendisi"), 1.0);
        assert!(similarity("The Office US", "The Office") < SIM_WITH_YEAR);
        assert!(similarity("Big Buck Bunny", "Big Hero 6") < 0.6);
        assert!(similarity("Spider Man No Way Home", "Spider-Man: No Way Home") >= SIM_NO_YEAR);
        assert!(similarity("Lord of the Rings Return of the King", "The Lord of the Rings: The Return of the King") >= SIM_WITH_YEAR);
    }

    #[test]
    fn confidence_rules() {
        // Exact + year.
        assert!(confidence(1.0, Some(1999), Some(1999), 0, false).accepted);
        // Year ±1 is fine, ±2 is not.
        assert!(confidence(0.9, Some(2010), Some(2011), 0, false).accepted);
        let c = confidence(0.9, Some(2010), Some(2012), 0, false);
        assert!(!c.accepted && c.year_ok == Some(false));
        // Below the year threshold.
        assert!(!confidence(0.8, Some(2010), Some(2010), 0, false).accepted);
        // No year: needs 0.95 and uniqueness.
        assert!(confidence(1.0, None, Some(2008), 0, true).accepted);
        assert!(!confidence(0.9, None, Some(2008), 0, false).accepted);
        let c = confidence(1.0, None, Some(1994), 1, false);
        assert!(!c.accepted && c.ambiguous, "{c:?}");
        // Remakes: the year decides even with a rival.
        assert!(confidence(1.0, Some(2019), Some(2019), 1, false).accepted);
        // Movie with exact title but wrong year: rejected; TV: accepted when unique.
        assert!(!confidence(1.0, Some(2015), Some(2005), 0, false).accepted);
        assert!(confidence(1.0, Some(2015), Some(2005), 0, true).accepted);
    }

    #[test]
    fn civil() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_376), (2025, 10, 15));
    }
}
