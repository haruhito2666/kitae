//! F5: WebVTT caption parsing. The engine fetches `<track kind=captions>`
//! sources alongside their video, parses cues here, and paints the active
//! cue as an overlay at stamp time. Only the cue-text subset of the spec is
//! handled — voice/colour styling is a recorded deferral.

/// One caption: `start_ms..end_ms` with the tag-stripped text (newlines
/// preserved for multi-line cues).
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start_ms: f64,
    pub end_ms: f64,
    pub text: String,
}

/// Parse a WebVTT document into cues, in file order. Malformed blocks are
/// skipped; the header line is optional (be liberal — some sites omit it).
pub fn parse_vtt(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if !line.contains("-->") {
            continue; // header, NOTE blocks, cue ids, blank lines
        }
        let (start_s, end_s) = match parse_timing(line) {
            Some(t) => t,
            None => continue,
        };
        // Cue text: every line until a blank line.
        let mut body: Vec<String> = Vec::new();
        for l in lines.by_ref() {
            if l.trim().is_empty() {
                break;
            }
            body.push(strip_tags(l));
        }
        let joined = body.join("\n");
        let text = joined.trim().to_string();
        if !text.is_empty() {
            cues.push(Cue {
                start_ms: start_s * 1000.0,
                end_ms: end_s * 1000.0,
                text,
            });
        }
    }
    cues
}

/// `00:00:01.250 --> 00:00:03.000 align:center` (any trailing settings).
fn parse_timing(line: &str) -> Option<(f64, f64)> {
    let mut parts = line.split("-->");
    let start = parse_ts(parts.next()?.trim())?;
    let rest = parts.next()?.trim();
    // Everything up to the first whitespace is the end timestamp.
    let end_tok = rest.split_whitespace().next()?;
    let end = parse_ts(end_tok)?;
    if end < start {
        return None;
    }
    Some((start, end))
}

/// `HH:MM:SS.mmm` or `MM:SS.mmm` (hours optional in the wild).
fn parse_ts(s: &str) -> Option<f64> {
    let (secs, frac) = s.split_once('.')?;
    let mut total = 0.0f64;
    for c in secs.split(':') {
        total = total * 60.0 + c.trim().parse::<f64>().ok()?;
    }
    let digits = frac.len() as i32;
    let v: f64 = frac.parse().ok()?;
    Some(total + v / 10f64.powi(digits))
}

/// Strip cue markup (`<b>`, `<i.v.lang>`, `<00:00:01.000>`) and decode the
/// HTML entities that show up in real caption files.
fn strip_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ => out.push(c),
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_cues() {
        let v = "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHello\n\n00:00:02.000 --> 00:00:03.900\nSecond\nline\n";
        let cues = parse_vtt(v);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 0.0);
        assert_eq!(cues[0].end_ms, 1500.0);
        assert_eq!(cues[1].text, "Second\nline");
    }

    #[test]
    fn strips_tags_and_entities() {
        let v = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<b>A &amp; B</b>\n";
        let cues = parse_vtt(v);
        assert_eq!(cues[0].text, "A & B");
    }

    #[test]
    fn optional_hours_and_settings() {
        let v = "1\n00:05.250 --> 00:07.000 align:center line:80%\nHi\n";
        let cues = parse_vtt(v);
        assert_eq!(cues.len(), 1);
        assert!((cues[0].start_ms - 5250.0).abs() < 1e-6);
        assert!((cues[0].end_ms - 7000.0).abs() < 1e-6);
    }

    #[test]
    fn skips_notes_and_bad_blocks() {
        let v = "WEBVTT\n\nNOTE a comment\nspanning lines\n\n00:99.000 --> 00:01.000\nbad\n\n00:00:03.000 --> 00:00:04.000\ngood\n";
        let cues = parse_vtt(v);
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "good");
    }
}
