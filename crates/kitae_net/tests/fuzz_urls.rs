//! I4 fuzzing program — URL resolution and fetch policy (also the I6
//! "URL-parse parity" deliverable: the properties below pin the engine's
//! URL handling to the `url` crate's behavior, the same parser real
//! browsers share, so drift between them is a test failure).
//!
//! Invariants:
//!
//! 1. `resolve` never panics on any (base, href) pair, however hostile.
//! 2. When `resolve` succeeds for a non-`about:`/`kitae:` href, the result
//!    is an absolute URL that itself parses, and it matches
//!    `Url::parse(base).join(href)` — the parity property.
//! 3. `mixed_content_allowed` never panics, allows everything except the
//!    https-page → plaintext-loopback-free-subresource edge, and is
//!    consistent with the parsed schemes it inspects.
//!
//! ```sh
//! KITAE_FUZZ_ITERS=20000 cargo test -p kitae_net --test fuzz_urls
//! ```

/// The iteration budget per test. Override with `KITAE_FUZZ_ITERS`.
fn iters() -> usize {
    std::env::var("KITAE_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

/// xorshift64*: deterministic, dependency-free.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Bases: scheme zoo plus malformed shapes (empty, spaces, backslashes,
/// bare hosts, bad ports, userinfo tricks).
const BASES: [&str; 16] = [
    "https://example.com/a/b/page.html",
    "http://example.com/",
    "file:///Users/dev/page.html",
    "https://example.com:8443/x",
    "https://user:pass@example.com/secret",
    "https://[2001:db8::1]/v6",
    "https://example.com/a b/c?q=1&r=2#frag",
    "https://example.com/\\..\\..\\up",
    "//protocol.relative/x",
    "not a url at all",
    "",
    "https://",
    "https://example.com:port/x",
    "https://exa_mple.com/x",
    "https://example.com:0/x",
    "HTTP://EXAMPLE.COM/UPPER",
];

/// Hrefs: relative shapes, scheme-relative, absolute schemes, dot storms,
/// percent soup, nulls, whitespace, hostile override attempts.
const HREFS: [&str; 22] = [
    "./other.html",
    "../up.html",
    "/root.html",
    "?query=1",
    "#section",
    "//other.example/proto-rel",
    "https://abs.example/x",
    "http://abs.example/x",
    "data:image/png;base64,aGk=",
    "about:blank",
    "kitae:settings",
    "javascript:alert(1)",
    "file:///etc/passwd",
    "../../../..//../../etc/passwd",
    "%2e%2e%2f%2e%2e%2f",
    "https://example.com/%zz%2",
    " a/b ",
    "\t\n/x",
    "http://[::1]:9999/x",
    "http://127.0.0.1:8080/x",
    "http://localhost/x",
    "ht\0tp://split/x",
];

/// Hostile characters for byte-level mutation of the pairs above.
const HOSTILE: [char; 16] = [
    ':', '/', '\\', '?', '#', '%', '@', '[', ']', ' ', '\t', '\n', '\u{0}', '.', ':', '\\',
];

const CONTENT_TYPES: [&str; 12] = [
    "text/html",
    "application/xhtml+xml",
    "text/plain",
    "image/png",
    "application/x-qtsq",
    "text/html; charset=utf-8",
    "text/plain; charset=iso-8859-1",
    "text/plain; charset=utf-16",
    "text/plain; charset=windows-1252",
    "application/octet-stream",
    "not a content type",
    "",
];

const BODY_SEEDS: &[&[u8]] = &[
    b"",
    b"hello",
    b"\xef\xbb\xbfhello",
    b"\xff\xfeh\x00i\x00",
    b"\xfe\xff\x00h\x00i",
    b"\xff\xfe\xfd",
    b"QTS\x01\x03",
    b"<html>\0\xff",
    b"text",
];

fn mutate_bytes(rng: &mut XorShift, bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    match rng.below(4) {
        0 => {
            if !out.is_empty() {
                let at = rng.below(out.len());
                out[at] ^= rng.next() as u8;
            }
        }
        1 => {
            if !out.is_empty() {
                let at = rng.below(out.len());
                out.remove(at);
            }
        }
        2 => {
            let at = rng.below(out.len() + 1);
            out.insert(at, rng.next() as u8);
        }
        _ => out.push(rng.next() as u8),
    }
    out
}

fn mutate(rng: &mut XorShift, s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    match rng.below(3) {
        0 => {
            let at = rng.below(chars.len() + 1);
            chars.insert(at, HOSTILE[rng.below(HOSTILE.len())]);
        }
        1 => {
            if !chars.is_empty() {
                let at = rng.below(chars.len());
                chars.remove(at);
            }
        }
        _ => {
            if !chars.is_empty() {
                let at = rng.below(chars.len());
                chars[at] = HOSTILE[rng.below(HOSTILE.len())];
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn resolve_never_panics_and_matches_url_crate_semantics() {
    let mut rng = XorShift::new(0x55524C31); // "URL1"
    for _ in 0..iters() {
        let mut base = BASES[rng.below(BASES.len())].to_string();
        let mut href = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            base = mutate(&mut rng, &base);
        }
        if rng.below(2) == 0 {
            href = mutate(&mut rng, &href);
        }

        // Invariant 1: no panic for any pair.
        let resolved = kitae_net::resolve(&base, &href);

        // Invariant 2: parity with the `url` crate for ordinary hrefs.
        // about:/kitae: pass through verbatim; empty hrefs are None.
        let trimmed = href.trim();
        if let Some(out) = &resolved {
            if !trimmed.starts_with("about:")
                && !trimmed.starts_with("kitae:")
                && !trimmed.is_empty()
            {
                let reparsed = url::Url::parse(out);
                assert!(
                    reparsed.is_ok(),
                    "resolve({base:?}, {href:?}) = {out:?} does not reparse"
                );
                let expected = url::Url::parse(&base)
                    .ok()
                    .and_then(|b| b.join(trimmed).ok());
                assert_eq!(
                    Some(out.as_str()),
                    expected.as_ref().map(|u| u.as_str()),
                    "resolve({base:?}, {href:?}) diverged from Url::join"
                );
            }
        }
    }
}

#[test]
fn about_and_kitae_pass_through_verbatim() {
    let mut rng = XorShift::new(0x50415353); // "PASS"
    for _ in 0..iters() {
        // Force a passthrough prefix, then mutate the rest of the string.
        let href = if rng.below(2) == 0 {
            format!("about:{}", mutate(&mut rng, "blank/test"))
        } else {
            format!("kitae:{}", mutate(&mut rng, "settings/page"))
        };
        // resolve's contract: trim the href, then pass about:/kitae:
        // through verbatim (no joining, no normalization of the interior).
        let expected = href.trim();
        for base in BASES {
            assert_eq!(
                kitae_net::resolve(base, &href).as_deref(),
                Some(expected),
                "passthrough broken for base {base:?} href {href:?}"
            );
        }
    }
}

#[test]
fn response_and_origin_helpers_never_panic() {
    let mut rng = XorShift::new(0x52455350);
    for _ in 0..iters() {
        let mut content_type = CONTENT_TYPES[rng.below(CONTENT_TYPES.len())].to_string();
        if rng.below(2) == 0 {
            content_type = mutate(&mut rng, &content_type);
        }
        let mut body = BODY_SEEDS[rng.below(BODY_SEEDS.len())].to_vec();
        if rng.below(2) == 0 {
            body = mutate_bytes(&mut rng, &body);
        }
        let mut final_url = BASES[rng.below(BASES.len())].to_string();
        if rng.below(2) == 0 {
            final_url = mutate(&mut rng, &final_url);
        }
        let mut cache_control = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            cache_control = mutate(&mut rng, &cache_control);
        }
        let mut etag = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            etag = mutate(&mut rng, &etag);
        }
        let mut last_modified = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            last_modified = mutate(&mut rng, &last_modified);
        }
        let mut protocol = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            protocol = mutate(&mut rng, &protocol);
        }
        let mut content_disposition = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            content_disposition = mutate(&mut rng, &content_disposition);
        }
        let mut www_authenticate = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            www_authenticate = mutate(&mut rng, &www_authenticate);
        }
        let response = kitae_net::Response {
            final_url,
            status: rng.next() as u16,
            body,
            content_type,
            cache_control,
            etag,
            last_modified,
            protocol,
            content_disposition,
            www_authenticate,
        };
        let _ = response.is_html();
        let _ = response.is_plain_text();
        let _ = response.is_image();
        let _ = response.is_qtsq();
        let _ = response.text();

        let mut origin = BASES[rng.below(BASES.len())].to_string();
        if rng.below(2) == 0 {
            origin = mutate(&mut rng, &origin);
        }
        let _ = kitae_net::local_origin_allowed(&origin);
        kitae_net::set_page_origin(&origin);
        let _ = kitae_net::page_origin();
    }
    kitae_net::set_page_origin("");
}

