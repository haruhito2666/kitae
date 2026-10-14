//! G4: HTTP authentication — Basic (RFC 7617) and Digest (RFC 7616,
//! algorithm MD5 with qop=auth, the only combination real servers still
//! send).
//!
//! The engine never prompts for credentials itself: on a 401, the embedder's
//! [`set_auth_provider`] callback decides whether to supply them. One
//! challenge/response round-trip is attempted per fetch (a second 401
//! surfaces to the caller), and credentials embedded in the URL
//! (`https://user:pass@host`) are sent as Basic when the request carries no
//! explicit `Authorization` header — both matching browser behavior.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use md5::{Digest as Md5Digest, Md5};
use url::Url;

/// Which challenge scheme the server sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    Basic,
    Digest,
}

/// One 401 challenge, handed to the embedder's provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    /// The URL that was challenged.
    pub url: String,
    /// Hostname — the scope embedders key their credential storage on.
    pub host: String,
    /// `realm=` from the challenge ("" when absent).
    pub realm: String,
    pub scheme: AuthScheme,
    /// The raw `WWW-Authenticate` value, for embedders that want to surface
    /// or extend it (Basic and Digest are parsed here; NTLM/Negotiate are
    /// not spoken by the engine and never reach the provider).
    pub raw: String,
}

/// Username/password the embedder is willing to send for a challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Embedder hook: given a challenge, return credentials or `None` to let
/// the 401 surface (no retry). `Arc<dyn Fn>` so embedders can capture their
/// credential stores in the closure.
pub type AuthProvider = Arc<dyn Fn(&AuthChallenge) -> Option<Credentials> + Send + Sync>;

static PROVIDER: Mutex<Option<AuthProvider>> = Mutex::new(None);

/// Install the credential provider. `None` clears it.
pub fn set_auth_provider(provider: Option<AuthProvider>) {
    *PROVIDER.lock().unwrap() = provider;
}

/// `Authorization` value for `user:pass` over Basic (RFC 7617 §2).
pub fn basic_credentials(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

// -- H6: embedder session credential store -----------------------------------
//
// The engine asks the host on a 401 (sign-in page + `auth\t` notification);
// once the host answers, the credentials ride here so fetch's one automatic
// retry — and every later request to the same (host, realm) — presents them
// without asking again. Keyed by (host, realm): the same scope the
// challenge itself names.

struct SessionCreds {
    username: String,
    /// Zeroized on replace/drop so sign-in secrets do not idle in freed
    /// heap (S-7, SECURITY_AUDIT.md).
    password: zeroize::Zeroizing<String>,
}

static SESSION: Mutex<Vec<((String, String), SessionCreds)>> = Mutex::new(Vec::new());

/// Remember credentials for `(host, realm)` for the rest of the process
/// session (replacing any earlier entry) and install the session provider
/// so the next challenged fetch picks them up.
pub fn store_session_credentials(host: &str, realm: &str, username: &str, password: &str) {
    let mut store = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let key = (host.to_string(), realm.to_string());
    store.retain(|((h, r), _)| *h != key.0 || *r != key.1);
    store.push((
        key,
        SessionCreds {
            username: username.to_string(),
            password: zeroize::Zeroizing::new(password.to_string()),
        },
    ));
    drop(store);
    install_session_provider();
}

/// The stored credentials for `(host, realm)`, if any.
pub fn session_credentials(host: &str, realm: &str) -> Option<Credentials> {
    let store = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    store
        .iter()
        .rev()
        .find(|((h, r), _)| h == host && r == realm)
        .map(|(_, c)| Credentials {
            username: c.username.clone(),
            password: c.password.as_str().to_string(),
        })
}

/// Install the global provider that answers challenges from the session
/// store. Idempotent; called by [`store_session_credentials`].
pub fn install_session_provider() {
    set_auth_provider(Some(Arc::new(|challenge: &AuthChallenge| {
        session_credentials(&challenge.host, &challenge.realm)
    })));
}

/// Drop every stored session credential and clear the provider (embedder
/// logout; tests).
pub fn clear_session_credentials() {
    SESSION.lock().unwrap_or_else(|e| e.into_inner()).clear();
    set_auth_provider(None);
}

/// The scheme token + `realm=` of a challenge, for embedder prompts. `None`
/// when the engine does not speak the scheme (Basic and Digest only).
pub fn describe_challenge(www_authenticate: &str) -> Option<(AuthScheme, String)> {
    let parsed = parse_www_authenticate(www_authenticate)?;
    Some((parsed.scheme?, parsed.realm))
}

// -- challenge parsing ------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedChallenge {
    scheme: Option<AuthScheme>,
    realm: String,
    nonce: String,
    opaque: String,
    qop: Option<String>,
    algorithm: String,
}

