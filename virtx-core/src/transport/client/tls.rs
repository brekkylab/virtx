//! A router served over TLS for the tests, offering HTTP/2 and HTTP/1.1 by ALPN as a server
//! in front of virtx does.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};

/// `router` served over TLS at an `https://localhost:<port>` URL ending in `/`, with the
/// certificate a client trusts to reach it, and how many connections it has accepted.
pub(super) async fn serve(
    router: axum::Router,
) -> (String, reqwest::Certificate, Arc<AtomicUsize>) {
    let made = rcgen::generate_simple_self_signed(["localhost".to_owned()]).unwrap();
    let key = PrivatePkcs8KeyDer::from(made.signing_key.serialize_der());
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![made.cert.der().clone()], key.into())
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "https://localhost:{}/",
        listener.local_addr().unwrap().port()
    );
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&accepted);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            counted.fetch_add(1, SeqCst);
            let (acceptor, router) = (acceptor.clone(), router.clone());
            tokio::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(
                        TokioIo::new(stream),
                        TowerToHyperService::new(router),
                    )
                    .await;
            });
        }
    });
    let certificate = reqwest::Certificate::from_der(made.cert.der()).unwrap();
    (url, certificate, accepted)
}
