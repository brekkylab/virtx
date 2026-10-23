//! The methods the transports' tests serve.

use crate::{
    method::Request,
    transport::{Methods, Notifier, r#impl::streamable_http::SessionId},
};

/// `echo`: answers the text it is given.
pub struct Echo;

impl Request for Echo {
    const NAME: &'static str = "echo";
    type Args = (String,);
    type Output = String;
}

/// `count`: notifies `counted` with 1 up to the number it is given, then answers it.
pub struct Count;

impl Request for Count {
    const NAME: &'static str = "count";
    type Args = (u32,);
    type Output = u32;
}

/// `session`: answers the id of the Streamable HTTP session the request came in.
pub struct Session;

impl Request for Session {
    const NAME: &'static str = "session";
    type Args = [(); 0];
    type Output = String;
}

pub fn methods() -> Methods {
    let mut methods = Methods::new();
    methods
        .add::<Echo, _, _>(|(text,), _| async move { Ok(text) })
        .add::<Count, _, _>(|(to,), extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            for i in 1..=to {
                notifier.notify("counted", i);
                tokio::task::yield_now().await;
            }
            Ok(to)
        })
        .add::<Session, _, _>(|_, extensions| async move {
            Ok(extensions.get::<SessionId>().expect("a session").0.clone())
        });
    methods
}
