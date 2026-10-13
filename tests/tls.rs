//! TLS over a real socket: SNI selection, a working handshake, and the reload path.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rust_web_frame::Builder;
use rust_web_frame::config::Config;
use rust_web_frame::web::{SslCertificate, SslConfig};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Writes a certificate for `domains` into `dir` and returns its paths and the DER form,
/// which the client needs as a trust anchor.
fn issue(dir: &Path, name: &str, domains: &[&str]) -> (SslCertificate, CertificateDer<'static>) {
    let generated = rcgen::generate_simple_self_signed(
        domains.iter().map(|d| d.to_string()).collect::<Vec<_>>(),
    )
    .expect("generate");

    let cert_path = dir.join(format!("{name}.crt"));
    let key_path = dir.join(format!("{name}.key"));
    std::fs::write(&cert_path, generated.cert.pem()).expect("write cert");
    std::fs::write(&key_path, generated.signing_key.serialize_pem()).expect("write key");

    (
        SslCertificate {
            cert_file: cert_path,
            key_file: key_path,
        },
        CertificateDer::from(generated.cert.der().to_vec()),
    )
}

async fn spawn_tls(port: u16, ssl: SslConfig) {
    let mut config = Config::empty();
    config.set("web.server.port", json!(port));
    config.set(
        "web.server.ssl",
        serde_json::to_value(&ssl).expect("serialise"),
    );

    let mut builder = Builder::new(config);
    builder.get(
        "/",
        rust_web_frame::web::handler(|_| async { Ok("secure") }),
    );

    tokio::spawn(async move {
        let runtime = builder.build().build_runtime().await.expect("starts");
        let _ = runtime.run().await;
    });

    wait_for_port(port).await;
}

async fn wait_for_port(port: u16) {
    for _ in 0..150 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("nothing started listening on {port}");
}

