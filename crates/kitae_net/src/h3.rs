//! G2: HTTP/3 over QUIC — quinn transport, h3 semantics, rustls+ring crypto
//! (pure Rust end to end per docs/CPP_ROLE.md). Opt-in per fetch: the
//! shared HTTP/1–2 client stays the default until the embedder asks for h3.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bytes::Buf as _;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use crate::{FetchError, Response};

/// Per-fetch HTTP/3 options.
#[derive(Debug, Clone, Default)]
pub struct H3Options {
    /// Skip certificate verification (self-signed loopback, embedder
    /// opt-in). Default `false` — verification is on otherwise.
    pub insecure_skip_verify: bool,
}

/// An "accept anything" verifier for the opt-in insecure path. Also used by
/// the wss insecure mode (`ws.rs`); never the default.
#[derive(Debug)]
pub(crate) struct InsecureSkipVerify;

impl rustls::client::danger::ServerCertVerifier for InsecureSkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Fetch `raw_url` over HTTP/3 (QUIC). HTTPS only — h3 speaks TLS 1.3 and
/// is therefore never downgrade-attacked to plaintext.
pub fn fetch_h3(raw_url: &str) -> Result<Response, FetchError> {
    fetch_h3_with(raw_url, &H3Options::default())
}

/// [`fetch_h3`] with explicit options (insecure skip-verify for self-signed
/// origins).
pub fn fetch_h3_with(raw_url: &str, opts: &H3Options) -> Result<Response, FetchError> {
    let mut parsed =
        url::Url::parse(raw_url).map_err(|e| FetchError::InvalidUrl(format!("{raw_url}: {e}")))?;
    if parsed.scheme() != "https" {
        return Err(FetchError::UnsupportedScheme(parsed.scheme().to_string()));
    }
    crate::privacy::apply_request_url_policy(&mut parsed, false)?;
    let host = parsed.host_str().unwrap_or("").to_string();
    if host.is_empty() {
        return Err(FetchError::InvalidUrl(raw_url.to_string()));
    }
    let port = parsed.port_or_known_default().unwrap_or(443);
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    }
    .to_string();

    // DNS stays on this (blocking) thread; only the QUIC work is async.
    let addrs: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| FetchError::Transport(format!("dns: {e}")))?
        .collect();
    let server_addr = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| FetchError::Transport(format!("dns: no address for {host}")))?;

    let tls_config = if opts.insecure_skip_verify {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(InsecureSkipVerify))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };
    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls_config))
        .map_err(|e| FetchError::Transport(format!("quic tls: {e}")))?;
    let client_config = quinn::ClientConfig::new(Arc::new(qcc));

    let bind_addr = if server_addr.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };

    let cfg = crate::CONFIG.get().cloned().unwrap_or_default();
    let total = cfg.total_timeout;
    let origin = crate::page_origin();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| FetchError::Transport(format!("tokio runtime: {e}")))?;

    let outcome = rt.block_on(async {
        tokio::time::timeout(total, async {
            // quinn needs a runtime context to build endpoints — the bind
            // happens here, not on the blocking thread above.
            let mut endpoint = quinn::Endpoint::client(bind_addr)
                .map_err(|e| FetchError::Transport(format!("quic bind: {e}")))?;
            endpoint.set_default_client_config(client_config);
            let conn = endpoint
                .connect(server_addr, &host)
                .map_err(|e| FetchError::Transport(format!("quic connect: {e}")))?
                .await
                .map_err(|e| FetchError::Transport(format!("quic handshake: {e}")))?;

            let (mut h3_conn, mut send_req) = h3::client::new(h3_quinn::Connection::new(conn))
                .await
                .map_err(|e| FetchError::Transport(format!("h3 handshake: {e}")))?;

            // The connection driver must be polled before any request can
            // complete — without it send_request/recv_response hang. h3 0.0.7
            // drives the connection through `wait_idle` (it is not a Future).
            let _driver = tokio::spawn(async move {
                let _ = h3_conn.wait_idle().await;
            });

            let mut builder = http::Request::builder().method("GET").uri(raw_url);
            if crate::dnt_enabled() {
                builder = builder.header("dnt", "1");
            }
            if let Some(value) = crate::cookie_header_for_origin(&host, &path, true, &origin) {
                builder = builder.header("cookie", value);
            }
            let request = builder
                .body(())
                .map_err(|e| FetchError::Transport(format!("h3 request: {e}")))?;
            let mut stream = send_req
                .send_request(request)
                .await
                .map_err(|e| FetchError::Transport(format!("h3 send: {e}")))?;
            let resp = stream
                .recv_response()
                .await
                .map_err(|e| FetchError::Transport(format!("h3 response: {e}")))?;

            let status = resp.status().as_u16();
            let mut content_type = String::new();
            let mut cache_control = String::new();
            let mut etag = String::new();
            let mut last_modified = String::new();
            let mut content_disposition = String::new();
            let mut strict_transport_security = String::new();
            let mut set_cookies: Vec<String> = Vec::new();
            for (name, value) in resp.headers() {
                let v = String::from_utf8_lossy(value.as_bytes()).into_owned();
                match name.as_str() {
                    "content-type" => content_type = v,
                    "cache-control" => cache_control = v,
                    "etag" => etag = v,
                    "last-modified" => last_modified = v,
                    "content-disposition" => content_disposition = v,
                    "strict-transport-security" => strict_transport_security = v,
                    "set-cookie" => set_cookies.push(v),
                    _ => {}
                }
            }
            let mut body = Vec::new();
            while let Some(mut chunk) = stream
                .recv_data()
                .await
                .map_err(|e| FetchError::Transport(format!("h3 body: {e}")))?
            {
                let bytes = chunk.copy_to_bytes(chunk.remaining());
                body.extend_from_slice(&bytes);
            }
            Ok((
                status,
                content_type,
                cache_control,
                etag,
                last_modified,
                content_disposition,
                strict_transport_security,
                body,
                set_cookies,
            ))
        })
        .await
        .map_err(|_| FetchError::Transport("h3: fetch timed out".into()))?
    });

    let (
        status,
        content_type,
        cache_control,
        etag,
        last_modified,
        content_disposition,
        strict_transport_security,
        body,
        set_cookies,
    ) = outcome?;
    crate::check_final_url(raw_url, false)?;
    if !strict_transport_security.is_empty() {
        crate::observe_hsts(raw_url, &strict_transport_security);
    }
    crate::store_cookie_values_for_origin(&set_cookies, &host, &path, true, &origin);
    Ok(Response {
        final_url: raw_url.to_string(),
        status,
        body,
        content_type,
        cache_control,
        etag,
        last_modified,
        protocol: "HTTP/3".to_string(),
        content_disposition,
        // H3 challenges (h2c/HTTP/3 401s) are rare; the H1/H2 path carries
        // the header when a server sends one.
        www_authenticate: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::privacy_test_lock;

    #[test]
    fn h3_roundtrip_over_quic() {
        let _lock = privacy_test_lock();
        crate::reset_privacy_config();
        crate::set_dnt_enabled(true);
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("cert");
        let cert_der = cert.der().clone();
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

        let tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .expect("server cert");
        let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls))
            .expect("quic server tls");
        // The endpoint must live inside the server thread's runtime — quinn
        // spawns its driver at construction, so a scratch runtime here would
        // strand it. The port comes back over a channel.
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            rt.block_on(async move {
                let endpoint = quinn::Endpoint::server(
                    quinn::ServerConfig::with_crypto(Arc::new(qsc)),
                    "127.0.0.1:0".parse().expect("addr"),
                )
                .expect("server endpoint");
                port_tx
                    .send(endpoint.local_addr().unwrap().port())
                    .expect("send port");

                let incoming = endpoint.accept().await.expect("incoming");
                let conn = incoming.await.expect("quic connection");
                let mut h3 = h3::server::Connection::new(h3_quinn::Connection::new(conn))
                    .await
                    .expect("h3 server");
                if let Some((req, mut stream)) = h3.accept().await.expect("h3 accept") {
                    assert_eq!(
                        req.headers().get("dnt").map(|value| value.as_bytes()),
                        Some(b"1".as_slice())
                    );
                    let resp = http::Response::builder()
                        .status(200)
                        .header("content-type", "text/plain")
                        .header("etag", "\"h3-1\"")
                        .body(())
                        .expect("response");
                    stream.send_response(resp).await.expect("send_response");
                    stream
                        .send_data(&b"h3 says hi"[..])
                        .await
                        .expect("send_data");
                    stream.finish().await.expect("finish");
                }
                // Give the client a beat to read the body before closing.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            });
        });

        let opts = H3Options {
            insecure_skip_verify: true,
        };
        let port = port_rx.recv().expect("server port");
        let resp = fetch_h3_with(&format!("https://localhost:{port}/hi"), &opts).expect("h3 fetch");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.protocol, "HTTP/3");
        assert_eq!(resp.text(), "h3 says hi");
        assert_eq!(resp.etag, "\"h3-1\"");
        assert_eq!(resp.content_type, "text/plain");
        server.join().expect("server thread");
        crate::reset_privacy_config();
    }

    #[test]
    fn h3_refuses_plaintext_schemes() {
        let e = fetch_h3("http://localhost:9/x").unwrap_err();
        assert_eq!(e, FetchError::UnsupportedScheme("http".into()));
    }
}