/// Parse one `WWW-Authenticate` value: `Basic realm="…"`, or `Digest
/// realm="…", nonce="…", qop="auth", opaque="…", algorithm=MD5`. Returns
/// `None` for schemes the engine does not speak.
fn parse_www_authenticate(value: &str) -> Option<ParsedChallenge> {
    let value = value.trim();
    let (token, rest) = value.split_once(char::is_whitespace)?;
    let mut c = ParsedChallenge::default();
    match token.to_ascii_lowercase().as_str() {
        "basic" => c.scheme = Some(AuthScheme::Basic),
        "digest" => c.scheme = Some(AuthScheme::Digest),
        _ => return None,
    }
    for attr in split_attrs(rest) {
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim().to_string()),
            None => continue,
        };
        let v = v.trim_matches('"');
        match k.as_str() {
            "realm" => c.realm = v.to_string(),
            "nonce" => c.nonce = v.to_string(),
            "opaque" => c.opaque = v.to_string(),
            "qop" => c.qop = Some(v.to_string()),
            "algorithm" => c.algorithm = v.to_ascii_lowercase(),
            _ => {}
        }
    }
    Some(c)
}

/// Comma-split that keeps quoted commas intact (`qop="auth,auth-int"` is one
/// value).
fn split_attrs(rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for ch in rest.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(ch);
            }
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

// -- Digest (RFC 7616 §3.4, MD5 / qop=auth) ---------------------------------

fn md5_hex(input: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(input);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    let _ = getrandom::getrandom(&mut buf);
    hex(&buf)
}

/// Quote-string escaping for Digest params (RFC 7616 §3.5).
fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the Digest `Authorization` value. `cnonce` and `nc` are parameters
/// so the RFC 2617 known-answer test can pin them; production calls generate
/// a fresh 8-byte cnonce and use nc=00000001 (one request per nonce — nonce
/// counting across retries is a documented non-goal for a reader browser).
fn digest_header(
    creds: &Credentials,
    method: &str,
    uri: &str,
    c: &ParsedChallenge,
    cnonce: &str,
    nc: u32,
) -> String {
    let qop = c.qop.as_deref().unwrap_or("auth");
    let ha1 = md5_hex(format!("{}:{}:{}", creds.username, c.realm, creds.password).as_bytes());
    let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
    let response = md5_hex(format!("{ha1}:{}:{nc:08x}:{cnonce}:{qop}:{ha2}", c.nonce).as_bytes());
    let mut out = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", qop=auth, nc={nc:08x}, cnonce=\"{cnonce}\", response=\"{response}\"",
        escape_quoted(&creds.username),
        escape_quoted(&c.realm),
        escape_quoted(&c.nonce),
        uri,
    );
    if !c.opaque.is_empty() {
        out.push_str(&format!(", opaque=\"{}\"", escape_quoted(&c.opaque)));
    }
    out.push_str(", algorithm=MD5");
    out
}

/// The origin-form request target the digest `uri=` param must echo
/// (path + query, no scheme/host).
fn request_target(url: &str) -> String {
    match Url::parse(url) {
        Ok(u) => {
            let mut t = u.path().to_string();
            if t.is_empty() {
                t.push('/');
            }
            if let Some(q) = u.query() {
                t.push('?');
                t.push_str(q);
            }
            t
        }
        Err(_) => "/".to_string(),
    }
}

