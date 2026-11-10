//! The image operations against the [`testing`](crate::testing) server's registry, served at
//! `/images/oci/v2/` over an in-memory HTTP/2 connection as stdio serves one.

use std::{collections::BTreeSet, path::PathBuf};

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;

use crate::{
    client::Client,
    image::Event,
    testing::registry::{IMAGE_MANIFEST, Registry},
    transport::{TransportClient, serve_stdio},
};

impl Registry {
    /// A client of this registry over an in-memory pipe.
    fn client(&self) -> Client {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(server);
        let registry = self.clone();
        let handle = move |request| {
            let registry = registry.clone();
            async move { registry.handle(request).await }
        };
        tokio::spawn(serve_stdio(handle, input, output));
        Client {
            inner: TransportClient::over(client),
        }
    }
}

/// A path in the system's temporary directory no other test uses.
fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("virtx-{}-{name}", uuid::Uuid::new_v4()))
}

/// What a stream's events were: its lines, and its output.
async fn run<T>(
    events: impl futures_util::Stream<Item = Result<Event<T, String>, crate::Error>> + Unpin,
) -> Result<(Vec<String>, T), crate::Error> {
    let mut events = events;
    let mut lines = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            Event::Progress(progress) => lines.extend(progress.content),
            Event::Output(output) => return Ok((lines, output)),
        }
    }
    unreachable!("a stream ends with its output or its error")
}

#[tokio::test]
async fn every_tag_is_listed_with_its_digest_across_pages() {
    let registry = Registry::default();
    let alpine = registry.image("alpine", Some("3.20"), "alpine");
    // The guard is let go of before `manifest` takes it again.
    let manifest = serde_json::from_slice(&registry.state().manifests["alpine"][&alpine].1);
    registry.manifest("alpine", IMAGE_MANIFEST, manifest.unwrap(), Some("latest"));
    let base = registry.index("team/base", "1");
    let jq = registry.image("jq", Some("latest"), "jq");
    let client = registry.client();

    let listed: Vec<_> = client
        .list_image()
        .await
        .unwrap()
        .into_iter()
        .map(|image| (image.reference, image.digest))
        .collect();
    assert_eq!(
        listed,
        [
            ("alpine:3.20".to_owned(), alpine.clone()),
            ("alpine:latest".to_owned(), alpine),
            ("jq:latest".to_owned(), jq),
            ("team/base:1".to_owned(), base),
        ]
    );
}

#[tokio::test]
async fn a_tag_in_the_same_repository_points_at_the_same_manifest() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();

    client
        .tag_image("alpine:3.20", "alpine:stable")
        .await
        .unwrap();
    assert_eq!(registry.tagged("alpine", "stable"), Some(digest.clone()));

    client
        .tag_image(format!("alpine@{digest}"), "alpine:pinned")
        .await
        .unwrap();
    assert_eq!(registry.tagged("alpine", "pinned"), Some(digest));
}

#[tokio::test]
async fn a_tag_in_another_repository_brings_what_the_image_names() {
    for mounts in [true, false] {
        let registry = Registry::default();
        registry.state().mounts = mounts;
        let digest = registry.index("team/base", "1");
        let client = registry.client();

        client.tag_image("team/base:1", "mine:1").await.unwrap();
        assert_eq!(registry.tagged("mine", "1"), Some(digest), "{mounts}");
        let state = registry.state();
        assert_eq!(state.manifests["mine"].len(), 3, "{mounts}");
        assert_eq!(state.blobs["mine"], state.blobs["team/base"], "{mounts}");
    }
}

#[tokio::test]
async fn untagging_leaves_the_image_and_removing_takes_every_tag_on_it() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();
    client
        .tag_image("alpine:3.20", "alpine:latest")
        .await
        .unwrap();
    client
        .tag_image("alpine:3.20", "alpine:stable")
        .await
        .unwrap();

    client.untag_image("alpine:stable").await.unwrap();
    assert_eq!(registry.tagged("alpine", "stable"), None);
    assert!(registry.state().manifests["alpine"].contains_key(&digest));

    client.remove_image("alpine:latest").await.unwrap();
    assert_eq!(registry.tagged("alpine", "3.20"), None);
    assert!(!registry.state().manifests["alpine"].contains_key(&digest));

    assert!(
        client
            .untag_image(format!("alpine@{digest}"))
            .await
            .is_err()
    );
    assert!(client.remove_image("alpine:3.20").await.is_err());
}

