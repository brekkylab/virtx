//! One session driven from two places that share no types: frames over a [`Relay`].
//!
//! Two native extensions in one process (virtx's Python module and another that links this
//! crate, say) each have their own copy of [`ConsoleClient`], and neither can see into the
//! other's. What they can pass is bytes, so the console that owns a session answers protocol
//! frames with [`ConsoleClient::relay`], and the other side drives it as an ordinary
//! [`ConsoleClient`] from [`ConsoleClient::attach`], over whatever carries the frames.
//!
//! A frame is one [`Message`] as BSON: the document the stdio channel carries, without its
//! length prefix. So the two ends need only speak one protocol version, not share a build of
//! this crate.

use std::path::PathBuf;

use futures_core::future::BoxFuture;

use super::{ConsoleClient, Tree};
use crate::protocol::{Call, Client, Error, Failure, Message, Notification, RequestId, Response};

/// What carries a frame to the console that owns the session, and its answer back.
///
/// `relay` hands `frame` to the owner's [`ConsoleClient::relay`] and resolves to what that
/// returned. An owner that is gone (closed, or unreachable) is
/// [`Broken`](Failure::Broken), as a channel's end is.
pub trait Relay: Send {
    fn relay(&mut self, frame: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Failure>>;
}

impl ConsoleClient {
    /// Answer one frame from a console [attached](Self::attach) to this one's session.
    ///
    /// A request's answer is its response frame, a refusal included: it travels as the
    /// response's `error`, as it does on the wire. A notification's is empty. `Err` is only a
    /// broken channel (this console's, or an unreadable frame), after which the attached
    /// side hears nothing more.
    ///
    /// The session stays this console's: `init` is refused, since the session was announced
    /// when this console was built, and `quit` is dropped, since only the owner ends it.
    pub async fn relay(&mut self, frame: &[u8]) -> Result<Vec<u8>, Failure> {
        match decode(frame)? {
            Message::Request {
                id,
                call: Call::Init(_),
            } => encode(&Message::Response {
                id,
                result: Response::Error(Error::new(
                    Error::INVALID_REQUEST,
                    "the session was announced by the console that owns it",
                )),
            }),

            Message::Request { id, call } => {
                let result = match self.client.call(call).await {
                    Ok(answer) => answer,
                    Err(Failure::Refused(error)) => Response::Error(error),
                    Err(broken) => return Err(broken),
                };
                encode(&Message::Response { id, result })
            }

            Message::Notification(Notification::Quit) => Ok(Vec::new()),

            Message::Notification(notification) => {
                self.client.notify(notification).await?;
                Ok(Vec::new())
            }

            Message::Response { id, .. } => Err(Failure::broken(format!(
                "a relayed frame answered request {id}; only the owning console answers"
            ))),
        }
    }

    /// A console over a session another console owns, reached through `relay`.
    ///
    /// `mounts` are the owner's [`mounts`](Self::mounts), which this console reports as its
    /// own; the owner keeps the trees up.
    ///
    /// Everything but the ending is the owner's session: [`start`](Self::start),
    /// [`exec`](Self::exec) and the rest go to it, one frame each. Dropping this console
    /// says no `quit`, so the session lasts until the owner ends it, and a call after that
    /// fails as [`Broken`](Failure::Broken).
    pub fn attach(
        relay: impl Relay + 'static,
        mounts: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> ConsoleClient {
        ConsoleClient {
            client: Box::new(Relayed { relay, next_id: 0 }),
            mounts: mounts
                .into_iter()
                .map(|path| {
                    let path = path.into();
                    // The owner holds the tree; a path is a mount with nothing to take down.
                    Tree {
                        mount: Box::new(path.clone()),
                        path,
                    }
                })
                .collect(),
        }
    }
}

/// The client an [attached](ConsoleClient::attach) console speaks through.
struct Relayed<R> {
    relay: R,

    /// The id the next request goes out under, so an answer can be checked against it.
    ///
    /// Not what pairs an answer with its request, as on the stdio channel: each
    /// [`Relay::relay`] resolves to its own frame's answer, and one call is outstanding at a
    /// time. A fresh id per call is what lets a `Relay` that hands back some other call's
    /// answer (out of order, or a stale one) be caught as [`Broken`](Failure::Broken) rather
    /// than taken for this call's.
    next_id: RequestId,
}

impl<R: Relay> Client for Relayed<R> {
    fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
        Box::pin(async move {
            let id = self.next_id;
            self.next_id += 1;

            let answer = self
                .relay
                .relay(encode(&Message::Request { id, call })?)
                .await?;

            match decode(&answer)? {
                Message::Response {
                    id: answered,
                    result,
                } if answered == id => match result {
                    Response::Error(error) => Err(Failure::Refused(error)),
                    answer => Ok(answer),
                },
                other => Err(Failure::broken(format!(
                    "request {id} was relayed an answer that is not its response ({:?})",
                    other.id(),
                ))),
            }
        })
    }

