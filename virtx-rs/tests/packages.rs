//! Packages and the sandboxes made of them, against the fixture's servers: a member keeps one,
//! an app's user makes a sandbox of it, reaches that sandbox alone with its grant, and comes back
//! to it with the grant or as who they are, and over stdio a package makes sandboxes with no
//! grant.

use virtx::{
    AppClient, Auth, Client, PackageAccess, PackagePutArgs, PackageSandboxCreateArgs, PortSide,
    PortSpec, SandboxCreateArgs,
};

mod common;

#[test]
fn an_app_s_user_makes_a_sandbox_of_a_public_package_and_reaches_it_alone() {
    let member = common::tcp_client();
    common::alpine(&member);
    let other = member
        .create_sandbox(&SandboxCreateArgs::new(common::ALPINE))
        .wait()
        .unwrap();
    let args = PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE).port(8000))
        .access(PackageAccess::Public);
    let package = member
        .put_package(common::unique("public"), &args)
        .wait()
        .unwrap();
    assert_eq!(
        member.get_package(package.name()).wait().unwrap().id(),
        package.id()
    );

    let url = &common::fixture().url;
    // A port in the sandbox is the package's to give; one on local, the app's user's to ask for.
    let inside = [PortSpec::new(9000)];
    let refused = common::block_on(AppClient::connect(url, package.id(), Auth::None, inside));
    assert_eq!(refused.unwrap_err().status(), Some(400));
    let adb = PortSpec::new(5037).on(PortSide::Local);
    let app = common::block_on(AppClient::connect(url, package.id(), Auth::None, [adb])).unwrap();
    let sandbox = app.sandbox();
    let ports: Vec<_> = (sandbox.info().ports.iter())
        .map(|port| (port.port, port.on))
        .collect();
    assert_eq!(ports, [(8000, PortSide::Sandbox), (5037, PortSide::Local)]);
    assert_eq!(
        common::run(sandbox, &["echo", "made of a package"]),
        (vec!["made of a package".to_owned()], 0)
    );
    let listed = sandbox.client().list_sandbox().wait().unwrap();
    let listed: Vec<_> = listed
        .iter()
        .map(|sandbox| sandbox.id().to_owned())
        .collect();
    assert_eq!(listed, [sandbox.id()], "its own alone");
    let refused = sandbox.client().get_sandbox(other.id()).wait().unwrap_err();
    assert_eq!(refused.status(), Some(403), "{refused}");
    let refused = sandbox.client().list_volume().wait().unwrap_err();
    assert_eq!(refused.status(), Some(403), "{refused}");
    // The member sees it as any other sandbox of the project.
    let id = sandbox.id().to_owned();
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);

    // Closed, or dropped, it leaves its sandbox, which its grant reaches again.
    let grant = app.grant().to_owned();
    common::block_on(app.close()).unwrap();
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);
    let app = common::block_on(AppClient::resume(url, &grant)).unwrap();
    assert_eq!(app.sandbox().id(), id);
    assert_eq!(common::run(app.sandbox(), &["true"]).1, 0);
    drop(app);
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);
    // Made with no credentials, it is not found again as who made it: its grant alone.
    let found = AppClient::resume_as(url, package.id(), Auth::None, &id);
    let refused = common::block_on(found).unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");
    // Removed, it is gone, and its grant reaches nothing.
    let app = common::block_on(AppClient::resume(url, &grant)).unwrap();
    common::block_on(app.remove()).unwrap();
    let gone = member.get_sandbox(&id).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    let refused = common::block_on(AppClient::resume(url, &grant)).unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");
    // Removing the package leaves what was made of it before.
    package.remove().wait().unwrap();
    other.remove().wait().unwrap();
}

#[test]
fn a_package_no_one_else_may_make_lets_its_members_alone_in() {
    let member = common::tcp_client();
    common::alpine(&member);
    let args = PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE));
    let package = member
        .put_package(common::unique("members"), &args)
        .wait()
        .unwrap();
    let url = &common::fixture().url;
    let refused =
        common::block_on(AppClient::connect(url, package.id(), Auth::None, [])).unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");

    let made = package
        .create_sandbox(&PackageSandboxCreateArgs::default().name(common::unique("by-member")))
        .wait()
        .unwrap();
    assert!(
        made.grant.is_some(),
        "a member's sandbox has a grant too, to hand on"
    );
    made.sandbox.remove().wait().unwrap();
    package.remove().wait().unwrap();
}

