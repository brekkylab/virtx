//! Packages and the sandboxes made of them, against the fixture's servers: a member keeps one,
//! an app's user makes a sandbox of it and reaches that sandbox alone with its grant, and over
//! stdio a package makes sandboxes with no grant.

use virtx::{
    AppClient, Auth, PackageAccess, PackagePutArgs, PackageSandboxCreateArgs, PortSide, PortSpec,
    SandboxCreateArgs,
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

    common::block_on(app.close()).unwrap();
    let gone = member.get_sandbox(&id).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    // Dropped, as closed: its sandbox is deleted.
    let dropped = common::block_on(AppClient::connect(url, package.id(), Auth::None, [])).unwrap();
    let id = dropped.sandbox().id().to_owned();
    drop(dropped);
    let gone = member.get_sandbox(&id).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
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
