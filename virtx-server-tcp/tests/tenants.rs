//! A server built on this one, from outside the crate: an identity and a store of projects of its
//! own in place of the config's, and a route of its own beside the server's.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use http::StatusCode;
use virtx_core::Role;
use virtx_server_tcp::access::{Access, Identity};
use virtx_server_tcp::serve;
use virtx_uvm::{Membership, ProjectError, ProjectStore};

/// Each token `<name>-key` is the user `<name>`'s.
struct Keys;

impl Identity for Keys {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { token?.strip_suffix("-key").map(str::to_owned) })
    }
}

/// Each user the one owner of the project of their name, and of no other: as a tenant is, in a
/// server that keeps its tenants elsewhere.
struct Tenants;

impl Membership for Tenants {
    fn role<'a>(
        &'a self,
        user: &'a str,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move { Ok((user == project).then_some(Role::Owner)) })
    }

    fn projects<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move { Ok(vec![(user.to_owned(), Role::Owner)]) })
    }

    fn create<'a>(
        &'a self,
        project: &'a str,
        _owner: &'a str,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::AlreadyExists(project.to_owned())) })
    }

    fn remove<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::NoOwner(project.to_owned())) })
    }

    fn members<'a>(
        &'a self,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move { Ok(vec![(project.to_owned(), Role::Owner)]) })
    }

    fn set_member<'a>(
        &'a self,
        project: &'a str,
        _user: &'a str,
        _role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::NoOwner(project.to_owned())) })
    }
}

#[tokio::test]
async fn a_server_of_its_own_identity_projects_and_routes_is_built_from_the_library() {
    let root = std::env::temp_dir().join(format!("virtx-tcp-tenants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = ProjectStore::new(&root);
    let access = Access::new(Vec::new(), Arc::new(Keys), Arc::new(Tenants), store);
    let access = Arc::new(access.unwrap());
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
        get("/volumes/", None, Some("acme")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get("/volumes/", Some("nobody"), Some("acme")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get("/volumes/", Some("acme-key"), Some("other")).await.0,
        StatusCode::FORBIDDEN
    );
    // Naming none, a request is in its sender's own project, which a tenant has not.
    assert_eq!(
        get("/volumes/", Some("acme-key"), None).await.0,
        StatusCode::FORBIDDEN
    );

    // A tenant's project, whose home is made as it is first reached.
    let (status, body) = get("/volumes/", Some("acme-key"), Some("acme")).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    assert!(root.join("project").join("acme").is_dir());
    assert_eq!(get("/node/homes", None, None).await.1, "1");
    let (status, body) = get("/projects/", Some("acme-key"), None).await;
    assert_eq!(
        (status, body.as_str()),
        (StatusCode::OK, r#"[{"name":"acme","role":"owner"}]"#)
    );

    // Its images are its own.
    assert_eq!(
        get("/images/oci/v2/", Some("acme-key"), Some("acme"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        get("/images/oci/v2/", None, Some("acme")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(
            "/images/oci/v2/alpine/tags/list",
            Some("acme-key"),
            Some("acme")
        )
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