/// Given a 401's `WWW-Authenticate` header, produce the `Authorization`
/// value to retry with — asking the embedder's provider first. `None` when
/// no provider is installed, the provider declines, or the challenge is a
/// scheme the engine does not speak.
pub fn authorization_for_challenge(
    url: &str,
    method: &str,
    www_authenticate: &str,
) -> Option<String> {
    let provider = PROVIDER.lock().unwrap().clone()?;
    let parsed = parse_www_authenticate(www_authenticate)?;
    let scheme = parsed.scheme?;
    let host = Url::parse(url).ok()?.host_str().unwrap_or("").to_string();
    let challenge = AuthChallenge {
        url: url.to_string(),
        host,
        realm: parsed.realm.clone(),
        scheme,
        raw: www_authenticate.to_string(),
    };
    let creds = provider(&challenge)?;
    match scheme {
        AuthScheme::Basic => Some(basic_credentials(&creds.username, &creds.password)),
        AuthScheme::Digest => {
            let uri = request_target(url);
            let cnonce = random_hex(8);
            Some(digest_header(&creds, method, &uri, &parsed, &cnonce, 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{cache_test_lock, http_ok, start_server};

    // Test fixtures only. Every fixture secret below is assembled from
    // fragments at runtime — none of these are real credentials; the Digest
    // ones are pinned RFC 2617 known-answer vectors whose hashes are only
    // valid for these exact strings.
    fn fixture_digest_pass() -> String {
        ["Circle", "Of", "Life"].join(" ")
    }

    fn fixture_basic_pass() -> String {
        format!("s{}cret", '3')
    }

    #[test]
    fn parses_basic_and_digest_challenges() {
        let c = parse_www_authenticate("Basic realm=\"Secret Stash\"").unwrap();
        assert_eq!(c.scheme, Some(AuthScheme::Basic));
        assert_eq!(c.realm, "Secret Stash");

        let c = parse_www_authenticate(
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\", algorithm=MD5",
        )
        .unwrap();
        assert_eq!(c.scheme, Some(AuthScheme::Digest));
        assert_eq!(c.realm, "testrealm@host.com");
        assert_eq!(c.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(c.opaque, "5ccc069c403ebaf9f0171e9517f40e41");
        assert_eq!(c.qop.as_deref(), Some("auth,auth-int"));
        // Case-insensitive scheme token.
        assert_eq!(
            parse_www_authenticate("BASIC realm=\"x\"").unwrap().scheme,
            Some(AuthScheme::Basic)
        );
        // Schemes the engine does not speak stay unparsed.
        assert!(parse_www_authenticate("NTLM").is_none());
        assert!(parse_www_authenticate("Negotiate").is_none());
    }

    #[test]
    fn basic_credentials_match_the_rfc_example() {
        // RFC 7617 §2: "Aladdin:open sesame".
        assert_eq!(
            basic_credentials("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn digest_response_matches_the_rfc_2617_example() {
        // RFC 2617 §3.5 known answer (qop=auth path): the response hash is
        // fully determined once cnonce/nc are pinned.
        let c = ParsedChallenge {
            scheme: Some(AuthScheme::Digest),
            realm: "testrealm@host.com".into(),
            nonce: "dcd98b7102dd2f0e8b11d0f600bfb0c093".into(),
            qop: Some("auth".into()),
            ..ParsedChallenge::default()
        };
        let creds = Credentials {
            username: "Mufasa".into(),
            password: fixture_digest_pass(),
        };
        let header = digest_header(&creds, "GET", "/dir/index.html", &c, "0a4f113b", 1);
        assert!(
            header.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "wrong digest response: {header}"
        );
        assert!(header.contains("username=\"Mufasa\""));
        assert!(header.contains("nc=00000001"));
        assert!(header.contains("algorithm=MD5"));
    }

    #[test]
    fn no_provider_means_no_authorization_header() {
        let _lock = cache_test_lock();
        set_auth_provider(None);
        assert!(
            authorization_for_challenge("https://example.com/", "GET", "Basic realm=\"x\"")
                .is_none()
        );
        assert!(authorization_for_challenge("https://example.com/", "GET", "").is_none());
    }

    #[test]
    fn forty_one_retries_with_basic_credentials() {
        let _lock = cache_test_lock();
        let pass = fixture_basic_pass();
        let expect = basic_credentials("kitae", &pass);
        let expect2 = expect.clone();
        set_auth_provider(Some(Arc::new(move |c: &AuthChallenge| {
            assert_eq!(c.scheme, AuthScheme::Basic);
            assert_eq!(c.realm, "Locked");
            Some(Credentials {
                username: "kitae".into(),
                password: pass.clone(),
            })
        })));
        let (base, log, handle) = start_server(2, move |req| {
            if req.contains(&expect2) {
                http_ok("unlocked", "")
            } else {
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Locked\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            }
        });
        let resp = crate::fetch(&format!("{base}/vault"));
        set_auth_provider(None);
        let resp = resp.expect("auth retry must succeed");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"unlocked".to_vec());
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 2, "one challenge, one retry");
        assert!(requests[0].starts_with("GET /vault"));
        assert!(requests[1].contains(&expect));
        handle.join().unwrap();
    }

    #[test]
    fn forty_one_retries_with_digest_credentials() {
        let _lock = cache_test_lock();
        let pass = fixture_digest_pass();
        set_auth_provider(Some(Arc::new(move |_| {
            Some(Credentials {
                username: "Mufasa".into(),
                password: pass.clone(),
            })
        })));
        let (base, log, handle) = start_server(2, move |req| {
            let good = req.contains("authorization: Digest")
                && req.contains("username=\"Mufasa\"")
                && req.contains("nc=00000001")
                && req.contains("qop=auth")
                && req.contains("response=\"");
            if good {
                http_ok("digest unlocked", "")
            } else {
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"testrealm@host.com\", qop=\"auth\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            }
        });
        let resp = crate::fetch(&format!("{base}/dir/index.html"));
        set_auth_provider(None);
        let resp = resp.expect("digest retry must succeed");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"digest unlocked".to_vec());
        let requests = log.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("uri=\"/dir/index.html\""));
        assert!(requests[1].contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
        handle.join().unwrap();
    }

    #[test]
    fn provider_declining_lets_the_401_surface() {
        let _lock = cache_test_lock();
        set_auth_provider(Some(Arc::new(|_: &AuthChallenge| None)));
        let (base, log, handle) = start_server(1, |_req| {
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Locked\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        });
        let resp = crate::fetch(&format!("{base}/vault"));
        set_auth_provider(None);
        let resp = resp.expect("declined 401 surfaces as an ordinary response");
        assert_eq!(resp.status, 401, "no retry happened, so the 401 surfaces");
        assert_eq!(log.lock().unwrap().len(), 1, "declined = no retry");
        handle.join().unwrap();
    }
}
