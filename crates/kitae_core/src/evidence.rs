//! Archive-and-verify — Track B's tier B6: QTSQ as evidence.
//!
//! An agent's claims about a page are worth something only if the page can
//! be produced later. This module makes that mechanical: [`archive`] writes
//! the page as a **signed** `.qtsq` plus a manifest of digests, and
//! [`verify`] re-checks every one of them — including a **replay**, where
//! the archived bytes are loaded into a fresh engine and hashed extraction
//! must come back identical.
//!
//! What each digest means (they are deliberately not one number):
//!
//! - `contentSha256` — the archived document bytes. This is what the
//!   signature covers, so "the archive says what it says" is a crypto
//!   claim, not a hash claim.
//! - `replayExtractionSha256` — the digest of extracting the archive
//!   **offline**: same viewport, no scripts, no network. Reproducible on
//!   any machine, which is what makes it a useful regression anchor.
//! - `liveExtractionSha256` — the digest of the extraction of the page as
//!   it stood when archiving (scripts included). Recorded, never replayed:
//!   re-running a page's scripts is not reproducible, and pretending
//!   otherwise would turn a fact into a coin flip.
//!
//! Geometry is excluded from both extraction digests (`rect` keys are
//! dropped): boxes depend on the font stack, so a digest that included them
//! would fail to verify across machines while proving nothing. The full
//! extraction, rects included, is in the manifest as `extraction` when
//! asked for — as recorded observation, not as a checksum.

use std::path::{Path, PathBuf};

use kitae_json::Value;
use kitae_qtsq::{decode_text, probe, verify_signature, SignatureCheck, SIGN_PK_LEN};

use crate::agent::HeadlessHost;
use crate::{ApproxMeasurer, Browser};

/// Manifest format version. Bumped when a field's *meaning* changes.
pub const EVIDENCE_VERSION: &str = "evidence/1";