#[tokio::test]
async fn a_downloaded_image_uploads_back_as_it_was() {
    let registry = Registry::default();
    let digest = registry.index("team/base", "1");
    let client = registry.client();
    let path = temporary("base.tar");

    let (lines, output) = run(client.download_image("team/base:1", &path))
        .await
        .unwrap();
    assert_eq!(output.digest, digest);
    // Two images of a config and a layer each, `arm64`'s and `amd64`'s.
    assert_eq!(lines.len(), 4, "{lines:?}");

    let mut archive = tar::Archive::new(std::fs::File::open(&path).unwrap());
    let mut index = None;
    let mut names = BTreeSet::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        if name == "index.json" {
            index = Some(serde_json::from_reader::<_, serde_json::Value>(&mut entry).unwrap());
        }
        names.insert(name);
    }
    let index = index.unwrap();
    assert_eq!(index["manifests"][0]["digest"], digest.as_str());
    assert_eq!(
        index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
        "1"
    );
    // `oci-layout`, `index.json`, and three manifests and four blobs.
    assert_eq!(names.len(), 9, "{names:?}");

    let (lines, output) = run(client.upload_image(&path, "copy:latest"))
        .await
        .unwrap();
    assert_eq!(output.digest, digest);
    assert!(
        lines.iter().all(|l| l.starts_with("uploading")),
        "{lines:?}"
    );
    assert_eq!(registry.tagged("copy", "latest"), Some(digest.clone()));
    {
        let state = registry.state();
        assert_eq!(state.blobs["copy"], state.blobs["team/base"]);
        assert_eq!(state.manifests["copy"], state.manifests["team/base"]);
    }

    let (lines, _) = run(client.upload_image(&path, "copy:again")).await.unwrap();
    assert!(
        lines.iter().all(|l| l.ends_with("is there already")),
        "{lines:?}"
    );
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn a_failed_download_writes_nothing() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();
    let path = temporary("alpine.tar");

    assert!(
        run(client.download_image("missing:1", &path))
            .await
            .is_err()
    );
    assert!(!path.exists());

    // A layer whose bytes are not its digest's.
    let manifest: serde_json::Value =
        serde_json::from_slice(&registry.state().manifests["alpine"][&digest].1).unwrap();
    let layer = manifest["layers"][0]["digest"].as_str().unwrap().to_owned();
    registry
        .state()
        .blobs
        .get_mut("alpine")
        .unwrap()
        .insert(layer, Bytes::from_static(b"alpinf"));
    let error = run(client.download_image("alpine:3.20", &path))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("was read as"), "{error}");
    assert!(!path.exists());
    let partial = path.with_file_name(format!(
        "{}.partial",
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(!partial.exists());
}

#[tokio::test]
async fn an_upload_takes_the_image_its_archive_names_by_the_tag() {
    let registry = Registry::default();
    let one = registry.image("alpine", Some("1"), "one");
    let two = registry.image("alpine", Some("2"), "two");
    let client = registry.client();
    let (first, second) = (temporary("one.tar"), temporary("two.tar"));
    run(client.download_image("alpine:1", &first))
        .await
        .unwrap();
    run(client.download_image("alpine:2", &second))
        .await
        .unwrap();

    // One archive of both: the first's entries, then the second's blobs and an index of both.
    let path = temporary("both.tar");
    let mut both = tar::Builder::new(std::fs::File::create(&path).unwrap());
    let mut descriptors = Vec::new();
    for archive in [&first, &second] {
        let mut archive = tar::Archive::new(std::fs::File::open(archive).unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            match name.as_str() {
                "index.json" => {
                    let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    descriptors.push(index["manifests"][0].clone());
                }
                "oci-layout" if !descriptors.is_empty() => {}
                _ => both
                    .append_data(&mut entry.header().clone(), &name, &bytes[..])
                    .unwrap(),
            }
        }
    }
    let index = serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": descriptors})).unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_size(index.len() as u64);
    header.set_mode(0o644);
    both.append_data(&mut header, "index.json", &index[..])
        .unwrap();
    both.finish().unwrap();

    let (_, output) = run(client.upload_image(&path, "other:2")).await.unwrap();
    assert_eq!(output.digest, two);
    let (_, output) = run(client.upload_image(&path, "other:1")).await.unwrap();
    assert_eq!(output.digest, one);
    let error = run(client.upload_image(&path, "other:3"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("none named"), "{error}");

    for path in [first, second, path] {
        std::fs::remove_file(path).unwrap();
    }
}
