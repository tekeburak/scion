//! `scn` against an `https://` address: a TLS proxy in front of an in-process
//! `scion-server`, the way a deployment terminates TLS. The certificates in
//! `tests/tls/` are a test certificate authority and a certificate for
//! `localhost` it signed, valid until 2126.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use scion_store::{Grant, MemoryStore, MetadataStore, Role, SERVER_GRANTS};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::{ServerConfig, crypto};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/tls")
        .join(name)
}

fn key() -> scion_server::TokenKey {
    scion_server::TokenKey::from_hex(&"5c".repeat(32)).unwrap()
}

/// A plain `scion-server` behind a proxy that terminates TLS with the
/// `localhost` certificate; returns the proxy's port.
async fn serve_behind_tls() -> u16 {
    let store = Arc::new(MemoryStore::new());
    let admin = Grant {
        principal: "sp:test".into(),
        path_prefix: String::new(),
        role: Role::Admin,
    };
    store.put_grant(SERVER_GRANTS, &admin).await.unwrap();
    let plain = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = plain.local_addr().unwrap();
    tokio::spawn(scion_server::serve(
        plain,
        store.clone(),
        store,
        key(),
        None,
    ));

    let certs = CertificateDer::pem_file_iter(fixture("localhost.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let private = PrivateKeyDer::from_pem_file(fixture("localhost.key")).unwrap();
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, private)
            .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = proxy.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (conn, _) = proxy.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                // A refused handshake is the client's business, not the proxy's.
                let Ok(mut tls) = acceptor.accept(conn).await else {
                    return;
                };
                let mut up = TcpStream::connect(backend).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut tls, &mut up).await;
            });
        }
    });
    port
}

/// Run `scn args` in `dir`, trusting the test certificate authority.
fn scn(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let hour = std::time::Duration::from_secs(3600);
    let token = scion_server::token(&key(), "sp:test", hour, std::time::SystemTime::now()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scn"))
        .env("SCN_TOKEN", token)
        .env("SCN_AUTHOR", "Ada <ada@example.com>")
        .env("SSL_CERT_FILE", fixture("ca.pem"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clones_and_lands_over_tls() {
    let port = serve_behind_tls().await;
    let server = format!("https://localhost:{port}");
    let dir = tempfile::tempdir().unwrap();

    let (ok, _, err) = scn(dir.path(), &["init", &server, "secure"]);
    assert!(ok, "{err}");
    let (ok, _, err) = scn(dir.path(), &["clone", &server, "secure"]);
    assert!(ok, "{err}");
    let wc = dir.path().join("secure");
    std::fs::write(wc.join("hello.txt"), b"hello\n").unwrap();
    assert!(scn(&wc, &["add", "hello.txt"]).0);
    let (ok, _, err) = scn(&wc, &["commit", "-m", "over tls"]);
    assert!(ok, "{err}");
    let (ok, out, err) = scn(&wc, &["land"]);
    assert!(ok && out.starts_with("landed "), "{out}{err}");

    let (ok, _, err) = scn(dir.path(), &["clone", &server, "secure", "again"]);
    assert!(ok, "{err}");
    assert_eq!(
        std::fs::read(dir.path().join("again/hello.txt")).unwrap(),
        b"hello\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_for_another_name_is_refused() {
    let port = serve_behind_tls().await;
    // The certificate names `localhost`, not `127.0.0.1`.
    let server = format!("https://127.0.0.1:{port}");
    let dir = tempfile::tempdir().unwrap();
    let (ok, _, err) = scn(dir.path(), &["clone", &server, "secure"]);
    assert!(!ok);
    assert!(err.contains(&server), "{err}");
    assert!(err.contains("certificate"), "{err}");
}