    /// `quit` is not relayed: the session is the owner's to end.
    fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            if matches!(notification, Notification::Quit) {
                return Ok(());
            }
            self.relay
                .relay(encode(&Message::Notification(notification))?)
                .await
                .map(drop)
        })
    }
}

fn encode(message: &Message) -> Result<Vec<u8>, Failure> {
    bson::serialize_to_vec(message)
        .map_err(|e| Failure::broken(format!("serializing a relayed message: {e}")))
}

fn decode(frame: &[u8]) -> Result<Message, Failure> {
    bson::deserialize_from_slice(frame)
        .map_err(|e| Failure::broken(format!("reading a relayed message: {e}")))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::protocol::{ExecResp, InitCall, Method};

    /// An owner's channel: answers `exec` with its argv joined, refuses a `cat` of nothing,
    /// and records every method it was handed.
    struct Server(Arc<Mutex<Vec<Method>>>);

    impl Client for Server {
        fn call(&mut self, call: Call) -> BoxFuture<'_, Result<Response, Failure>> {
            self.0.lock().unwrap().push(call.method());
            Box::pin(async move {
                match call {
                    Call::Exec(exec) if exec.cmd == ["cat"] => {
                        Err(Failure::Refused(Error::new(Error::NOT_FOUND, "nothing")))
                    }
                    Call::Exec(exec) => Ok(Response::Exec(ExecResp {
                        stdout: exec.cmd.join(" ").into_bytes(),
                        ..ExecResp::default()
                    })),
                    _ => Err(Failure::broken("unscripted")),
                }
            })
        }

        fn notify(&mut self, notification: Notification) -> BoxFuture<'_, Result<(), Failure>> {
            self.0.lock().unwrap().push(notification.method());
            Box::pin(async { Ok(()) })
        }
    }

    /// Frames to an owner in the same process, as a binding's call into the other
    /// extension would carry them.
    struct Direct(Arc<tokio::sync::Mutex<ConsoleClient>>);

    impl Relay for Direct {
        fn relay(&mut self, frame: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Failure>> {
            Box::pin(async move { self.0.lock().await.relay(&frame).await })
        }
    }

    fn owner() -> (
        Arc<tokio::sync::Mutex<ConsoleClient>>,
        Arc<Mutex<Vec<Method>>>,
    ) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let owner = ConsoleClient {
            client: Box::new(Server(log.clone())),
            mounts: Vec::new(),
        };
        (Arc::new(tokio::sync::Mutex::new(owner)), log)
    }

    #[tokio::test]
    async fn an_attached_console_drives_the_owners_session() {
        let (owner, log) = owner();
        let mut attached = ConsoleClient::attach(Direct(owner.clone()), ["/work"]);

        assert_eq!(
            attached.mounts().collect::<Vec<_>>(),
            [std::path::Path::new("/work")]
        );

        attached.start().await.unwrap();
        let ran = attached.exec(["echo", "hi"], None).await.unwrap();
        assert_eq!(ran.stdout, b"echo hi");

        // A refusal keeps its code across the relay.
        let Err(Failure::Refused(error)) = attached.exec(["cat"], None).await else {
            panic!("a refusal should arrive as one");
        };
        assert_eq!(error.code, Error::NOT_FOUND);

        drop(attached);
        tokio::task::yield_now().await;

        // No `quit`: the session is still the owner's.
        assert_eq!(
            *log.lock().unwrap(),
            [Method::Start, Method::Exec, Method::Exec]
        );
        let ran = owner
            .lock()
            .await
            .exec(["still", "up"], None)
            .await
            .unwrap();
        assert_eq!(ran.stdout, b"still up");
    }

    #[tokio::test]
    async fn the_owner_keeps_the_session_its_own() {
        let (owner, log) = owner();
        let mut owner = owner.lock().await;

        let init = Message::Request {
            id: 7,
            call: Call::Init(InitCall::default()),
        };
        let answer = decode(&owner.relay(&encode(&init).unwrap()).await.unwrap()).unwrap();
        let Message::Response {
            id: 7,
            result: Response::Error(error),
        } = answer
        else {
            panic!("init should be refused, answered under its id: {answer:?}");
        };
        assert_eq!(error.code, Error::INVALID_REQUEST);

        let quit = encode(&Message::Notification(Notification::Quit)).unwrap();
        assert!(owner.relay(&quit).await.unwrap().is_empty());

        // Neither reached the channel.
        assert!(log.lock().unwrap().is_empty());

        // A frame that is no message breaks only the relay, not the owner.
        assert!(matches!(
            owner.relay(b"not bson").await,
            Err(Failure::Broken(_))
        ));
    }

    #[tokio::test]
    async fn a_gone_owner_is_a_broken_channel() {
        struct Gone;

        impl Relay for Gone {
            fn relay(&mut self, _: Vec<u8>) -> BoxFuture<'_, Result<Vec<u8>, Failure>> {
                Box::pin(async { Err(Failure::broken("this console has been closed")) })
            }
        }

        let mut attached = ConsoleClient::attach(Gone, Vec::<PathBuf>::new());
        assert!(matches!(
            attached.exec(["true"], None).await,
            Err(Failure::Broken(_))
        ));
    }
}
