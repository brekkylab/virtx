//! Packages and the sandboxes made of them, against the fixture's servers: a member keeps one,
//! an app's user makes a sandbox of it with their token, reaches that sandbox alone with it, and
//! comes back to it; and over stdio a package makes sandboxes for its one client.

use virtx::{
    AppClient, Auth, Client, PackageAccess, PackagePutArgs, PackageSandboxCreateArgs, PortSide,
    PortSpec, SandboxCreateArgs,
};

mod common;

#[test]
fn an_app_s_user_makes_a_sandbox_of_a_package_and_reaches_it_alone() {
    let member = common::tcp_client();
    common::alpine(&member);
    let other = member
        .create_sandbox(&SandboxCreateArgs::new(common::ALPINE))
        .wait()
        .unwrap();
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
    let args =
        PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE).port(8000)).access(access);
    let package = member
        .put_package(common::unique("jwt"), &args)
        .wait()
        .unwrap();
    assert_eq!(
        member.get_package(package.name()).wait().unwrap().id(),
        package.id()
    );
    // Bob is a member of another project, let in here by the package's access alone.
    let users: serde_json::Value =
        serde_json::from_str(&std::env::var("VIRTX_TEST_USERS").unwrap()).unwrap();
    let bob = || Auth::Bearer {
        token: users["bob"].as_str().unwrap().to_owned(),
    };

    // A port in the sandbox is the package's to give; one on local, the app's user's to ask for.
    let inside = [PortSpec::new(9000)];
    let refused = common::block_on(AppClient::connect(url, package.id(), bob(), inside));
    assert_eq!(refused.unwrap_err().status(), Some(400));
    let adb = PortSpec::new(5037).on(PortSide::Local);
    let app = common::block_on(AppClient::connect(url, package.id(), bob(), [adb])).unwrap();
    let sandbox = app.sandbox();
    let ports: Vec<_> = (sandbox.info().ports.iter())
        .map(|port| (port.port, port.on))
        .collect();
    assert_eq!(ports, [(8000, PortSide::Sandbox), (5037, PortSide::Local)]);
    assert_eq!(
        common::run(sandbox, &["echo", "made of a package"]),
        (vec!["made of a package".to_owned()], 0)
    );
    // Their token reaches the sandbox they made, and nothing else of the project.
    let refused = sandbox.client().get_sandbox(other.id()).wait().unwrap_err();
    assert_eq!(refused.status(), Some(403), "{refused}");
    let refused = sandbox.client().list_sandbox().wait().unwrap_err();
    assert_eq!(refused.status(), Some(403), "{refused}");
    let refused = sandbox.client().list_volume().wait().unwrap_err();
    assert_eq!(refused.status(), Some(403), "{refused}");
    // The member sees it as any other sandbox of the project.
    let id = sandbox.id().to_owned();
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);

    // Closed, or dropped, it leaves its sandbox, which they find and reach again as who they are.
    common::block_on(app.close()).unwrap();
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);
    let theirs = Client::connect(url, bob(), None).unwrap();
    let found = theirs.list_package_sandbox(package.id()).wait().unwrap();
    let found: Vec<_> = found
        .iter()
        .map(|sandbox| sandbox.id().to_owned())
        .collect();
    assert_eq!(found, std::slice::from_ref(&id));
    let app = common::block_on(AppClient::resume(url, package.id(), bob(), &id)).unwrap();
    assert_eq!(app.sandbox().id(), id);
    assert_eq!(common::run(app.sandbox(), &["true"]).1, 0);
    drop(app);
    assert_eq!(member.get_sandbox(&id).wait().unwrap().id(), id);

    // A member finds every sandbox made of it; one let in, none but their own.
    let members = package
        .create_sandbox(&PackageSandboxCreateArgs::default())
        .wait()
        .unwrap();
    let found = package.list_sandbox().wait().unwrap();
    let found: Vec<_> = found.iter().map(|sandbox| sandbox.id()).collect();
    assert!(
        found.contains(&id.as_str()) && found.contains(&members.id()),
        "{found:?}"
    );
    let refused = common::block_on(AppClient::resume(url, package.id(), bob(), members.id()));
    assert_eq!(refused.unwrap_err().status(), Some(403));

    // Removed, it is gone, and their token reaches it no more.
    let app = common::block_on(AppClient::resume(url, package.id(), bob(), &id)).unwrap();
    common::block_on(app.remove()).unwrap();
    let gone = member.get_sandbox(&id).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    let refused = common::block_on(AppClient::resume(url, package.id(), bob(), &id));
    assert_eq!(refused.unwrap_err().status(), Some(403));
    // Removing the package leaves what was made of it before.
    package.remove().wait().unwrap();
    members.remove().wait().unwrap();
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

    let name = common::unique("by-member");
    let made = package
        .create_sandbox(&PackageSandboxCreateArgs::default().name(&name))
        .wait()
        .unwrap();
    assert_eq!(made.info().name.as_deref(), Some(name.as_str()));
    made.remove().wait().unwrap();
    package.remove().wait().unwrap();
}

#[test]
fn over_stdio_a_package_makes_sandboxes_for_its_one_client() {
    let client = common::client();
    common::alpine(&client);
    let args = PackagePutArgs::new(SandboxCreateArgs::new(common::ALPINE));
    let name = common::unique("stdio");
    let package = client.put_package(&name, &args).wait().unwrap();
    let made = client
        .create_package_sandbox(&name, &PackageSandboxCreateArgs::default())
        .wait()
        .unwrap();
    assert_eq!(common::run(&made, &["true"]).1, 0);
    made.remove().wait().unwrap();
    package.remove().wait().unwrap();
    let kept = client.list_package().wait().unwrap();
    assert!(kept.iter().all(|kept| kept.id() != package.id()));
}
