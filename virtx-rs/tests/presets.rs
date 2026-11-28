//! Presets and the sandboxes made of them, against the fixture's servers: a member keeps one,
//! an app's user makes a sandbox of it and reaches that sandbox alone with its grant, and a
//! sandbox's lifetime ends it.

use std::time::{Duration, Instant};

use virtx::{AppClient, Auth, PresetAccess, PresetPutArgs, SandboxCreateArgs};

mod common;

#[test]
fn an_app_s_user_makes_a_sandbox_of_a_public_preset_and_reaches_it_alone() {
    let member = common::tcp_client();
    common::alpine(&member);
    let other = member
        .create_sandbox(&SandboxCreateArgs::new(common::ALPINE))
        .wait()
        .unwrap();
    let args =
        PresetPutArgs::new(SandboxCreateArgs::new(common::ALPINE)).access(PresetAccess::Public);
    let preset = member
        .put_preset(common::unique("public"), &args)
        .wait()
        .unwrap();
    assert_eq!(
        member.get_preset(preset.name()).wait().unwrap().id(),
        preset.id()
    );

    let app = common::block_on(AppClient::connect(
        &common::fixture().url,
        preset.id(),
        Auth::None,
    ))
    .unwrap();
    let sandbox = app.sandbox();
    assert_eq!(
        common::run(sandbox, &["echo", "made of a preset"]),
        (vec!["made of a preset".to_owned()], 0)
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
    let dropped = common::block_on(AppClient::connect(
        &common::fixture().url,
        preset.id(),
        Auth::None,
    ))
    .unwrap();
    let id = dropped.sandbox().id().to_owned();
    drop(dropped);
    let gone = member.get_sandbox(&id).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    // Removing the preset leaves what was made of it before.
    preset.remove().wait().unwrap();
    other.remove().wait().unwrap();
}

#[test]
fn a_preset_no_one_else_may_make_lets_its_members_alone_in() {
    let member = common::tcp_client();
    common::alpine(&member);
    let args = PresetPutArgs::new(SandboxCreateArgs::new(common::ALPINE));
    let preset = member
        .put_preset(common::unique("members"), &args)
        .wait()
        .unwrap();
    let url = &common::fixture().url;
    let refused = common::block_on(AppClient::connect(url, preset.id(), Auth::None)).unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");

    let made = preset
        .create_sandbox(Some(&common::unique("by-member")))
        .wait()
        .unwrap();
    assert!(
        made.grant.is_some(),
        "a member's sandbox has a grant too, to hand on"
    );
    made.sandbox.remove().wait().unwrap();
    preset.remove().wait().unwrap();
}

#[test]
fn over_stdio_a_preset_makes_sandboxes_with_no_grant() {
    let client = common::client();
    common::alpine(&client);
    let args = PresetPutArgs::new(SandboxCreateArgs::new(common::ALPINE));
    let name = common::unique("stdio");
    let preset = client.put_preset(&name, &args).wait().unwrap();
    let made = client.create_preset_sandbox(&name, None).wait().unwrap();
    assert_eq!(made.grant, None);
    assert_eq!(common::run(&made.sandbox, &["true"]).1, 0);
    made.sandbox.remove().wait().unwrap();
    preset.remove().wait().unwrap();
    let kept = client.list_preset().wait().unwrap();
    assert!(kept.iter().all(|kept| kept.id() != preset.id()));
}

#[test]
fn a_sandbox_is_deleted_once_its_lifetime_has_passed() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(common::ALPINE).lifetime(2))
        .wait()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match client.get_sandbox(sandbox.id()).wait() {
            Err(gone) if gone.status() == Some(404) => break,
            answered => {
                assert!(Instant::now() < deadline, "still there: {answered:?}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}
