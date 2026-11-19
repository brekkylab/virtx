//! A server built on this one that holds its machines to room of its own
//! ([`virtx_uvm::set_admission`]): a sandbox or a build it has no room for is refused as the
//! protocol says, `503` `capacity` with `Retry-After`, nothing of it made. Its own test binary,
//! since an admission is the process's.

use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::StatusCode;
use serde_json::{Value, json};
use virtx_core::{Recipe, SandboxCreateArgs, Step};
use virtx_server_tcp::access::{Access, Identity, Principal};
use virtx_server_tcp::project::{Membership as _, TcpMembership};
use virtx_server_tcp::serve;
use virtx_uvm::{Admission, FsTree, ImageStore, MachineSpec, NoRoom, ProjectStore, Room};

/// Every request the tester's.
struct Tester;

impl Identity for Tester {
    fn identify<'a>(&'a self, _token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async { Some(Principal::new("tester")) })
    }
}

/// Room for no machine, and each machine it was asked for, by its vCPUs and memory.
#[derive(Default)]
struct Full {
    asked: Mutex<Vec<(u8, u32)>>,
}

impl Admission for Full {
    fn admit(&self, spec: &MachineSpec) -> Result<Room, NoRoom> {
        self.asked.lock().unwrap().push((spec.vcpus, spec.ram_mib));
        Err(NoRoom(format!(
            "no room for {} vCPUs and {} MiB",
            spec.vcpus, spec.ram_mib
        )))
    }
}

#[tokio::test]
async fn a_machine_the_embedder_has_no_room_for_is_refused_capacity_and_nothing_is_made() {
    let full = Arc::new(Full::default());
    virtx_uvm::set_admission(Arc::clone(&full) as Arc<dyn Admission>);

    let root = std::env::temp_dir().join(format!("virtx-tcp-admission-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let projects = TcpMembership::new(&root);
    let tester = Principal::new("tester");
    let demo = projects.create(&tester, "demo").await.unwrap();
    let home = ProjectStore::new(&root).path(&demo.id);
    let store = ProjectStore::new(&root);
    let access = Access::new(
        Vec::new(),
        Arc::new(Tester),
        Arc::new(projects),
        store.clone(),
        Arc::new(store),
    );
    let access = Arc::new(access);
    let router = serve::router(Arc::clone(&access), axum::Router::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await });

    // An image to make a sandbox of: one empty layer.
    let images = ImageStore::new(home.join("images")).unwrap();
    let layer = images.insert_layer(&FsTree::new()).unwrap();
    let config = serde_json::from_str(
        r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
    )
    .unwrap();
    let image = images.insert_image(&config, &[layer]).unwrap();
    images
        .insert_tag(&"empty:1".parse().unwrap(), &image)
        .unwrap();

    let send = async |method: http::Method, path: &str, body: Option<Value>| {
        let mut request = reqwest::Client::new()
            .request(method, format!("{url}{path}"))
            .header("virtx-project", "demo");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let wait = response.headers().get("retry-after").cloned();
        let status = response.status();
        let body = response.text().await.unwrap();
        (status, wait, body)
    };
    let refused = |answered: (StatusCode, Option<http::HeaderValue>, String)| {
        let (status, wait, body) = answered;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(wait.is_some(), "a 503 says when to ask again");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"]["code"], "capacity");
        body["error"]["message"].as_str().unwrap().to_owned()
    };

    // A sandbox, asked for as it would boot, and refused with the admission's words.
    let mut made = SandboxCreateArgs::new("empty:1");
    (made.vcpus, made.memory_mib) = (Some(2), Some(512));
    let made = serde_json::to_value(made).unwrap();
    let message = refused(send(http::Method::POST, "/sandboxes/", Some(made)).await);
    assert_eq!(message, "no room for 2 vCPUs and 512 MiB");
    assert_eq!(*full.asked.lock().unwrap(), [(2, 512)]);
    // Nothing of it is left: not listed, and no disk of its own.
    let (status, _, listed) = send(http::Method::GET, "/sandboxes/", None).await;
    assert_eq!((status, listed.as_str()), (StatusCode::OK, "[]"));
    let disks = std::fs::read_dir(home.join("sandbox")).map_or(0, |dir| dir.count());
    assert_eq!(disks, 0, "a refused sandbox left its disk");

    // A build that boots a machine is refused as it is asked for, before any job is made.
    let build = |recipe: Recipe| json!({ "recipe": recipe, "reference": "built:1" });
    let booting = Recipe::new("scratch").step(Step::run("true"));
    refused(
        send(
            http::Method::POST,
            "/images/jobs/build/",
            Some(build(booting)),
        )
        .await,
    );
    assert_eq!(full.asked.lock().unwrap().len(), 2);
    let (status, _, jobs) = send(http::Method::GET, "/images/jobs/", None).await;
    assert_eq!((status, jobs.as_str()), (StatusCode::OK, "[]"));

    // One whose steps boot nothing asks for no room.
    let settled = Recipe::new("scratch").step(Step::env("A", "1"));
    let (status, _, body) = send(
        http::Method::POST,
        "/images/jobs/build/",
        Some(build(settled)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(full.asked.lock().unwrap().len(), 2);

    task.abort();
    for home in access.homes() {
        home.close_all();
    }
    let _ = std::fs::remove_dir_all(&root);
}
