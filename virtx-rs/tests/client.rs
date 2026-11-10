//! The client where no request is sent, and over HTTP to the fixture's server.

use virtx::{Auth, Client, Error};

mod common;

#[test]
fn a_second_library_is_refused() {
    virtx::load(common::library()).unwrap();
    let error = virtx::load("/nonexistent/libvirtx_core").unwrap_err();
    assert!(matches!(error, Error::Library(_)));
    virtx::load(common::library()).unwrap();
}

#[test]
fn a_client_over_http_sends_its_token_and_project() {
    virtx::load(common::library()).unwrap();
    let common::Fixture {
        url,
        token,
        project,
        ..
    } = common::fixture();
    let headers = [("virtx-project", project.as_str())];
    let token = Auth::Bearer {
        token: token.clone(),
    };
    let client = Client::connect(url, token, headers).unwrap();
    let volume = client
        .create_volume(Some(&common::unique("over-http")))
        .wait()
        .unwrap();
    let listed = client.list_volume().wait().unwrap();
    assert!(listed.iter().any(|v| v.name() == volume.name()));
    volume.remove().wait().unwrap();

    let anonymous = Client::connect(url, Auth::None, headers).unwrap();
    let refused = anonymous.list_volume().wait().unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");
}

#[test]
fn a_command_is_run_for_the_token_before_a_request_is_sent() {
    virtx::load(common::library()).unwrap();
    let failing = Auth::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "echo 'not signed in' >&2; exit 1".into()],
    };
    let client = Client::connect("http://127.0.0.1:9/", failing, [] as [(&str, &str); 0]).unwrap();
    let Err(error) = client.list_sandbox().wait() else {
        panic!("listed");
    };
    assert!(error.to_string().contains("not signed in"), "{error}");
}