/// Default directory for archives when a verb does not name one.
pub fn default_dir() -> PathBuf {
    std::env::var_os("KITAE_EVIDENCE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("kitae-evidence"))
}

/// Archive the current page and write its manifest. Returns the manifest
/// document (also written next to the archive as `<stem>.manifest.json`).
pub fn archive(browser: &mut Browser, dir: Option<&str>) -> Result<Value, String> {
    if browser.page_dom().is_none() {
        return Err("no page is open — nothing to archive".to_string());
    }
    let dir = dir.map(PathBuf::from).unwrap_or_else(default_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stem = format!("{}-{stamp}", file_stem_for(&browser.page_url));
    let qtsq_path = dir.join(format!("{stem}.qtsq"));
    let manifest_path = dir.join(format!("{stem}.manifest.json"));

    // The engine's own save path: an ML-DSA-65 signed QTSQ archive.
    let qtsq_path_str = qtsq_path.to_string_lossy().to_string();
    browser.save_page_qtsq(&qtsq_path_str)?;
    let bytes =
        std::fs::read(&qtsq_path).map_err(|e| format!("read {}: {e}", qtsq_path.display()))?;

    let info = probe(&bytes);
    let device_pk = kitae_qtsq::device_sign_pk();
    let signature = match (&info, device_pk) {
        (Some(info), _) if !info.signed => "unsigned".to_string(),
        (_, Some(pk)) => match verify_signature(&bytes, &pk) {
            Ok(SignatureCheck::Valid) => "valid".to_string(),
            Ok(SignatureCheck::Invalid) => "invalid".to_string(),
            Ok(SignatureCheck::Unsigned) => "unsigned".to_string(),
            Err(e) => format!("unverifiable: {e}"),
        },
        _ => "no-device-key".to_string(),
    };

    // The archived document: QTSQ archives here are unencrypted (they are
    // pages, not secrets), so the payload reads back with no password.
    let payload = decode_text(&bytes, None).ok();
    let parsed = payload.as_deref().and_then(crate::qtsq_archive_parts);
    let (base, title, html) = match parsed {
        Some(parts) => parts,
        None => (
            browser.page_url.clone(),
            browser.page_title.clone(),
            String::new(),
        ),
    };

    let viewport = (browser.viewport_size().0, browser.viewport_size().1);
    let content_sha256 = sha256_hex(html.as_bytes());
    let live_extraction = crate::extract::document_json(browser);
    let live_sha256 = sha256_hex(canonical_extraction(&live_extraction).as_bytes());
    // The replay anchor: extract the archive itself, offline.
    let replayed = replay_extraction(&base, &html, viewport);
    let replay_sha256 = replayed
        .as_ref()
        .map(|value| sha256_hex(canonical_extraction(value).as_bytes()));

    let mut manifest = vec![
        ("kitae", Value::s(EVIDENCE_VERSION)),
        ("url", Value::s(browser.page_url.clone())),
        ("archivedUrl", Value::s(base.clone())),
        ("title", Value::s(title.clone())),
        ("archivedAt", Value::Num(stamp as f64)),
        (
            "viewport",
            Value::arr(vec![
                Value::Num(viewport.0 as f64),
                Value::Num(viewport.1 as f64),
            ]),
        ),
        (
            "qtsq",
            Value::s(
                qtsq_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
        ),
        ("qtsqBytes", Value::Num(bytes.len() as f64)),
        ("qtsqSha256", Value::s(sha256_hex(&bytes))),
        ("signature", Value::s(signature)),
    ];
    if let Some(pk) = device_pk {
        manifest.push(("deviceSignPk", Value::s(hex(&pk))));
    }
    manifest.push(("contentSha256", Value::s(content_sha256)));
    manifest.push((
        "replayExtractionSha256",
        Value::s(replay_sha256.clone().unwrap_or_default()),
    ));
    manifest.push(("liveExtractionSha256", Value::s(live_sha256)));
    manifest.push(("scripted", Value::Bool(browser.page_js_running())));
    let manifest = Value::obj(manifest);
    std::fs::write(&manifest_path, manifest.to_json())
        .map_err(|e| format!("write {}: {e}", manifest_path.display()))?;

    let mut out = match manifest {
        Value::Obj(pairs) => pairs,
        other => return Ok(other),
    };
    if replay_sha256.is_none() {
        out.push((
            "error".to_string(),
            Value::s("the archive did not replay to a document".to_string()),
        ));
    }
    out.push(("path".to_string(), Value::s(qtsq_path_str)));
    out.push((
        "manifestPath".to_string(),
        Value::s(manifest_path.to_string_lossy().to_string()),
    ));
    Ok(Value::Obj(out))
}

/// Verify an archive (or a manifest, whose sibling archive it then checks).
/// Every check is reported individually — a single boolean would hide which
/// claim failed, and the whole point of evidence is that the weak link is
/// visible.
pub fn verify(path: &str) -> Result<Value, String> {
    let path = PathBuf::from(path);
    let (qtsq_path, manifest) = if path
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(".manifest.json"))
        .unwrap_or(false)
    {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let manifest = kitae_json::parse(&text)
            .map_err(|e| format!("manifest {} is not JSON: {e}", path.display()))?;
        let name = manifest
            .get("qtsq")
            .and_then(Value::as_str)
            .ok_or_else(|| "manifest names no archive".to_string())?;
        let sibling = path.with_file_name(name);
        (sibling, Some(manifest))
    } else {
        let sibling = path.with_file_name(format!(
            "{}.manifest.json",
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        let manifest = std::fs::read_to_string(&sibling)
            .ok()
            .and_then(|text| kitae_json::parse(&text).ok());
        (path, manifest)
    };

    let bytes =
        std::fs::read(&qtsq_path).map_err(|e| format!("read {}: {e}", qtsq_path.display()))?;
    let info = probe(&bytes);
    let mut checks: Vec<Value> = Vec::new();
    let mut push = |name: &str, ok: bool, detail: String| {
        let mut pairs = vec![("check", Value::s(name)), ("ok", Value::Bool(ok))];
        if !detail.is_empty() {
            pairs.push(("detail", Value::s(detail)));
        }
        checks.push(Value::obj(pairs));
    };

    let is_qtsq = info.is_some();
    push(
        "qtsq",
        is_qtsq,
        match &info {
            Some(i) => format!("strategy {:?}, {} bytes", i.strategy, bytes.len()),
            None => "not a QTSQ payload".to_string(),
        },
    );

    // The signature: against the manifest's recorded key when there is one
    // (that is what proves the archive was not re-signed later), else
    // against this device's key, which is stated in the detail.
    let manifest_pk = manifest
        .as_ref()
        .and_then(|m| m.get("deviceSignPk"))
        .and_then(Value::as_str)
        .and_then(from_hex::<SIGN_PK_LEN>);
    let (pk, pk_source) = match (manifest_pk, kitae_qtsq::device_sign_pk()) {
        (Some(pk), _) => (Some(pk), "the manifest's recorded key"),
        (None, Some(pk)) => (Some(pk), "this device's key"),
        (None, None) => (None, "no key available"),
    };
    let signature = match (info.as_ref().map(|i| i.signed), pk) {
        (Some(false), _) => "unsigned".to_string(),
        (Some(true), Some(pk)) => match verify_signature(&bytes, &pk) {
            Ok(SignatureCheck::Valid) => "valid".to_string(),
            Ok(SignatureCheck::Invalid) => "invalid".to_string(),
            Ok(SignatureCheck::Unsigned) => "unsigned".to_string(),
            Err(e) => format!("unverifiable: {e}"),
        },
        _ => "unknown".to_string(),
    };
    push(
        "signature",
        signature == "valid",
        format!("{signature} (against {pk_source})"),
    );

    let qtsq_sha256 = sha256_hex(&bytes);
    if let Some(expected) = manifest
        .as_ref()
        .and_then(|m| m.get("qtsqSha256"))
        .and_then(Value::as_str)
    {
        push(
            "qtsqSha256",
            expected == qtsq_sha256,
            format!("{qtsq_sha256} vs {expected}"),
        );
    }

    // Content: the archived document, and the extraction it replays to.
    let payload = decode_text(&bytes, None);
    let parts = payload
        .as_ref()
        .ok()
        .and_then(|text| crate::qtsq_archive_parts(text));
    let mut replay = "not-run".to_string();
    let mut content_sha256 = String::new();
    match parts {
        Some((base, _title, html)) => {
            content_sha256 = sha256_hex(html.as_bytes());
            if let Some(expected) = manifest
                .as_ref()
                .and_then(|m| m.get("contentSha256"))
                .and_then(Value::as_str)
            {
                push(
                    "contentSha256",
                    expected == content_sha256,
                    format!("{content_sha256} vs {expected}"),
                );
            }
            let viewport = manifest
                .as_ref()
                .and_then(|m| m.get("viewport"))
                .and_then(Value::as_arr)
                .map(|v| {
                    (
                        v.first().and_then(Value::as_f64).unwrap_or(1024.0) as f32,
                        v.get(1).and_then(Value::as_f64).unwrap_or(768.0) as f32,
                    )
                })
                .unwrap_or((1024.0, 768.0));
            match replay_extraction(&base, &html, viewport) {
                Some(value) => {
                    let sha = sha256_hex(canonical_extraction(&value).as_bytes());
                    if let Some(expected) = manifest
                        .as_ref()
                        .and_then(|m| m.get("replayExtractionSha256"))
                        .and_then(Value::as_str)
                    {
                        let ok = expected == sha;
                        replay = if ok {
                            "match".into()
                        } else {
                            "mismatch".into()
                        };
                        push("replay", ok, format!("{sha} vs {expected}"));
                    } else {
                        replay = format!("unchecked:{sha}");
                    }
                }
                None => {
                    replay = "failed".to_string();
                    push(
                        "replay",
                        false,
                        "the archived document did not load".to_string(),
                    );
                }
            }
        }
        None => {
            // Encrypted archives are legitimate evidence; the content
            // checks simply cannot run without the password, and saying so
            // is more useful than failing.
            push(
                "content",
                false,
                "the archive is encrypted or not a Kitae archive payload — content \
                 and replay checks need the unlock password"
                    .to_string(),
            );
        }
    }

    let ok = checks
        .iter()
        .all(|c| c.get("ok").and_then(Value::as_bool).unwrap_or(false));
    Ok(Value::obj(vec![
        ("kitae", Value::s(EVIDENCE_VERSION)),
        ("path", Value::s(qtsq_path.to_string_lossy().to_string())),
        ("manifest", manifest.clone().unwrap_or(Value::Null)),
        (
            "signed",
            Value::Bool(info.as_ref().map(|i| i.signed).unwrap_or(false)),
        ),
        ("signature", Value::s(signature)),
        ("qtsqSha256", Value::s(qtsq_sha256)),
        ("contentSha256", Value::s(content_sha256)),
        ("replay", Value::s(replay)),
        ("checks", Value::arr(checks)),
        ("ok", Value::Bool(ok)),
    ]))
}

/// Extract a document offline — the replay half of the evidence loop: a
/// fresh engine, no scripts, no network, same viewport.
fn replay_extraction(base: &str, html: &str, viewport: (f32, f32)) -> Option<Value> {
    if html.is_empty() {
        return None;
    }
    let mut browser = Browser::new(
        Box::new(HeadlessHost::default()),
        Box::new(ApproxMeasurer),
        viewport,
    );
    browser.load_html(html, base);
    Some(crate::extract::document_json(&browser))
}

/// The canonical, layout-independent projection of an extraction: `rect`
/// values are dropped, everything else is kept. Digests are taken over
/// this, so verification does not depend on the font stack of the machine
/// doing the verifying.
fn canonical_extraction(value: &Value) -> String {
    strip_rects(value).to_json()
}

fn strip_rects(value: &Value) -> Value {
    match value {
        Value::Obj(pairs) => Value::Obj(
            pairs
                .iter()
                .filter(|(key, _)| key != "rect")
                .map(|(key, value)| (key.clone(), strip_rects(value)))
                .collect(),
        ),
        Value::Arr(items) => Value::Arr(items.iter().map(strip_rects).collect()),
        other => other.clone(),
    }
}

/// A filename-safe stem from a URL: host and last path segment, digits and
/// ASCII letters only, so an archive name can never escape its directory.
fn file_stem_for(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let mut out: String = rest
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > 80 {
        out.truncate(80);
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "page".to_string()
    } else {
        trimmed
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    for i in 0..N {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// Whether a path looks like a Kitae evidence manifest.
pub fn is_manifest(path: &Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy().ends_with(".manifest.json"))
        .unwrap_or(false)
}
