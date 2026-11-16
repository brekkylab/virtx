//! A server built on this one, from outside the crate: an authenticator of its own in place of the
//! config's projects, and a route of its own beside the server's.

use std::sync::Arc;

use http::StatusCode;
use virtx_server_tcp::access::{Access, Authenticator, Checked, Homes};
use virtx_server_tcp::serve;

/// Each token `<project>-key` is its project's.
struct Keys;

impl Authenticator for Keys {
    fn authenticate<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, String)> {
        Box::pin(async move {
            let project = token
                .and_then(|token| token.strip_suffix("-key"))
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if named.is_some_and(|named| named != project) {
                return Err(StatusCode::FORBIDDEN);
            }
            Ok((project.to_owned(), project.to_owned()))
        })
    }
}

#[tokio::test]
async fn a_server_of_its_own_authenticator_and_routes_is_built_from_the_library() {
    let root = std::env::temp_dir().join(format!("virtx-tcp-tenants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let homes = Arc::new(Homes::new(root.clone()));
    let access = Arc::new(Access::new(Vec::new(), Arc::new(Keys), homes));
    let counting = Arc::clone(&access);
    let own = axum::Router::new().route(
        "/node/homes",
        axum::routing::get(move || async move { counting.homes().len().to_string() }),
    );
    let router = serve::router(Arc::clone(&access), own);
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
    assert!(root.join("project").join("acme").is_dir());
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
    assert_eq!(
        get("/images/oci/v2/alpine/tags/list", Some("acme-key"), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    task.abort();
    for home in access.homes() {
        home.close_all();
    }
    let _ = std::fs::remove_dir_all(&root);
}