#[test]
fn over_stdio_a_package_makes_sandboxes_with_no_grant() {
    let client = common::client();
    common::alpine(&client);
    let args = PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE));
    let name = common::unique("stdio");
    let package = client.put_package(&name, &args).wait().unwrap();
    let made = client
        .create_package_sandbox(&name, &PackageSandboxCreateArgs::default())
        .wait()
        .unwrap();
    assert_eq!(made.grant, None);
    assert_eq!(common::run(&made.sandbox, &["true"]).1, 0);
    made.sandbox.remove().wait().unwrap();
    package.remove().wait().unwrap();
    let kept = client.list_package().wait().unwrap();
    assert!(kept.iter().all(|kept| kept.id() != package.id()));
}

#[test]
fn an_app_s_user_comes_back_to_their_sandbox_as_who_they_are() {
    let member = common::tcp_client();
    common::alpine(&member);
    let url = &common::fixture().url;
    // The fixture's own issuer, as the server says it signs in with.
    let auth = reqwest::blocking::get(format!("{url}auth")).unwrap();
    let auth: serde_json::Value = serde_json::from_str(&auth.text().unwrap()).unwrap();
    let issuer = auth["oidc"]["issuer"].as_str().unwrap().to_owned();
    let access = PackageAccess::Jwt {
        issuer: issuer.clone(),
        audience: auth["oidc"]["client_id"].as_str().unwrap().to_owned(),
        jwks: Some(format!("{issuer}/keys")),
        claims: Default::default(),
    };
    let args = PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE)).access(access);
    let package = member
        .put_package(common::unique("jwt"), &args)
        .wait()
        .unwrap();
    // Bob is a member of another project, let in here by the package's access alone.
    let users: serde_json::Value =
        serde_json::from_str(&std::env::var("VIRTX_TEST_USERS").unwrap()).unwrap();
    let bob = || Auth::Bearer {
        token: users["bob"].as_str().unwrap().to_owned(),
    };

    let app = common::block_on(AppClient::connect(url, package.id(), bob(), [])).unwrap();
    let id = app.sandbox().id().to_owned();
    common::block_on(app.close()).unwrap();
    let theirs = Client::connect(url, bob(), None).unwrap();
    let found = theirs.list_package_sandbox(package.id()).wait().unwrap();
    let found: Vec<_> = found
        .iter()
        .map(|sandbox| sandbox.id().to_owned())
        .collect();
    assert_eq!(found, std::slice::from_ref(&id));
    let app = common::block_on(AppClient::resume_as(url, package.id(), bob(), &id)).unwrap();
    assert_eq!(app.sandbox().id(), id);
    assert_eq!(common::run(app.sandbox(), &["true"]).1, 0);

    // A member finds every sandbox made of it; one let in, none but their own.
    let members = package
        .create_sandbox(&PackageSandboxCreateArgs::default())
        .wait()
        .unwrap();
    let found = package.list_sandbox().wait().unwrap();
    let found: Vec<_> = found.iter().map(|sandbox| sandbox.id()).collect();
    assert!(
        found.contains(&id.as_str()) && found.contains(&members.sandbox.id()),
        "{found:?}"
    );
    let granted = package.grant_sandbox(&id).wait().unwrap();
    assert_eq!(granted.sandbox.id(), id);
    assert!(granted.grant.is_some_and(|grant| grant != app.grant()));
    let refused = theirs
        .grant_package_sandbox(package.id(), members.sandbox.id())
        .wait()
        .unwrap_err();
    assert_eq!(refused.status(), Some(404), "{refused}");

    common::block_on(app.remove()).unwrap();
    members.sandbox.remove().wait().unwrap();
    package.remove().wait().unwrap();
}
