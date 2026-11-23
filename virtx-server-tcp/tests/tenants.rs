//! A server built on this one, from outside the crate: an identity and a membership of its own
//! in place of the config's, and a route of its own beside the server's. Its membership keeps no
//! members: it decides from what the token grants, as a node behind a gateway does, and passes a
//! change of members on to whoever keeps them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::StatusCode;
use serde_json::json;
use virtx_core::{DEFAULT_PROJECT, Role};
use virtx_server_tcp::access::{Access, Identity, Principal};
use virtx_server_tcp::project::{Membership, Owner, Project};
use virtx_server_tcp::serve;
use virtx_uvm::{ProjectError, ProjectId, ProjectStore};

/// The token `acme-key` is an agent's, granted `acme` as its owner.
struct Keys;

impl Identity for Keys {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move {
            let claims = json!({ "project": ACME, "role": "owner" });
            let serde_json::Value::Object(claims) = claims else {
                unreachable!()
            };
            (token? == "acme-key").then(|| Principal {
                user: "agent".into(),
                claims,
            })
        })
    }
}

/// The one tenant's project, `acme`, which is whoever's token grants it, and theirs alone. Its
/// members are those a control plane keeps, which the project it answers shows none of.
#[derive(Default)]
struct Tenants {
    kept: Mutex<BTreeMap<String, Role>>,
}

const ACME: &str = "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

fn acme() -> Project {
    Project {
        id: ACME.parse().unwrap(),
        owner: Owner::User("acme".into()),
        name: "acme".into(),
        members: BTreeMap::new(),
    }
}

/// The project `who`'s token grants, and their role in it.
fn granted(who: &Principal) -> Option<(ProjectId, Role)> {
    let id = who.claims.get("project")?.as_str()?.parse().ok()?;
    let role = serde_json::from_value(who.claims.get("role")?.clone()).ok()?;
    Some((id, role))
}

impl Membership for Tenants {
    fn resolve<'a>(
        &'a self,
        who: &'a Principal,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>, ProjectError>> {
        Box::pin(async move {
            let acme = acme();
            let named = name == acme.name || name == DEFAULT_PROJECT;
            let granted = granted(who).is_some_and(|(id, _)| id == acme.id);
            Ok((granted && named).then_some(acme.id))
        })
    }

    fn role<'a>(
        &'a self,
        id: &'a ProjectId,
        who: &'a Principal,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move {
            Ok(granted(who)
                .filter(|(granted, _)| granted == id)
                .map(|(_, role)| role))
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
        _who: &'a Principal,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Project, ProjectError>> {
        Box::pin(async move { Err(ProjectError::AlreadyExists(name.to_owned())) })
    }

    fn projects<'a>(
        &'a self,
        who: &'a Principal,
    ) -> BoxFuture<'a, Result<Vec<(Project, Role)>, ProjectError>> {
        Box::pin(async move {
            let acme = acme();
            let role = granted(who).and_then(|(id, role)| (id == acme.id).then_some(role));
            Ok(role.map(|role| (acme, role)).into_iter().collect())
        })
    }

    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move {
            if *id != acme().id {
                return Err(ProjectError::Unknown(id.to_string()));
            }
            let mut kept = self.kept.lock().unwrap();
            Ok(match role {
                Some(role) => kept.insert(user.to_owned(), role),
                None => kept.remove(user),
            })
        })
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
    let access = Access::new(
        Vec::new(),
        Arc::new(Keys),
        Arc::new(Tenants::default()),
        store.clone(),
        Arc::new(store.clone()),
    );
    let access = Arc::new(access);
    let counting = Arc::clone(&access);
    // Its projects answered from its membership, merged beside its own route.
    let own = axum::Router::new()
        .route(
            "/node/homes",
            axum::routing::get(move || async move { counting.homes().len().to_string() }),
        )
        .merge(serve::projects(&access));
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
    // Naming none, a request is in the project its token grants.
    assert_eq!(
        get("/volumes/", Some("acme-key"), None).await.0,
        StatusCode::OK
    );

    // A tenant's project, whose home is made as it is first reached.
    let (status, body) = get("/volumes/", Some("acme-key"), Some("acme")).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));
    assert!(store.path(&ACME.parse().unwrap()).is_dir());
    // Its home, as a server built over the library reads it by the project's id.
    let home = access.home_of(&ACME.parse().unwrap()).await.unwrap();
    assert!(home.is_some());
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

    // Who is a member is the membership's to answer, not the members its project shows: a
    // member added is `201` and changed `200`, and one taken away `204` and then `404`.
    let bob = format!("{url}/projects/{ACME}/members/bob");
    let put = async || {
        let request = reqwest::Client::new().put(&bob).bearer_auth("acme-key");
        let request = request.json(&json!({ "role": "member" }));
        request.send().await.unwrap().status()
    };
    let delete = async || {
        let request = reqwest::Client::new().delete(&bob).bearer_auth("acme-key");
        request.send().await.unwrap().status()
    };
    assert_eq!(put().await, StatusCode::CREATED);
    assert_eq!(put().await, StatusCode::OK);
    assert_eq!(delete().await, StatusCode::NO_CONTENT);
    assert_eq!(delete().await, StatusCode::NOT_FOUND);

    task.abort();
    for home in access.homes() {
        home.close_all();
    }
    let _ = std::fs::remove_dir_all(&root);
}
