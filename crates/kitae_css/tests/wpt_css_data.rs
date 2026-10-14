//! WPT css-parsing-tests data conformance (subset bundled under tests/data/):
//! `An+B.json` (an-plus-b grammar) and the css-color-3 hexadecimal/keyword
//! color corpora. Each corpus is a flat JSON array of alternating
//! input/expected pairs; color expectations are the canonical `rgb()` /
//! `rgba()` serializations (or null when the input must be rejected) and
//! An+B expectations are `[a, b]` or null.
//!
//! Like the HTML harnesses this measures conformance and records the rate in
//! docs/WPT_SCORECARD.md; each test fails if the measured rate drops below
//! its baseline constant. `KITAE_CSS_DIFF=1` prints every divergence.

use kitae_css::{parse_color, parse_nth};
use std::fs;
use std::path::Path;

/// Recorded in docs/WPT_SCORECARD.md. Each test fails when its measured
/// rate drops below the baseline. Measured 2026-09-25: hexadecimal 81/81,
/// keyword 160/160, An+B 128/128.
const COLOR_HEX_BASELINE: f64 = 1.000;
const COLOR_KEYWORD_BASELINE: f64 = 1.000;
const AN_PLUS_B_BASELINE: f64 = 1.000;

// ---- Minimal JSON reader (flat arrays of string | [int, int] | null) ----

#[derive(Debug, PartialEq, Clone)]
enum JVal {
    Str(String),
    Nums(Option<(i32, i32)>),
    Null,
}

fn parse_array(s: &str) -> Option<Vec<JVal>> {
    let b = s.as_bytes();
    let mut i = 0;
    let ws = |b: &[u8], i: &mut usize| {
        while *i < b.len() && (b[*i] as char).is_ascii_whitespace() {
            *i += 1;
        }
    };
    ws(b, &mut i);
    if b.get(i) != Some(&b'[') {
        return None;
    }
    i += 1;
    let mut out = vec![];
    loop {
        ws(b, &mut i);
        match b.get(i)? {
            b']' => {
                i += 1;
                break;
            }
            b'"' => {
                i += 1;
                let mut v = String::new();
                loop {
                    match b.get(i)? {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            i += 1;
                            match b.get(i)? {
                                b'"' => v.push('"'),
                                b'\\' => v.push('\\'),
                                b'/' => v.push('/'),
                                b'n' => v.push('\n'),
                                b'r' => v.push('\r'),
                                b't' => v.push('\t'),
                                b'b' => v.push('\u{8}'),
                                b'f' => v.push('\u{c}'),
                                b'u' => {
                                    let hex = s.get(i + 1..i + 5)?;
                                    let cp = u32::from_str_radix(hex, 16).ok()?;
                                    i += 4;
                                    // Lone surrogates cannot appear in this
                                    // corpus; decode astral pairs anyway.
                                    if (0xD800..0xDC00).contains(&cp)
                                        && s[i + 1..].starts_with("\\u")
                                    {
                                        let hex2 = s.get(i + 3..i + 7)?;
                                        let lo = u32::from_str_radix(hex2, 16).ok()?;
                                        if (0xDC00..0xE000).contains(&lo) {
                                            i += 6;
                                            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            v.push(char::from_u32(c)?);
                                        }
                                    }
                                    v.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                }
                                _ => return None,
                            }
                            i += 1;
                        }
                        _ => {
                            let c = s[i..].chars().next()?;
                            v.push(c);
                            i += c.len_utf8();
                        }
                    }
                }
                out.push(JVal::Str(v));
            }
            b'n' => {
                if !s[i..].starts_with("null") {
                    return None;
                }
                i += 4;
                out.push(JVal::Null);
            }
            b'[' => {
                i += 1;
                ws(b, &mut i);
                let a = s[i..]
                    .trim_start()
                    .split(|c: char| !c.is_ascii_digit() && c != '-');
                let mut nums = vec![];
                for part in a {
                    if part.is_empty() {
                        continue;
                    }
                    nums.push(part.parse::<i32>().ok()?);
                    if nums.len() == 2 {
                        break;
                    }
                }
                if nums.len() != 2 {
                    return None;
                }
                // Position past the closing bracket: find ']' from i.
                i += s[i..].find(']')? + 1;
                out.push(JVal::Nums(Some((nums[0], nums[1]))));
            }
            _ => return None,
        }
        ws(b, &mut i);
        match b.get(i)? {
            b',' => i += 1,
            b']' => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    Some(out)
}

fn load(name: &str) -> Vec<JVal> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing corpus {}: {e}", path.display()));
    parse_array(&raw).unwrap_or_else(|| panic!("unparsable corpus {}", path.display()))
}