#[test]
fn mixed_content_policy_is_scheme_exact() {
    let mut rng = XorShift::new(0x4D495831); // "MIX1"
    for _ in 0..iters() {
        let mut base = BASES[rng.below(BASES.len())].to_string();
        let mut href = HREFS[rng.below(HREFS.len())].to_string();
        if rng.below(2) == 0 {
            base = mutate(&mut rng, &base);
        }
        if rng.below(2) == 0 {
            href = mutate(&mut rng, &href);
        }
        // Invariant: no panic, and the verdict is a pure function of the
        // two parsed schemes plus the subresource host — never of parsing
        // side effects (call twice, same answer).
        let a = kitae_net::mixed_content_allowed(&base, &href);
        let b = kitae_net::mixed_content_allowed(&base, &href);
        assert_eq!(
            a, b,
            "policy is not a pure function of ({base:?}, {href:?})"
        );

        // Anything that is not (https page, http subresource) allows.
        let page_scheme = url::Url::parse(base.trim())
            .ok()
            .map(|u| u.scheme().to_string());
        let sub_scheme = url::Url::parse(href.trim())
            .ok()
            .map(|u| u.scheme().to_string());
        if page_scheme.as_deref() != Some("https") || sub_scheme.as_deref() != Some("http") {
            assert!(
                a,
                "policy tightened beyond the https→http edge: ({base:?}, {href:?})"
            );
        }
    }
}

#[test]
fn plaintext_subresources_are_blocked_except_loopback() {
    // The exhaustive truth table at the heart of the policy.
    assert!(!kitae_net::mixed_content_allowed(
        "https://example.com/",
        "http://example.com/x"
    ));
    assert!(!kitae_net::mixed_content_allowed(
        "https://example.com/",
        "http://192.168.0.9/x"
    ));
    for sub in [
        "http://localhost/x",
        "http://127.0.0.1/x",
        "http://[::1]/x",
        "http://a.localhost/x",
    ] {
        assert!(
            kitae_net::mixed_content_allowed("https://example.com/", sub),
            "loopback {sub} must stay exempt"
        );
    }
    for base in ["http://example.com/", "file:///p.html", "", "garbage"] {
        assert!(
            kitae_net::mixed_content_allowed(base, "http://example.com/x"),
            "non-https page {base:?} has no https→http edge"
        );
    }
}
