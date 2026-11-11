//! A server built on this one, from outside the crate: tenants of its own in place of the
//! config's projects, and a route of its own beside the server's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::response::{IntoResponse as _, Response};
use http::StatusCode;
use serde_json::json;
use virtx_server_tcp::access::{Access, Checked, REGISTRY_PATH, Tenants};
use virtx_server_tcp::config::Config;
use virtx_server_tcp::endpoints::{Home, Images};
use virtx_server_tcp::serve;
use virtx_uvm::{ImageStore, RegistryServer};

/// Each token `<project>-key` is its project's, whose home is made the first time it is
/// named, its images its own.
struct Keys {
    root: PathBuf,
    made: Mutex<HashMap<String, Arc<Home>>>,
}

impl Tenants for Keys {
    fn home<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, Arc<Home>)> {
        Box::pin(async move {
            let project = token
                .and_then(|token| token.strip_suffix("-key"))
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if named.is_some_and(|named| named != project) {
                return Err(StatusCode::FORBIDDEN);
            }
            let mut made = self.made.lock().unwrap();
            let home = made.entry(project.to_owned()).or_insert_with(|| {
                let dir = self.root.join(project);
                let images = ImageStore::new(dir.join("images")).unwrap();
                let registry = RegistryServer::new(images.clone(), REGISTRY_PATH).unwrap();
                let registry = Arc::new(Kept(Arc::new(registry)));
                Arc::new(Home::new(images, registry, &dir).unwrap())
            });
            Ok((project.to_owned(), Arc::clone(home)))
        })
    }

    fn homes(&self) -> Vec<Arc<Home>> {
        self.made.lock().unwrap().values().cloned().collect()
    }
}

/// A registry answered around: a manifest it has not is one kept elsewhere.
struct Kept(Arc<RegistryServer>);

impl Images for Kept {
    fn handle(
        &self,
        request: axum::extract::Request,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + '_>> {
        Box::pin(async move {
            let manifest = request.uri().path().contains("/manifests/");
            let answered = Images::handle(&*self.0, request).await;
            match answered.status() == StatusCode::NOT_FOUND && manifest {
                true => "kept elsewhere".into_response(),
                false => answered,
            }
        })
    }
}

#[tokio::test]
async fn a_server_of_its_own_tenants_and_routes_is_built_from_the_library() {
    let root = std::env::temp_dir().join(format!("virtx-tcp-tenants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // Off the loopback and naming no provider: what the config's own projects refuse is not
    // asked of a server whose tenants authenticate.
    let config = Config::parse(json!({ "listen": "0.0.0.0:0", "home": root })).unwrap();
    let keys = Arc::new(Keys {
        root: config.home(),
        made: Mutex::default(),
    });
    let tenants: Arc<dyn Tenants> = keys.clone();
    let access = Arc::new(Access::new(&config, tenants));
    let counting = Arc::clone(&access);
    let own = axum::Router::new().route(
        "/node/homes",
        axum::routing::get(move || async move { counting.homes().len().to_string() }),
    );
    let router = serve::router(access, &config.origins, own);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, router).await });

    let get = async |path: &str, token: Option<&str>, project: Option<&str>| {
        let mut request = reqwest::Client::new().get(format!("{url}{path}"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(project) = project {
            request = request.header("virtx-project", project);
        }
        let response = request.send().await.unwrap();
        (response.status(), response.text().await.unwrap())
    };

    assert_eq!(get("/node/homes", None, None).await.1, "0");
    assert_eq!(
        get("/volumes/", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get("/volumes/", Some("nobody"), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get("/volumes/", Some("acme-key"), Some("other")).await.0,
        StatusCode::FORBIDDEN
    );

    // The token names its project, whose home is made as it first reaches it.
    let (status, body) = get("/volumes/", Some("acme-key"), None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    assert!(root.join("acme").is_dir());
    assert_eq!(get("/node/homes", None, None).await.1, "1");

    // Its images are its own.
    assert_eq!(
        get("/images/oci/v2/", Some("acme-key"), None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get("/images/oci/v2/", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let kept = get(
        "/images/oci/v2/alpine/manifests/3.21",
        Some("acme-key"),
        None,
    )
    .await;
    assert_eq!(kept, (StatusCode::OK, "kept elsewhere".to_owned()));
    assert_eq!(
        get("/images/oci/v2/alpine/tags/list", Some("acme-key"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    task.abort();
    for home in keys.homes() {
        home.close_all();
    }
    let _ = std::fs::remove_dir_all(&root);
}
