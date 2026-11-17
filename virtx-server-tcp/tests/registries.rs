//! A server whose config names a registry on a private address, in a process of its own: a
//! process's registry proxy is set once.

use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncReadExt as _;
use virtx_core::{Auth, Client};
use virtx_server_tcp::config::Config;
use virtx_server_tcp::serve;

/// A pull from the registry the config names reaches it on this host's loopback, and a pull
/// from one beside it, which the config does not name, fails without a connection.
#[tokio::test]
async fn an_image_job_connects_to_the_private_registry_its_config_names() {
    let root = std::env::temp_dir().join(format!("virtx-tcp-registries-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let named = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let named_host = named.local_addr().unwrap().to_string();
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    other.set_nonblocking(true).unwrap();
    let other_host = other.local_addr().unwrap().to_string();

    let config = Config::parse(json!({
        "listen": "127.0.0.1:0",
        "home": root,
        "private_registries": [named_host],
    }))
    .unwrap();
    let (_, _, router) = serve::configured(&config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let client = Client::connect(&url, Auth::None, http::HeaderMap::new()).unwrap();

    // A guard against a hang, not a measure of anything.
    let within = Duration::from_secs(60);

    client
        .pull_image(format!("{named_host}/team/app:1"), None)
        .await
        .unwrap();
    let first = tokio::time::timeout(within, async {
        let (mut stream, _) = named.accept().await.unwrap();
        stream.read_u8().await.unwrap()
    })
    .await
    .expect("the registry named was not connected to");
    // A TLS handshake's record: the job reached the listener itself.
    assert_eq!(first, 0x16);

    let reference = format!("{other_host}/team/app:1");
    let job = client.pull_image(&reference, None).await.unwrap();
    let error = tokio::time::timeout(within, async {
        loop {
            let job = client.get_image_job(&job.id).await.unwrap();
            if !job.running {
                break job.error.expect("the job did not fail");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the job went on");
    assert_eq!(
        error,
        format!(
            "{reference}: {other_host} is not reachable from this server: \
             it is not a public address"
        )
    );
    assert_eq!(
        other.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the registry not named was connected to"
    );

    task.abort();
    let _ = std::fs::remove_dir_all(&root);
}