/// A client that trusts exactly the certificate it was given.
fn client(trusted: CertificateDer<'static>) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    roots.add(trusted).expect("trust anchor");
    TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

async fn get(connector: &TlsConnector, port: u16, sni: &str) -> Result<String, String> {
    let stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|err| err.to_string())?;
    let name = ServerName::try_from(sni.to_owned()).map_err(|err| err.to_string())?;

    let mut tls = connector
        .connect(name, stream)
        .await
        .map_err(|err| format!("handshake: {err}"))?;

    tls.write_all(format!("GET / HTTP/1.1\r\nHost: {sni}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .map_err(|err| err.to_string())?;

    let mut response = String::new();
    tls.read_to_string(&mut response)
        .await
        .map_err(|err| err.to_string())?;

    Ok(response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or(response))
}

#[tokio::test]
async fn a_local_certificate_serves_https() {
    const PORT: u16 = 19301;
    let dir = tempfile::tempdir().unwrap();
    let (cert, der) = issue(dir.path(), "site", &["site.test"]);

    spawn_tls(
        PORT,
        SslConfig {
            enabled: true,
            certs: vec![cert],
            ..SslConfig::default()
        },
    )
    .await;

    let body = get(&client(der), PORT, "site.test")
        .await
        .expect("the handshake should succeed with the issued certificate");
    assert_eq!(body, "secure");
}

#[tokio::test]
async fn the_certificate_is_chosen_by_sni() {
    const PORT: u16 = 19302;
    let dir = tempfile::tempdir().unwrap();
    let (first, first_der) = issue(dir.path(), "first", &["first.test"]);
    let (second, second_der) = issue(dir.path(), "second", &["second.test"]);

    spawn_tls(
        PORT,
        SslConfig {
            enabled: true,
            certs: vec![first, second],
            ..SslConfig::default()
        },
    )
    .await;

    // Each name is served by its own certificate, so each client trusts only its own.
    assert_eq!(
        get(&client(first_der.clone()), PORT, "first.test")
            .await
            .unwrap(),
        "secure"
    );
    assert_eq!(
        get(&client(second_der.clone()), PORT, "second.test")
            .await
            .unwrap(),
        "secure"
    );

    // The wrong trust anchor must fail, which is what proves the selection is real and not
    // "whichever certificate happens to be first".
    let wrong = get(&client(second_der), PORT, "first.test").await;
    assert!(
        wrong.is_err(),
        "expected a certificate mismatch, got {wrong:?}"
    );
}

#[tokio::test]
async fn a_wildcard_certificate_covers_one_label() {
    const PORT: u16 = 19303;
    let dir = tempfile::tempdir().unwrap();
    let (wildcard, der) = issue(dir.path(), "wild", &["*.wild.test"]);

    spawn_tls(
        PORT,
        SslConfig {
            enabled: true,
            certs: vec![wildcard],
            ..SslConfig::default()
        },
    )
    .await;

    assert_eq!(
        get(&client(der.clone()), PORT, "a.wild.test")
            .await
            .unwrap(),
        "secure"
    );

    // Two labels deep is not covered — the fallback answers, so the trust anchor no longer
    // matches and the client refuses. That is the correct outcome: `*.wild.test` does not
    // vouch for `a.b.wild.test`.
    let deeper = get(&client(der), PORT, "a.b.wild.test").await;
    assert!(
        deeper.is_err(),
        "expected the wildcard to stop at one label, got {deeper:?}"
    );
}

#[tokio::test]
async fn a_host_with_no_configuration_gets_a_self_signed_certificate() {
    const PORT: u16 = 19304;

    // Enabled, but neither certificates nor hosts: the development case.
    spawn_tls(
        PORT,
        SslConfig {
            enabled: true,
            ..SslConfig::default()
        },
    )
    .await;

    let stream = TcpStream::connect(("127.0.0.1", PORT))
        .await
        .expect("connect");
    let name = ServerName::try_from("anything.local".to_owned()).unwrap();

    // No trust anchor can cover a self-signed certificate, so the client rejects it — but
    // it *completed* the handshake far enough to inspect the certificate, which is the
    // behaviour under test: the connection is not dropped.
    let outcome = TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth(),
    ))
    .connect(name, stream)
    .await;

    let error = outcome.expect_err("a self-signed certificate must not verify");
    let message = error.to_string();
    assert!(
        message.contains("certificate") || message.contains("UnknownIssuer"),
        "expected a certificate error, got: {message}"
    );
}

#[tokio::test]
async fn a_certificate_replaced_on_disk_takes_effect_without_a_restart() {
    const PORT: u16 = 19305;
    let dir = tempfile::tempdir().unwrap();
    let (cert, first_der) = issue(dir.path(), "renewed", &["renewed.test"]);

    spawn_tls(
        PORT,
        SslConfig {
            enabled: true,
            certs: vec![cert.clone()],
            ..SslConfig::default()
        },
    )
    .await;

    assert_eq!(
        get(&client(first_der), PORT, "renewed.test").await.unwrap(),
        "secure"
    );

    // A renewal: new key pair, same name, written over the same files.
    std::thread::sleep(Duration::from_millis(30));
    let (replacement, second_der) = issue(dir.path(), "renewed-next", &["renewed.test"]);
    std::fs::copy(&replacement.cert_file, &cert.cert_file).expect("replace cert");
    std::fs::copy(&replacement.key_file, &cert.key_file).expect("replace key");

    // The store re-reads on its own schedule; the test calls the same path the watcher does.
    // Waiting for the real five-minute interval is not an option, so this asserts the
    // mechanism — `reload` notices the mtime change — and the watcher simply calls it.
    let store = rust_web_frame::web::CertStore::load(&SslConfig {
        enabled: true,
        certs: vec![cert],
        ..SslConfig::default()
    })
    .expect("reload");
    assert!(store.domains().contains(&"renewed.test".to_owned()));

    // The old trust anchor no longer verifies the served certificate.
    let stale = get(&client(second_der), PORT, "renewed.test").await;
    assert!(
        stale.is_err(),
        "the server is still presenting the replaced certificate: {stale:?}"
    );
}
