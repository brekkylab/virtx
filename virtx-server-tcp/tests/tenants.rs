//! A server built on this one, from outside the crate: an identity and a store of projects of its
//! own in place of the config's, and a route of its own beside the server's.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use http::StatusCode;
use virtx_core::{DEFAULT_PROJECT, Role};
use virtx_server_tcp::access::{Access, Identity};
use virtx_server_tcp::serve;
use virtx_uvm::{Membership, Owner, Project, ProjectError, ProjectId, ProjectStore};

/// Each token `<name>-key` is the user `<name>`'s.
struct Keys;

impl Identity for Keys {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { token?.strip_suffix("-key").map(str::to_owned) })
    }
}

/// The one tenant, `acme`, the one owner of the project of its name, which is its own too: as a
/// tenant is, in a server that keeps its tenants elsewhere.
struct Tenants;

const ACME: &str = "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

fn acme() -> Project {
    Project {
        id: ACME.parse().unwrap(),
        owner: Owner::User("acme".into()),
        name: "acme".into(),
        members: BTreeMap::from([("acme".to_owned(), Role::Owner)]),
    }
}

impl Membership for Tenants {
    fn resolve<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>, ProjectError>> {
        Box::pin(async move {
            let acme = acme();
            let named = name == acme.name || name == DEFAULT_PROJECT;
            Ok((*owner == acme.owner && named).then_some(acme.id))
        })
    }

    fn project<'a>(
        &'a self,
        id: &'a ProjectId,
    ) -> BoxFuture<'a, Result<Option<Project>, ProjectError>> {
        Box::pin(async move { Ok(Some(acme()).filter(|acme| acme.id == *id)) })
    }

    fn create<'a>(
        &'a self,
        _owner: &'a Owner,
        name: &'a str,
        _creator: &'a str,
    ) -> BoxFuture<'a, Result<Project, ProjectError>> {
        Box::pin(async move { Err(ProjectError::AlreadyExists(name.to_owned())) })
    }

    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<Project>, ProjectError>> {
        Box::pin(async move {
            Ok(Some(acme())
                .filter(|acme| acme.members.contains_key(user))
                .into_iter()
                .collect())
        })
    }

    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        _user: &'a str,
        _role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::NoOwner(id.to_string())) })
    }

    fn remove<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::NoOwner(id.to_string())) })
    }
}

#[tokio::test]
async fn a_server_of_its_own_identity_projects_and_routes_is_built_from_the_library() {
    let root = std::env::temp_dir().join(format!("virtx-tcp-tenants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = ProjectStore::new(&root);
    let access = Access::new(Vec::new(), Arc::new(Keys), Arc::new(Tenants), store.clone());
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
    // Naming none, a request is in its sender's own project, the tenant's.
    assert_eq!(
        get("/volumes/", Some("acme-key"), None).await.0,
        StatusCode::OK
    );

    // A tenant's project, whose home is made as it is first reached.
    let (status, body) = get("/volumes/", Some("acme-key"), Some("acme")).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    assert!(store.path(&ACME.parse().unwrap()).is_dir());
    assert_eq!(get("/node/homes", None, None).await.1, "1");
    let (status, body) = get("/projects/", Some("acme-key"), None).await;
    assert_eq!(
        (status, body.as_str()),
        (
            StatusCode::OK,
            format!(r#"[{{"id":"{ACME}","name":"acme","owner":"acme","role":"owner"}}]"#).as_str()
        )
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
