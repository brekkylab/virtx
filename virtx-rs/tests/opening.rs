//! A client opened and closed inside the program's runtime, which, the library linked in, is
//! the library's too.

use virtx::{Auth, Client};

mod common;

// The only test of this binary: it sets `$VIRTX_STDIO`, which no other thread may read meanwhile.
#[test]
fn a_client_is_opened_and_closed_inside_a_runtime() {
    virtx::load(common::library()).unwrap();
    // The fixture's server, so nothing is fetched.
    unsafe { std::env::set_var("VIRTX_STDIO", &common::fixture().stdio) };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = Client::spawn_virtx([] as [&str; 0]).await.unwrap();
        client.list_image().await.unwrap();
        client.close().await.unwrap();

        let refused = Client::connect("not a url", Auth::None, None, [] as [(&str, &str); 0]);
        assert!(refused.is_err());
    });
}