/// Canonical serialization of kitae's packed-RGBA color: opaque colors use
/// the level-3 `rgb(r, g, b)` form, anything else `rgba(r, g, b, a)`.
fn fmt_color(c: u32) -> String {
    let (r, g, b, a) = (c >> 24, (c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF);
    if a == 255 {
        format!("rgb({}, {}, {})", r, g, b)
    } else {
        format!("rgba({}, {}, {}, {})", r, g, b, a)
    }
}

/// Independent canonical parser for the corpus's `rgb()`/`rgba()` strings.
fn expected_color(s: &str) -> Option<u32> {
    let inner = s
        .strip_prefix("rgba(")
        .or_else(|| s.strip_prefix("rgb("))?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(", ").collect();
    if parts.len() != 3 && parts.len() != 4 {
        return None;
    }
    let mut n = |p: &str| -> Option<u32> {
        if p.contains('.') {
            return None; // fractional channels never occur in this corpus
        }
        p.parse::<u32>().ok()
    };
    let r = n(parts[0])?;
    let g = n(parts[1])?;
    let b = n(parts[2])?;
    let a = if parts.len() == 4 { n(parts[3])? } else { 255 };
    Some((r << 24) | (g << 16) | (b << 8) | a)
}

fn run_color_corpus(name: &str, baseline: f64, label: &str) {
    let data = load(name);
    assert!(
        data.len() % 2 == 0,
        "{}: expected input/expected pairs",
        name
    );
    let diff = std::env::var("KITAE_CSS_DIFF").is_ok();
    let (mut pass, mut fail) = (0usize, 0usize);
    for pair in data.chunks(2) {
        let JVal::Str(input) = &pair[0] else {
            panic!("{}: non-string input {:?}", name, pair[0]);
        };
        let want = match &pair[1] {
            JVal::Str(expected) => expected_color(expected),
            // null: the corpus requires the input to be rejected.
            JVal::Null => None,
            other => panic!("{}: non-color expectation {:?}", name, other),
        };
        let got = parse_color(input);
        if got == want {
            pass += 1;
        } else {
            fail += 1;
            if diff {
                println!(
                    "[{}] in {:?}\n  wpt:   {:?}\n  kitae: {:?}",
                    name,
                    input,
                    want.map(fmt_color),
                    got.map(fmt_color)
                );
            }
        }
    }
    let rate = pass as f64 / (pass + fail) as f64;
    println!(
        "WPT {} color: {}/{} pass ({:.1}%)",
        label,
        pass,
        pass + fail,
        rate * 100.0
    );
    assert!(
        rate >= baseline,
        "{} conformance {:.3} fell below recorded baseline {:.3} — see docs/WPT_SCORECARD.md; \
         if this drop is intentional, re-measure and update the baseline",
        label,
        rate,
        baseline
    );
}

#[test]
fn color_hexadecimal() {
    run_color_corpus(
        "color_hexadecimal_3.json",
        COLOR_HEX_BASELINE,
        "hexadecimal",
    );
}

#[test]
fn color_keywords() {
    run_color_corpus("color_keywords_3.json", COLOR_KEYWORD_BASELINE, "keyword");
}

#[test]
fn an_plus_b() {
    let data = load("An+B.json");
    assert!(data.len() % 2 == 0, "An+B: expected input/expected pairs");
    let diff = std::env::var("KITAE_CSS_DIFF").is_ok();
    let (mut pass, mut fail) = (0usize, 0usize);
    for pair in data.chunks(2) {
        let (JVal::Str(input), want) = (&pair[0], &pair[1]) else {
            panic!("An+B: non-string input {:?}", pair);
        };
        let want = match want {
            JVal::Nums(n) => *n,
            JVal::Null => None,
            other => panic!("An+B: unexpected expectation {:?}", other),
        };
        let got = parse_nth(input);
        if got == want {
            pass += 1;
        } else {
            fail += 1;
            if diff {
                println!(
                    "[An+B] in {:?}\n  wpt:   {:?}\n  kitae: {:?}",
                    input, want, got
                );
            }
        }
    }
    let rate = pass as f64 / (pass + fail) as f64;
    println!(
        "WPT An+B: {}/{} pass ({:.1}%)",
        pass,
        pass + fail,
        rate * 100.0
    );
    assert!(
        rate >= AN_PLUS_B_BASELINE,
        "An+B conformance {:.3} fell below recorded baseline {:.3} — see docs/WPT_SCORECARD.md",
        rate,
        AN_PLUS_B_BASELINE
    );
}
