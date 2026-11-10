//! The typed client: each request `PROTOCOL.md` lists, as a call over a [`TransportClient`],
//! made by the module of what it names: [`image`](crate::image),
//! [`volume`](crate::volume) and [`sandbox`](crate::sandbox), and the files of a volume
//! over WebDAV.

use std::{io, path::PathBuf};

use bytes::Bytes;
use futures_util::Stream;
use http::HeaderMap;
use tokio::process::Command;

use crate::{
    Auth, Error,
    image::{
        self, Event, ImageBuildArgs, ImageCommitArgs, ImageDownloadOutput, ImageJob, ImageJobEntry,
        ImageListOutputItem, ImagePruneOutput, ImagePullArgs, ImagePushArgs, ImageUploadOutput,
        Recipe, RegistryAuth, oci,
    },
    sandbox::{
        self, InboundRule, PortInbound, Sandbox, SandboxCreateArgs, SandboxExecArgs,
        SandboxExecItem, SandboxJob, SandboxJobEntry, SandboxPort, SandboxPortOpenArgs,
        SandboxSignalArgs, SandboxStdinArgs,
    },
    transport::{DirEntry, FileRoot, LogEntry, Metadata, TransportClient, webdav},
    volume::{
        self, MountSource, VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs,
        VolumeMountListOutputItem,
    },
};

/// A client of one server, over stdio or HTTP.
pub struct Client {
    pub(crate) inner: TransportClient,
}

impl Client {
    /// Start `command` as the server, and talk to it over its stdin and stdout. Must be called
    /// inside a tokio runtime.
    pub fn spawn(command: impl Into<Command>) -> io::Result<Client> {
        let inner = TransportClient::spawn(command)?;
        Ok(Client { inner })
    }

    /// Reach the server at `url`, its base `http` or `https` URL, sending `auth`, the
    /// credentials it wants, and `headers`, whatever else it asks of each request, with every
    /// request. `Authorization` is `auth`'s, over one given as a header. Refused when a token
    /// would go in the clear: over `http` to any host but this machine.
    pub fn connect(url: &str, auth: Auth, headers: HeaderMap) -> anyhow::Result<Client> {
        let inner = TransportClient::connect(url, auth, headers)?;
        Ok(Client { inner })
    }

    /// Send `auth` with every request from now on, in place of what was given at
    /// [`connect`](Self::connect): a new token, when the server's are short-lived. A forwarder
    /// already open sends it with its next connection. Over stdio, where none is sent, nothing
    /// changes. Refused as at `connect`, the auth unchanged.
    pub fn set_auth(&self, auth: Auth) -> anyhow::Result<()> {
        self.inner.set_auth(auth)
    }

    /// Send `headers` with every request from now on, in place of those given at
    /// [`connect`](Self::connect). A forwarder already open sends them with its next
    /// connection. Over stdio, where none are sent, nothing changes.
    pub fn set_headers(&self, headers: HeaderMap) {
        self.inner.set_headers(headers);
    }

    /// Start building `recipe`, to point `reference` at what it builds. Its log is
    /// [`image_job_log`](Self::image_job_log)'s.
    pub async fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> Result<ImageJob, Error> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        image::client::build(&self.inner, &args).await
    }

    /// Start pulling `reference` from its registry, to point it at what it pulls; without
    /// `auth` the server pulls anonymously. Its log is [`image_job_log`](Self::image_job_log)'s.
    pub async fn pull_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> Result<ImageJob, Error> {
        let args = ImagePullArgs {
            reference: reference.into(),
            auth,
        };
        image::client::pull(&self.inner, &args).await
    }

    /// Start pushing the image `reference` points at to the registry `reference` names;
    /// without `auth` the server pushes anonymously. Its log is
    /// [`image_job_log`](Self::image_job_log)'s.
    pub async fn push_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> Result<ImageJob, Error> {
        let args = ImagePushArgs {
            reference: reference.into(),
            auth,
        };
        image::client::push(&self.inner, &args).await
    }

    /// Every image job the server holds.
    pub async fn list_image_job(&self) -> Result<Vec<ImageJob>, Error> {
        image::client::list_job(&self.inner).await
    }

    /// The image job `id`.
    pub async fn get_image_job(&self, id: impl AsRef<str>) -> Result<ImageJob, Error> {
        image::client::get_job(&self.inner, id.as_ref()).await
    }

    /// Cancel the image job `id` if it runs, and forget it.
    pub async fn remove_image_job(&self, id: impl AsRef<str>) -> Result<(), Error> {
        image::client::remove_job(&self.inner, id.as_ref()).await
    }

    /// The log of the image job `id`, from its start or after its `after`th entry, until its
    /// `end` entry. Dropping it leaves the job as it is.
    pub fn image_job_log(
        &self,
        id: &str,
        after: Option<u64>,
    ) -> impl Stream<Item = Result<LogEntry<ImageJobEntry>, Error>> + Unpin + Send + use<'_> {
        Box::pin(image::client::job_log(&self.inner, id, after))
    }

    /// Every tag on the server's images, as `name:tag`, with the digest it points at.
    pub async fn list_image(&self) -> Result<Vec<ImageListOutputItem>, Error> {
        oci::list(&self.inner).await
    }

    /// Point the tag `target` (`name:tag`) at the image `source` names (`name:tag` or
    /// `name@algorithm:hex`), moving it if it is in use.
    pub async fn tag_image(
        &self,
        source: impl AsRef<str>,
        target: impl AsRef<str>,
    ) -> Result<(), Error> {
        oci::tag(&self.inner, source.as_ref(), target.as_ref()).await
    }

    /// Remove the tag `reference` (`name:tag`), leaving the image it points at.
    pub async fn untag_image(&self, reference: impl AsRef<str>) -> Result<(), Error> {
        oci::untag(&self.inner, reference.as_ref()).await
    }

    /// Remove the image `reference` names (`name:tag` or `name@algorithm:hex`), and every tag on
    /// it in that name.
    pub async fn remove_image(&self, reference: impl AsRef<str>) -> Result<(), Error> {
        oci::remove(&self.inner, reference.as_ref()).await
    }

    /// Write the image `reference` names to `path`, as an OCI image layout in a tar archive,
    /// replacing what is there once all of it is written. The stream is a line of log a blob,
    /// then what it wrote as its last item, an [`Event::Output`] or the error it failed with.
    /// Dropping it drops the download, and writes nothing.
    pub fn download_image(
        &self,
        reference: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> impl Stream<Item = Result<Event<ImageDownloadOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(oci::download(&self.inner, reference.into(), path.into()))
    }

    /// Upload the image in the archive at `path`, an OCI image layout in a tar archive, and
    /// point the tag `reference` (`name:tag`) at it: the archive's only image, or the one it
    /// names by that tag. Blobs the server has already are skipped. The stream is a line of log
    /// a blob, then what it uploaded as its last item, an [`Event::Output`] or the error it
    /// failed with. Dropping it drops the upload, and moves no tag.
    pub fn upload_image(
        &self,
        path: impl Into<PathBuf>,
        reference: impl Into<String>,
    ) -> impl Stream<Item = Result<Event<ImageUploadOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(oci::upload(&self.inner, path.into(), reference.into()))
    }

    /// Start making an image of the sandbox `sandbox` -- its image, with what it has written
    /// as one more layer -- to point `reference` at. Of a running one, it is as it is when the
    /// job starts; of a stopped one, as it was left. Its log is
    /// [`image_job_log`](Self::image_job_log)'s.
    pub async fn commit_image(
        &self,
        sandbox: impl Into<String>,
        reference: impl Into<String>,
    ) -> Result<ImageJob, Error> {
        let args = ImageCommitArgs {
            sandbox: sandbox.into(),
            reference: reference.into(),
        };
        image::client::commit(&self.inner, &args).await
    }

    /// Delete each image no tag points at and no sandbox stands on, and what only such images
    /// held.
    pub async fn prune_image(&self) -> Result<ImagePruneOutput, Error> {
        image::client::prune(&self.inner).await
    }

    /// Every volume the server holds.
    pub async fn list_volume(&self) -> Result<Vec<VolumeListOutputItem>, Error> {
        volume::client::list(&self.inner).await
    }

    /// Keep a new, empty volume under `name`; without one the server picks it.
    pub async fn create_volume(
        &self,
        name: Option<impl Into<String>>,
    ) -> Result<VolumeListOutputItem, Error> {
        let args = VolumeCreateArgs {
            name: name.map(Into::into),
        };
        volume::client::create(&self.inner, &args).await
    }

    /// The volume `name`.
    pub async fn get_volume(&self, name: impl AsRef<str>) -> Result<VolumeListOutputItem, Error> {
        volume::client::get(&self.inner, name.as_ref()).await
    }

    /// Delete the volume `name` and everything in it.
    pub async fn remove_volume(&self, name: impl AsRef<str>) -> Result<(), Error> {
        volume::client::remove(&self.inner, name.as_ref()).await
    }

    /// The mounts in `volume`, without their credentials.
    pub async fn list_volume_mount(
        &self,
        volume: impl AsRef<str>,
    ) -> Result<Vec<VolumeMountListOutputItem>, Error> {
        volume::client::list_mount(&self.inner, volume.as_ref()).await
    }

    /// Mount `source` at `path` in `volume`, a directory the mount creates.
    pub async fn create_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
        source: MountSource,
    ) -> Result<VolumeMountListOutputItem, Error> {
        let args = VolumeMountCreateArgs { source };
        volume::client::create_mount(&self.inner, volume.as_ref(), path.as_ref(), &args).await
    }

    /// Take away the mount at `path` in `volume`, leaving its source as it is.
    pub async fn remove_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
    ) -> Result<(), Error> {
        volume::client::remove_mount(&self.inner, volume.as_ref(), path.as_ref()).await
    }

    /// What is at `path` in `root`.
    pub async fn stat_file(&self, root: &FileRoot, path: &str) -> Result<Metadata, Error> {
        webdav::client::stat(&self.inner, root, path).await
    }

    /// Whether anything is at `path` in `root`.
    pub async fn exists(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::exists(&self.inner, root, path).await
    }

    /// Whether a file is at `path` in `root`.
    pub async fn is_file(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::is_file(&self.inner, root, path).await
    }

    /// Whether a directory is at `path` in `root`.
    pub async fn is_dir(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::is_dir(&self.inner, root, path).await
    }

    /// The entries of the directory at `path` in `root`.
    pub async fn read_dir(&self, root: &FileRoot, path: &str) -> Result<Vec<DirEntry>, Error> {
        webdav::client::read_dir(&self.inner, root, path).await
    }

    /// The file at `path` in `root`, from `offset`, `length` bytes of it or all that is left.
    pub async fn read_file(
        &self,
        root: &FileRoot,
        path: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Bytes, Error> {
        webdav::client::read(&self.inner, root, path, offset, length).await
    }

    /// The whole file at `path` in `root` as text, which it has to be: UTF-8.
    pub async fn read_text(&self, root: &FileRoot, path: &str) -> Result<String, Error> {
        webdav::client::read_text(&self.inner, root, path).await
    }

    /// Write `content` as the whole file at `path` in `root`, creating it if it does not
    /// exist. Its parent directory must exist. With `if_match`, the `etag` a [`Metadata`] of
    /// the file gave, the write goes only while the file still has it, and is refused with a
    /// `412` otherwise.
    pub async fn write_file(
        &self,
        root: &FileRoot,
        path: &str,
        content: impl Into<Bytes>,
        if_match: Option<&str>,
    ) -> Result<(), Error> {
        webdav::client::write(&self.inner, root, path, content, if_match).await
    }

    /// Create the directory at `path` in `root`, whose parent must exist.
    pub async fn create_dir(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        webdav::client::create_dir(&self.inner, root, path).await
    }

    /// Have the directory at `path` in `root`, as `mkdir -p`: its parents are created as
    /// needed, and one there already is left as it is. Refused with a `405` when a file is at
    /// `path`.
    pub async fn create_dir_all(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        webdav::client::create_dir_all(&self.inner, root, path).await
    }

    /// Remove what is at `path` in `root`: a directory with everything in it. With
    /// `missing_ok`, nothing being there is fine, as `rm -f`; without it, a `404`.
    pub async fn remove_file(
        &self,
        root: &FileRoot,
        path: &str,
        missing_ok: bool,
    ) -> Result<(), Error> {
        webdav::client::remove(&self.inner, root, path, missing_ok).await
    }

    /// Move what is at `from` in `root` to `to` in it, replacing what is at `to` if
    /// `overwrite`, and refused if not and something is there.
    pub async fn rename_file(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        webdav::client::rename(&self.inner, root, from, to, overwrite).await
    }

    /// Copy what is at `from` in `root` to `to` in it, a directory with everything in it, as
    /// [`rename_file`](Self::rename_file) replaces what is at `to`.
    pub async fn copy_file(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        webdav::client::copy(&self.inner, root, from, to, overwrite).await
    }

    /// Every sandbox the server holds.
    pub async fn list_sandbox(&self) -> Result<Vec<Sandbox>, Error> {
        sandbox::client::list(&self.inner).await
    }

    /// Make a sandbox as `args` says; see [`SandboxCreateArgs::new`]. Its ports are answered
    /// here only.
    pub async fn create_sandbox(&self, args: &SandboxCreateArgs) -> Result<Sandbox, Error> {
        sandbox::client::create(&self.inner, args).await
    }

    /// The sandbox `id`, without its ports.
    pub async fn get_sandbox(&self, id: impl AsRef<str>) -> Result<Sandbox, Error> {
        sandbox::client::get(&self.inner, id.as_ref()).await
    }

    /// The sandbox named `name`, without its ports, if one is.
    pub async fn find_sandbox(&self, name: impl AsRef<str>) -> Result<Option<Sandbox>, Error> {
        sandbox::client::find(&self.inner, name.as_ref()).await
    }

    /// Start the sandbox `id` if it is stopped -- its VM booted over its disk as it was left --
    /// and answer it once it runs, with where its ports are now. A job started in a stopped
    /// sandbox starts it too.
    pub async fn start_sandbox(&self, id: impl AsRef<str>) -> Result<Sandbox, Error> {
        sandbox::client::start(&self.inner, id.as_ref()).await
    }

    /// Stop the sandbox `id` if it runs: its jobs end, and its disk is kept -- or, for an
    /// ephemeral one, deleted with it. Answered once it has stopped.
    pub async fn stop_sandbox(&self, id: impl AsRef<str>) -> Result<Sandbox, Error> {
        sandbox::client::stop(&self.inner, id.as_ref()).await
    }

    /// Delete the sandbox `id`: stopped first if it runs, and its disk removed. Its volumes stay.
    pub async fn remove_sandbox(&self, id: impl AsRef<str>) -> Result<(), Error> {
        sandbox::client::remove(&self.inner, id.as_ref()).await
    }

    /// Open the TCP port `port` in the sandbox `id`, as those it was made with are, and answer
    /// where it is reached from: as secret as a port's URL is. A program in it that listens on
    /// `port` already is reached once it listens again.
    pub async fn open_sandbox_port(
        &self,
        id: impl AsRef<str>,
        port: u16,
    ) -> Result<SandboxPort, Error> {
        self.open_sandbox_port_with(id, port, &SandboxPortOpenArgs::default())
            .await
    }

    /// Close the port `port` in the sandbox `id`: its URL reaches it no more, and it is not
    /// opened again as the sandbox starts. One not open is refused `404`.
    pub async fn close_sandbox_port(&self, id: impl AsRef<str>, port: u16) -> Result<(), Error> {
        sandbox::client::close_port(&self.inner, id.as_ref(), port).await
    }

    /// Open the port `port` in the sandbox `id` as `args` say -- what it is for, and how it is
    /// reached, TCP or HTTP -- and answer where it is reached from.
    pub async fn open_sandbox_port_with(
        &self,
        id: impl AsRef<str>,
        port: u16,
        args: &SandboxPortOpenArgs,
    ) -> Result<SandboxPort, Error> {
        sandbox::client::open_port(&self.inner, id.as_ref(), port, args).await
    }

    /// The inbound rules of the port `port` in the sandbox `id`, and its public address if it
    /// has one: virtx cloud's, which a self-hosted server does not answer. Its desktop's port
    /// is refused `409`, `not_public`; one not open, `404`.
    pub async fn get_sandbox_port_inbound(
        &self,
        id: impl AsRef<str>,
        port: u16,
    ) -> Result<PortInbound, Error> {
        sandbox::client::port_inbound(&self.inner, id.as_ref(), port).await
    }

    /// Make the inbound rules of the port `port` in the sandbox `id` `rules`, all of them -- none
    /// lets in no one -- and answer them as the server keeps them, as
    /// [`get_sandbox_port_inbound`](Self::get_sandbox_port_inbound) does. A source that is not
    /// an address or a CIDR is refused `400`, `invalid`.
    pub async fn set_sandbox_port_inbound(
        &self,
        id: impl AsRef<str>,
        port: u16,
        rules: &[InboundRule],
    ) -> Result<PortInbound, Error> {
        sandbox::client::set_port_inbound(&self.inner, id.as_ref(), port, rules).await
    }

    /// Every job in the sandbox `id`.
    pub async fn list_sandbox_job(&self, id: impl AsRef<str>) -> Result<Vec<SandboxJob>, Error> {
        sandbox::client::list_job(&self.inner, id.as_ref()).await
    }

    /// Start a process in the sandbox `id` as `args` says; see [`SandboxExecArgs::new`]. Its
    /// output and how it ended are [`sandbox_job_log`](Self::sandbox_job_log)'s.
    pub async fn exec_sandbox(
        &self,
        id: impl AsRef<str>,
        args: &SandboxExecArgs,
    ) -> Result<SandboxJob, Error> {
        sandbox::client::exec(&self.inner, id.as_ref(), args).await
    }

    /// Start a process in the sandbox `id` as `args` says, and read its log: its job, once,
    /// first, then each entry until its `end`. One request where the server answers an exec
    /// with its log (virtx cloud's), two elsewhere. Dropping it leaves the job running.
    pub fn exec_sandbox_with_log(
        &self,
        id: &str,
        args: &SandboxExecArgs,
    ) -> impl Stream<Item = Result<SandboxExecItem, Error>> + Unpin + Send + use<'_> {
        Box::pin(sandbox::client::exec_log(&self.inner, id, args))
    }

    /// The job `job` in the sandbox `id`.
    pub async fn get_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
    ) -> Result<SandboxJob, Error> {
        sandbox::client::get_job(&self.inner, id.as_ref(), job.as_ref()).await
    }

    /// Cancel the job `job` in the sandbox `id` if it runs, killing its process, and forget it.
    pub async fn remove_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
    ) -> Result<(), Error> {
        sandbox::client::remove_job(&self.inner, id.as_ref(), job.as_ref()).await
    }

    /// Send `signal`, such as `SIGTERM`, to the process of the job `job` in the sandbox `id`
    /// and every process it started.
    pub async fn signal_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        signal: impl Into<String>,
    ) -> Result<(), Error> {
        let args = SandboxSignalArgs {
            signal: signal.into(),
        };
        sandbox::client::signal_job(&self.inner, id.as_ref(), job.as_ref(), &args).await
    }

    /// Write `data` to the stdin of the process of the job `job` in the sandbox `id`, then
    /// close it if `eof`. Answers once it is written: a caller writes again only then, so the
    /// writes land in order.
    pub async fn write_sandbox_job_stdin(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        data: Option<impl Into<String>>,
        eof: bool,
    ) -> Result<(), Error> {
        let args = SandboxStdinArgs {
            data: data.map(Into::into),
            eof,
        };
        sandbox::client::write_job_stdin(&self.inner, id.as_ref(), job.as_ref(), &args).await
    }

    /// The log of the job `job` in the sandbox `id`, its process's output, from its start or
    /// after its `after`th entry, until its `end` entry. Dropping it leaves the job as it is.
    pub fn sandbox_job_log(
        &self,
        id: &str,
        job: &str,
        after: Option<u64>,
    ) -> impl Stream<Item = Result<LogEntry<SandboxJobEntry>, Error>> + Unpin + Send + use<'_> {
        Box::pin(sandbox::client::job_log(&self.inner, id, job, after))
    }

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    //! The typed client against the [`testing`](crate::testing) server, over each transport.

    use futures_util::{StreamExt, TryStreamExt};

    use super::*;
    use crate::{
        image::{ImageJobEnd, ImageJobKind},
        sandbox::{SandboxJobEnd, SandboxState},
        transport::tests::clients,
        volume::MountType,
    };

    /// A client of a test server of its own over each transport.
    async fn each() -> [Client; 2] {
        clients().await.map(|inner| Client { inner })
    }

    /// The status `result` was refused with, and its message.
    fn refusal<T: std::fmt::Debug>(result: Result<T, Error>) -> (u16, String) {
        match result {
            Err(Error::Status(status, message)) => (status, message),
            other => panic!("not a refusal: {other:?}"),
        }
    }

    /// The entries of the job `job`'s log in `sandbox`, after its `after`th.
    async fn sandbox_log(
        client: &Client,
        sandbox: &str,
        job: &str,
        after: Option<u64>,
    ) -> Vec<LogEntry<SandboxJobEntry>> {
        client
            .sandbox_job_log(sandbox, job, after)
            .try_collect()
            .await
            .unwrap()
    }

    fn entry(id: u64, entry: SandboxJobEntry) -> LogEntry<SandboxJobEntry> {
        LogEntry { id, entry }
    }

    fn end(code: i32) -> SandboxJobEntry {
        SandboxJobEntry::End(SandboxJobEnd { code })
    }

    #[tokio::test]
    async fn a_name_that_needs_encoding_names_its_resource() {
        for client in each().await {
            let name = "한글 & spaces #1";
            client.create_volume(Some(name)).await.unwrap();
            assert_eq!(client.get_volume(name).await.unwrap().name, name);
            let root = FileRoot::Volume(name.into());
            client.write_file(&root, "a", "x", None).await.unwrap();
            assert_eq!(client.read_text(&root, "a").await.unwrap(), "x");
            client.remove_volume(name).await.unwrap();
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_volume_and_its_mounts_are_made_read_and_removed() {
        for client in each().await {
            let volume = client.create_volume(Some("data")).await.unwrap();
            assert_eq!(volume.name, "data");
            assert!(client.list_volume().await.unwrap().contains(&volume));

            let source = MountSource::Notion {
                api_key: "key".into(),
            };
            let mount = client
                .create_volume_mount("data", "docs/notion", source.clone())
                .await
                .unwrap();
            assert_eq!(
                mount,
                VolumeMountListOutputItem {
                    path: "docs/notion".into(),
                    mount_type: MountType::Notion,
                }
            );
            assert_eq!(client.list_volume_mount("data").await.unwrap(), [mount]);
            let again = client.create_volume_mount("data", "docs/notion", source);
            assert_eq!(refusal(again.await).0, 409);
            client
                .remove_volume_mount("data", "docs/notion")
                .await
                .unwrap();
            assert!(client.list_volume_mount("data").await.unwrap().is_empty());

            client.remove_volume("data").await.unwrap();
            assert_eq!(refusal(client.get_volume("data").await).0, 404);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_sandbox_stops_starts_again_and_is_found_by_its_name() {
        for client in each().await {
            let mut args = SandboxCreateArgs::new("alpine:3.20");
            args.name = Some("work".into());
            args.ports = vec![3000.into()];
            let sandbox = client.create_sandbox(&args).await.unwrap();
            assert_eq!(sandbox.name.as_deref(), Some("work"));
            assert_eq!(sandbox.state, SandboxState::Running);
            assert!(!sandbox.ephemeral);
            assert_eq!(sandbox.ports.len(), 1);
            assert_eq!(sandbox.ports[0].port, 3000);

            // Ports are answered at making and starting only.
            let read = client.get_sandbox(&sandbox.id).await.unwrap();
            assert!(read.ports.is_empty());
            assert!(client.list_sandbox().await.unwrap().contains(&read));
            assert_eq!(client.find_sandbox("work").await.unwrap(), Some(read));
            assert_eq!(client.find_sandbox("nobody").await.unwrap(), None);
            assert_eq!(refusal(client.create_sandbox(&args).await).0, 409);

            let stopped = client.stop_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(stopped.state, SandboxState::Stopped);
            let started = client.start_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(started.state, SandboxState::Running);
            assert_eq!(started.ports, sandbox.ports);

            // A port opened after is kept as those it was made with are.
            let opened = client.open_sandbox_port(&sandbox.id, 8080).await.unwrap();
            assert_eq!(opened.port, 8080);
            let again = client.open_sandbox_port(&sandbox.id, 8080).await.unwrap();
            assert_eq!(again, opened);
            let started = client.start_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(started.ports[1], opened);
            // One for a browser, named for what it is.
            let web = client
                .open_sandbox_port_with(
                    &sandbox.id,
                    5173,
                    &SandboxPortOpenArgs {
                        label: Some("dev server".into()),
                        protocol: Some(crate::PortProtocol::Http),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                (web.label.as_deref(), web.protocol),
                (Some("dev server"), crate::PortProtocol::Http)
            );
            // Closed: gone from what starting it answers; closed again, not found.
            client.close_sandbox_port(&sandbox.id, 5173).await.unwrap();
            let started = client.start_sandbox(&sandbox.id).await.unwrap();
            assert!(
                started.ports.iter().all(|p| p.port != 5173),
                "{:?}",
                started.ports
            );
            assert_eq!(
                refusal(client.close_sandbox_port(&sandbox.id, 5173).await).0,
                404
            );
            let missing = client.open_sandbox_port("nobody", 8080).await;
            assert_eq!(refusal(missing).0, 404);

            // A job started in a stopped one starts it.
            client.stop_sandbox(&sandbox.id).await.unwrap();
            client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "hi"]))
                .await
                .unwrap();
            let read = client.get_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(read.state, SandboxState::Running);

            client.remove_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(refusal(client.get_sandbox(&sandbox.id).await).0, 404);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_ports_inbound_rules_are_set_as_it_opens_and_after() {
        for client in each().await {
            let office = InboundRule::new("203.0.113.7").description("office");
            let mut ssh = crate::PortSpec::new(22);
            ssh.inbound = Some(vec![office]);
            let args = SandboxCreateArgs::new("alpine:3.20")
                .port(ssh)
                .port(8080)
                .port(5900);
            let sandbox = client.create_sandbox(&args).await.unwrap();

            // Made with them: each source a CIDR.
            let read = client
                .get_sandbox_port_inbound(&sandbox.id, 22)
                .await
                .unwrap();
            let office = InboundRule::new("203.0.113.7/32").description("office");
            assert_eq!(read.rules, std::slice::from_ref(&office));
            assert_eq!((read.port, read.address), (22, None));
            // Made without: none.
            let read = client
                .get_sandbox_port_inbound(&sandbox.id, 8080)
                .await
                .unwrap();
            assert!(read.rules.is_empty());

            // Set: all of them replaced.
            let lan = [InboundRule::new("10.0.0.0/8")];
            let set = client
                .set_sandbox_port_inbound(&sandbox.id, 22, &lan)
                .await
                .unwrap();
            assert_eq!(set.rules, lan);
            let read = client.get_sandbox_port_inbound(&sandbox.id, 22).await;
            assert_eq!(read.unwrap(), set);

            // An HTTP port opened with them keeps them.
            let web = SandboxPortOpenArgs {
                protocol: Some(crate::PortProtocol::Http),
                inbound: Some(vec![office.clone()]),
                ..Default::default()
            };
            client
                .open_sandbox_port_with(&sandbox.id, 5173, &web)
                .await
                .unwrap();
            let read = client.get_sandbox_port_inbound(&sandbox.id, 5173).await;
            assert_eq!(read.unwrap().rules, [office]);

            // Refused: the desktop's, a port not open, and a source not a CIDR.
            let desktop = client.set_sandbox_port_inbound(&sandbox.id, 5900, &lan);
            let refused = desktop.await.unwrap_err();
            assert_eq!(refused.code(), Some("not_public"));
            let closed = client.get_sandbox_port_inbound(&sandbox.id, 9000).await;
            assert_eq!(closed.unwrap_err().code(), Some("not_found"));
            let nowhere = [InboundRule::new("nowhere")];
            let invalid = client.set_sandbox_port_inbound(&sandbox.id, 22, &nowhere);
            assert_eq!(invalid.await.unwrap_err().code(), Some("invalid"));

            client.remove_sandbox(&sandbox.id).await.unwrap();
            client.close().await;
        }
    }

    #[tokio::test]
    async fn an_ephemeral_sandbox_goes_when_it_stops() {
        for client in each().await {
            let mut args = SandboxCreateArgs::new("alpine:3.20");
            args.ephemeral = true;
            args.auto_delete_days = Some(None);
            let sandbox = client.create_sandbox(&args).await.unwrap();
            assert!(sandbox.ephemeral);
            assert_eq!(sandbox.auto_delete_days, None);
            client.stop_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(refusal(client.get_sandbox(&sandbox.id).await).0, 404);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_sandbox_is_committed_to_an_image() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let job = client.commit_image(&sandbox.id, "mine:v1").await.unwrap();
            assert_eq!(job.kind, ImageJobKind::Commit);
            assert_eq!(job.reference, "mine:v1");
            let ended = client.get_image_job(&job.id).await.unwrap();
            assert_eq!(ended.digest.as_deref(), Some("sha256:0"));
            let refused = client.commit_image("nobody", "mine:v2").await.unwrap();
            let ended = client.get_image_job(&refused.id).await.unwrap();
            assert!(ended.error.is_some());
            client.remove_sandbox(&sandbox.id).await.unwrap();
            client.close().await;
        }
    }

    #[tokio::test]
    async fn an_exec_read_with_its_log_is_its_job_then_its_entries() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let args = SandboxExecArgs::new(["echo", "hi", "there"]);
            let items: Vec<_> = client
                .exec_sandbox_with_log(&sandbox.id, &args)
                .try_collect()
                .await
                .unwrap();
            let [SandboxExecItem::Job(job), rest @ ..] = &items[..] else {
                panic!("not its job first: {items:?}");
            };
            assert_eq!(job.cmd, ["echo", "hi", "there"]);
            assert_eq!(
                rest,
                [
                    SandboxExecItem::Log(entry(1, SandboxJobEntry::Stdout("hi there".into()))),
                    SandboxExecItem::Log(entry(2, end(0))),
                ]
            );
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_sandbox_s_files_are_its_own_and_go_with_it() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let root = FileRoot::Sandbox(sandbox.id.clone());
            client.create_dir_all(&root, "work/dir").await.unwrap();
            client
                .write_file(&root, "work/dir/a.txt", "hello", None)
                .await
                .unwrap();
            assert_eq!(
                client.read_text(&root, "work/dir/a.txt").await.unwrap(),
                "hello"
            );
            let entries = client.read_dir(&root, "work/dir").await.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "a.txt");

            client.remove_sandbox(&sandbox.id).await.unwrap();
            assert_eq!(refusal(client.read_dir(&root, "").await).0, 404);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_process_s_log_is_its_output_then_how_it_ended() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let job = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "hi", "there"]))
                .await
                .unwrap();
            assert_eq!(job.kind, sandbox::SandboxJobKind::Exec);
            assert_eq!(job.cmd, ["echo", "hi", "there"]);
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &job.id, None).await,
                [
                    entry(1, SandboxJobEntry::Stdout("hi there".into())),
                    entry(2, end(0)),
                ]
            );
            let ended = client.get_sandbox_job(&sandbox.id, &job.id).await.unwrap();
            assert!(!ended.running);
            assert_eq!(ended.code, Some(0));

            let failed = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["fail"]))
                .await
                .unwrap();
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &failed.id, None).await,
                [
                    entry(1, SandboxJobEntry::Stderr("failed".into())),
                    entry(2, end(1)),
                ]
            );
            let jobs = client.list_sandbox_job(&sandbox.id).await.unwrap();
            assert_eq!(jobs.len(), 2);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_log_is_read_again_from_after_any_entry() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let job = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["lines", "3"]))
                .await
                .unwrap();
            let whole = sandbox_log(&client, &sandbox.id, &job.id, None).await;
            assert_eq!(whole.len(), 4);
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &job.id, None).await,
                whole
            );
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &job.id, Some(2)).await,
                whole[2..]
            );
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_log_past_the_limit_is_truncated_but_keeps_its_end() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let job = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["lines", "100"]))
                .await
                .unwrap();
            let log = sandbox_log(&client, &sandbox.id, &job.id, None).await;
            let limit = crate::testing::LOG_LIMIT;
            assert_eq!(log.len(), limit + 2);
            assert_eq!(log[limit].entry, SandboxJobEntry::Truncated {});
            assert_eq!(log[limit + 1].entry, end(0));
            client.close().await;
        }
    }

    #[tokio::test]
    async fn stdin_written_in_order_comes_back_and_its_end_ends_the_process() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let mut args = SandboxExecArgs::new(["cat"]);
            args.stdin = true;
            let job = client.exec_sandbox(&sandbox.id, &args).await.unwrap();
            // A reader from the start sees what is written after it started reading.
            let mut log = client.sandbox_job_log(&sandbox.id, &job.id, None);
            client
                .write_sandbox_job_stdin(&sandbox.id, &job.id, Some("a\nb\n"), false)
                .await
                .unwrap();
            assert_eq!(
                log.next().await.unwrap().unwrap(),
                entry(1, SandboxJobEntry::Stdout("a".into()))
            );
            client
                .write_sandbox_job_stdin(&sandbox.id, &job.id, Some("c"), true)
                .await
                .unwrap();
            let rest: Vec<_> = log.try_collect().await.unwrap();
            assert_eq!(
                rest,
                [
                    entry(2, SandboxJobEntry::Stdout("b".into())),
                    entry(3, SandboxJobEntry::Stdout("c".into())),
                    entry(4, end(0)),
                ]
            );
            let closed = client
                .write_sandbox_job_stdin(&sandbox.id, &job.id, Some("d"), false)
                .await;
            assert_eq!(refusal(closed).0, 409);

            let quiet = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo"]))
                .await
                .unwrap();
            let unasked = client
                .write_sandbox_job_stdin(&sandbox.id, &quiet.id, Some("x"), false)
                .await;
            assert_eq!(refusal(unasked).0, 409);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_signal_or_a_cancellation_ends_a_process() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let sleep = SandboxExecArgs::new(["sleep"]);
            let job = client.exec_sandbox(&sandbox.id, &sleep).await.unwrap();
            assert!(job.running);
            client
                .signal_sandbox_job(&sandbox.id, &job.id, "SIGTERM")
                .await
                .unwrap();
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &job.id, None).await,
                [entry(1, end(143))]
            );
            let again = client.signal_sandbox_job(&sandbox.id, &job.id, "SIGTERM");
            assert_eq!(refusal(again.await).0, 409);

            // Removed while read, its log ends where it is.
            let job = client.exec_sandbox(&sandbox.id, &sleep).await.unwrap();
            let log = client.sandbox_job_log(&sandbox.id, &job.id, None);
            let remove = async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                client.remove_sandbox_job(&sandbox.id, &job.id).await
            };
            let (read, removed) = tokio::join!(log.try_collect::<Vec<_>>(), remove);
            removed.unwrap();
            assert!(read.unwrap().len() <= 1);
            assert_eq!(
                refusal(client.get_sandbox_job(&sandbox.id, &job.id).await).0,
                404
            );

            let mut timed = sleep.clone();
            timed.timeout_ms = Some(10);
            let job = client.exec_sandbox(&sandbox.id, &timed).await.unwrap();
            assert_eq!(
                sandbox_log(&client, &sandbox.id, &job.id, None).await,
                [entry(1, end(137))]
            );
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_command_that_cannot_start_makes_no_job() {
        for client in each().await {
            let sandbox = client
                .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
                .await
                .unwrap();
            let refused = client
                .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["nonexistent"]))
                .await;
            let (status, message) = refusal(refused);
            assert_eq!(status, 422);
            assert!(message.starts_with("not_started: "), "{message}");
            assert!(
                client
                    .list_sandbox_job(&sandbox.id)
                    .await
                    .unwrap()
                    .is_empty()
            );
            client.close().await;
        }
    }

    /// The entries of the image job `id`'s log.
    async fn image_log(client: &Client, id: &str) -> Vec<ImageJobEntry> {
        client
            .image_job_log(id, None)
            .map_ok(|entry| entry.entry)
            .try_collect()
            .await
            .unwrap()
    }

    fn digest(digest: &str) -> ImageJobEntry {
        ImageJobEntry::End(ImageJobEnd {
            digest: Some(digest.into()),
            error: None,
        })
    }

    #[tokio::test]
    async fn an_image_job_logs_then_ends_with_what_it_did() {
        for client in each().await {
            let build = client
                .build_image(Recipe::new("alpine:3.20"), "jq:latest")
                .await
                .unwrap();
            assert_eq!(build.kind, ImageJobKind::Build);
            assert_eq!(build.reference, "jq:latest");
            assert_eq!(
                image_log(&client, &build.id).await,
                [
                    ImageJobEntry::Log("pulling alpine:3.20".into()),
                    digest("sha256:0")
                ]
            );
            let read = client.get_image_job(&build.id).await.unwrap();
            assert!(!read.running);
            assert_eq!(read.digest.as_deref(), Some("sha256:0"));

            let pull = client.pull_image("missing", None).await.unwrap();
            assert_eq!(pull.kind, ImageJobKind::Pull);
            assert_eq!(pull.error.as_deref(), Some("no image missing"));
            let log = image_log(&client, &pull.id).await;
            assert!(matches!(
                log.last(),
                Some(ImageJobEntry::End(ImageJobEnd { error: Some(_), .. }))
            ));

            let auth = RegistryAuth::Bearer {
                token: "token".into(),
            };
            let push = client
                .push_image("ghcr.io/owner/jq:latest", Some(auth))
                .await
                .unwrap();
            assert_eq!(
                image_log(&client, &push.id).await,
                [
                    ImageJobEntry::Log("pushing ghcr.io/owner/jq:latest as bearer".into()),
                    digest("sha256:0")
                ]
            );
            assert_eq!(client.list_image_job().await.unwrap().len(), 3);

            let pruned = client.prune_image().await.unwrap();
            assert_eq!(pruned.images, ["sha256:0"]);
            assert_eq!(pruned.reclaimed, 1024);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn an_image_job_cancelled_is_gone() {
        for client in each().await {
            let job = client.pull_image("hang", None).await.unwrap();
            assert!(job.running);
            let mut log = client.image_job_log(&job.id, None);
            assert_eq!(
                log.next().await.unwrap().unwrap().entry,
                ImageJobEntry::Log("pulling hang".into())
            );
            client.remove_image_job(&job.id).await.unwrap();
            // What was there when it went, and no more.
            while let Some(entry) = log.next().await {
                entry.unwrap();
            }
            drop(log);
            assert_eq!(refusal(client.get_image_job(&job.id).await).0, 404);
            client.close().await;
        }
    }
}

#[cfg(feature = "ffi")]
pub(crate) mod ffi {
    //! [`Client`] behind the C ABI: opening one, sharing it by reference count, and its methods,
    //! each of which starts the request and hands C its [`VirtxFuture`] or [`VirtxStream`].

    use std::{
        ffi::{CStr, c_char, c_int},
        slice,
    };

    use bytes::Bytes;
    use futures_util::StreamExt;
    use http::{HeaderMap, HeaderName, HeaderValue};
    use serde::Deserialize;
    use tokio::process::Command;

    use super::Client;
    use crate::{
        Auth,
        ffi::{
            Failure, Runtime, VIRTX_OK, VirtxBytes, VirtxFuture, VirtxStream, encode, failure,
            json, release, retain, share, write,
        },
        image::{
            ImageBuildArgs, ImageCommitArgs, ImageDownloadArgs, ImagePullArgs, ImagePushArgs,
            ImageRemoveArgs, ImageTagArgs, ImageUntagArgs, ImageUploadArgs,
        },
        sandbox::{InboundRule, SandboxCreateArgs, SandboxExecArgs},
        transport::{FileMoveArgs, FilePathArgs, FileReadArgs, FileRemoveArgs, FileWriteArgs},
        volume::{MountSource, VolumeCreateArgs},
    };

    /// What names a volume or a sandbox: `{"name": "..."}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Name {
        name: String,
    }

    /// What names a sandbox or an image job: `{"id": "..."}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Id {
        id: String,
    }

    /// What reading an image job's log takes: `{"id": "...", "after": n}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ImageJobLogArgs {
        id: String,
        after: Option<u64>,
    }

    /// What names a volume's mounts, or one of them, and what mounting one takes:
    /// `{"volume": "...", "path": "...", "source": {...}}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Mount {
        volume: String,
        #[serde(default)]
        path: String,
        source: Option<MountSource>,
    }

    /// What starting a process takes: its sandbox beside what `SandboxExecArgs` holds.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Exec {
        sandbox: String,
        cmd: Vec<String>,
        #[serde(default)]
        stdin: bool,
        timeout_ms: Option<u64>,
    }

    /// What names a port in a sandbox: `{"id": "...", "port": n}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct IdPort {
        id: String,
        port: u16,
    }

    /// What names a sandbox's jobs, or one of them, and what each request to one takes beside:
    /// `{"sandbox": "...", "job": "...", "signal": "...", "data": "...", "eof": bool,
    /// "after": n}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Job {
        sandbox: String,
        job: Option<String>,
        signal: Option<String>,
        data: Option<String>,
        #[serde(default)]
        eof: bool,
        after: Option<u64>,
    }

    /// A `Client`, as C holds it.
    pub struct VirtxClient(pub(crate) Runtime<Client>);

    /// Start `program` with `args_len` arguments from `args` as the server, over its stdin and
    /// stdout, and write the client, held once, to `client`. A null `program` is `virtx-stdio`,
    /// the server of this library's version, fetched first if this host has none -- see
    /// `virtx_ensure_virtx`, which does that fetch alone.
    ///
    /// # Safety
    ///
    /// `program` is null or NUL-terminated UTF-8, as each of the `args_len` pointers in `args`
    /// is. `client` is writable. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_spawn(
        program: *const c_char,
        args: *const *const c_char,
        args_len: usize,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = (|| {
            let utf8 = |ptr| {
                unsafe { CStr::from_ptr(ptr) }
                    .to_str()
                    .map_err(|e| e.to_string())
            };
            let program = if program.is_null() {
                None
            } else {
                Some(utf8(program)?.to_string())
            };
            let args = (0..args_len)
                .map(|i| utf8(unsafe { *args.add(i) }).map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?;
            Runtime::open(|| {
                let mut command = match program {
                    Some(program) => Command::new(program),
                    // On the runtime being opened, which this thread is not a worker of.
                    None => Command::new(
                        tokio::runtime::Handle::current()
                            .block_on(crate::ensure_virtx())
                            .map_err(|e| format!("fetching the server: {e:#}"))?,
                    ),
                };
                command.args(args);
                Client::spawn(command).map_err(|e| format!("starting the server: {e}"))
            })
        })();
        unsafe { share(opened.map(VirtxClient), client, error) }
    }

    /// Fetch `virtx-stdio`, the server of this library's version, into this host's cache if it
    /// is not there, and write its path, as a JSON string, to `out`: what `virtx_client_spawn`
    /// given a null `program` starts. Blocks for the fetch. `VIRTX_OK`, or `VIRTX_ERROR` with
    /// why written to `out`.
    ///
    /// The server is kept one directory per version, so a library that is upgraded fetches
    /// its own. `$VIRTX_STDIO` names a server to use as it is; `$VIRTX_STDIO_VERSION` another
    /// version; `$VIRTX_DIST_URL` where releases are fetched from; `$VIRTX_HOME` the cache.
    ///
    /// # Safety
    ///
    /// `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_ensure_virtx(out: *mut VirtxBytes) -> c_int {
        let fetched = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
            .and_then(|runtime| {
                runtime
                    .block_on(crate::ensure_virtx())
                    .map_err(|e| format!("{e:#}"))
            })
            .map(|path| path.to_string_lossy().into_owned());
        let (status, bytes) = encode(fetched);
        unsafe { write(out, bytes) };
        status
    }

    /// `ptr` as a `&str`.
    ///
    /// # Safety
    ///
    /// `ptr` is NUL-terminated UTF-8, kept for `'a`.
    unsafe fn utf8<'a>(ptr: *const c_char) -> Result<&'a str, String> {
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .map_err(|e| e.to_string())
    }

    /// The `headers_len` headers in `headers`, each its name then its value, so `2 *
    /// headers_len` strings.
    ///
    /// # Safety
    ///
    /// Each of the `2 * headers_len` pointers in `headers` is NUL-terminated UTF-8.
    unsafe fn header_map(
        headers: *const *const c_char,
        headers_len: usize,
    ) -> Result<HeaderMap, String> {
        let mut map = HeaderMap::with_capacity(headers_len);
        for i in 0..headers_len {
            let name = unsafe { utf8(*headers.add(2 * i)) }?;
            let value = unsafe { utf8(*headers.add(2 * i + 1)) }?;
            map.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| format!("header {name}: {e}"))?,
                HeaderValue::from_str(value).map_err(|e| format!("header {name}: {e}"))?,
            );
        }
        Ok(map)
    }

    /// The `auth_len` bytes at `auth`: `{"type": "bearer", "token": "..."}`, or null for none.
    ///
    /// # Safety
    ///
    /// `auth` is null, or `auth_len` readable bytes.
    unsafe fn auth(auth: *const u8, auth_len: usize) -> Result<Auth, String> {
        if auth.is_null() || auth_len == 0 {
            return Ok(Auth::None);
        }
        unsafe { json(auth, auth_len) }
    }

    /// Reach the server at `url`, its base `http` or `https` URL (every endpoint is under it), sending
    /// `auth`, the credentials it wants as the `auth_len` bytes of `{"type": "bearer", "token":
    /// "..."}` or null for none, and the `headers_len` headers in `headers` -- each its name
    /// then its value, so `2 * headers_len` strings -- with every request, and write the
    /// client, held once, to `client`. The headers are whatever else the server asks of each
    /// request; `Authorization` is `auth`'s. Refused when a token would go in the clear: over
    /// `http` to any host but this machine.
    ///
    /// # Safety
    ///
    /// `url` and each of the `2 * headers_len` pointers in `headers` are NUL-terminated UTF-8.
    /// `auth` is null or `auth_len` readable bytes. `client` is writable. `error` is null or
    /// writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_connect(
        url: *const c_char,
        auth: *const u8,
        auth_len: usize,
        headers: *const *const c_char,
        headers_len: usize,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = (|| {
            let url = unsafe { utf8(url) }?;
            let auth = unsafe { self::auth(auth, auth_len) }?;
            let map = unsafe { header_map(headers, headers_len) }?;
            Runtime::open(|| Client::connect(url, auth, map).map_err(|e| format!("{e:#}")))
        })();
        unsafe { share(opened.map(VirtxClient), client, error) }
    }

    /// Send `auth`, as `virtx_client_connect` takes it, with every request from now on, in
    /// place of what was sent until now: a new token, when the server's are short-lived. A
    /// forwarder already open sends it with its next connection. Over stdio, where none is
    /// sent, nothing changes. Answers `VIRTX_OK`, or `VIRTX_ERROR` with why it is refused
    /// written to `error`, the auth unchanged.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held. `auth`
    /// is null or `auth_len` readable bytes. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_set_auth(
        client: *mut VirtxClient,
        auth: *const u8,
        auth_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int {
        let set = unsafe { self::auth(auth, auth_len) }.and_then(|auth| {
            unsafe { &(*client).0 }
                .value()
                .set_auth(auth)
                .map_err(|e| format!("{e:#}"))
        });
        match set {
            Ok(()) => VIRTX_OK,
            Err(message) => {
                let (status, message) = failure(message);
                unsafe { write(error, message) };
                status
            }
        }
    }

    /// Send the `headers_len` headers in `headers`, as `virtx_client_connect` takes them, with
    /// every request from now on, in place of those sent until now. A forwarder already open
    /// sends them with its next connection. Over stdio, where none are sent, nothing changes.
    /// Answers `VIRTX_OK`, or `VIRTX_ERROR` with why a header cannot be sent written to
    /// `error`, the headers unchanged.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held. Each
    /// of the `2 * headers_len` pointers in `headers` is NUL-terminated UTF-8. `error` is null
    /// or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_set_headers(
        client: *mut VirtxClient,
        headers: *const *const c_char,
        headers_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int {
        match unsafe { header_map(headers, headers_len) } {
            Ok(map) => {
                unsafe { &(*client).0 }.value().set_headers(map);
                VIRTX_OK
            }
            Err(message) => {
                let (status, message) = failure(message);
                unsafe { write(error, message) };
                status
            }
        }
    }

    /// Hold `client` once more, for another holder to release.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_retain(client: *mut VirtxClient) {
        unsafe { retain(client) };
    }

    /// Let go of `client`. The last holder's release answers every request not yet answered
    /// with `VIRTX_ERROR`, ends the session, and frees it; its futures and streams stay to be
    /// polled and freed. Null does nothing.
    ///
    /// # Safety
    ///
    /// `client` is null, or held by the caller, who does not use it again. Not called from a
    /// `wake`, which runs on the runtime this may stop.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_release(client: *mut VirtxClient) {
        if let Some(VirtxClient(runtime)) = unsafe { release(client) } {
            runtime.close(Client::close);
        }
    }

    /// Starts building `recipe`, to point `reference` (`name:tag`) at it, given
    /// `{"recipe": {"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}, "reference": "name:tag"}`.
    /// Answers with the job, as `{"id": "...", "kind": "build", "reference": "name:tag",
    /// "running": bool, "digest": "algorithm:hex", "error": "..."}`, `digest` once it has
    /// succeeded and `error` once it has failed. Its log is `virtx_client_image_job_log`'s.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`. `args` is `args_len` readable bytes, not
    /// read after this returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_build_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageBuildArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImageBuildArgs { recipe, reference } = args?;
            client
                .build_image(recipe, reference)
                .await
                .map_err(Failure::from)
        })
    }

    /// Starts pulling `reference` from its registry, to point it at what it pulls, given
    /// `{"reference": "alpine:3.20", "auth": {...}}`, the optional auth as
    /// `{"type": "basic", "username": "...", "password": "..."}` or
    /// `{"type": "bearer", "token": "..."}`. Answers with the job, as
    /// `virtx_client_build_image` does, its `kind` `"pull"`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_pull_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImagePullArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImagePullArgs { reference, auth } = args?;
            client
                .pull_image(reference, auth)
                .await
                .map_err(Failure::from)
        })
    }

    /// Starts pushing the image `reference` points at to the registry `reference` names, given
    /// `{"reference": "ghcr.io/owner/name:tag", "auth": {...}}`, the optional auth as in
    /// `virtx_client_pull_image`. Answers with the job, as `virtx_client_build_image` does, its
    /// `kind` `"push"`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_push_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImagePushArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImagePushArgs { reference, auth } = args?;
            client
                .push_image(reference, auth)
                .await
                .map_err(Failure::from)
        })
    }

    /// Answers with every image job, each as `virtx_client_build_image` answers one.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image_job(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_image_job().await.map_err(Failure::from) })
    }

    /// Answers with the image job `id`, given `{"id": "..."}`, as `virtx_client_build_image`
    /// answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_image_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.get_image_job(args?.id).await.map_err(Failure::from)
        })
    }

    /// Cancels the image job `id` if it runs, and forgets it, given `{"id": "..."}`. Answers
    /// with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_image_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_image_job(args?.id)
                .await
                .map_err(Failure::from)
        })
    }

    /// The log of the image job `id`, given `{"id": "...", "after": n}`, `after` optional: from
    /// its start, or after its `after`th entry. Its items are the entries, each with its number
    /// beside it: `{"id": n, "log": "..."}`, `{"id": n, "truncated": {}}` once if the log
    /// reached the server's limit, then `{"id": n, "end": {"digest": "..."}}` or
    /// `{"id": n, "end": {"error": "..."}}` last. Freeing it leaves the job as it is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_image_job_log(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageJobLogArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageJobLogArgs { id, after } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(Failure::from(error));
                        return;
                    }
                };
                let mut log = client.image_job_log(&id, after);
                while let Some(entry) = log.next().await {
                    yield entry.map_err(Failure::from);
                }
            }
        })
    }

    /// Answers with every tag on the server's images, as
    /// `[{"reference": "name:tag", "digest": "algorithm:hex"}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image(client: *mut VirtxClient) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_image().await.map_err(Failure::from) })
    }

    /// Points the tag `target` at the image `source` names, given
    /// `{"source": "name:tag" or "name@algorithm:hex", "target": "name:tag"}`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_tag_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageTagArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImageTagArgs { source, target } = args?;
            client
                .tag_image(source, target)
                .await
                .map_err(Failure::from)
        })
    }

    /// Removes the tag `reference`, leaving its image, given `{"reference": "name:tag"}`.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_untag_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageUntagArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .untag_image(args?.reference)
                .await
                .map_err(Failure::from)
        })
    }

    /// Removes the image `reference` names and every tag on it in that name, given
    /// `{"reference": "name:tag" or "name@algorithm:hex"}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_image(args?.reference)
                .await
                .map_err(Failure::from)
        })
    }

    /// Writes the image `reference` names to `path` as an OCI image layout in a tar archive,
    /// given `{"reference": "name:tag" or "name@algorithm:hex", "path": "..."}`. Its items are
    /// its log, a line a blob, as `{"progress": {"progress": n, "total": n, "content": "..."}}`
    /// (`total` and `content` each optional), then what it wrote, as
    /// `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_download_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageDownloadArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageDownloadArgs { reference, path } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(Failure::from(error));
                        return;
                    }
                };
                let mut download = client.download_image(reference, path);
                while let Some(event) = download.next().await {
                    yield event.map_err(Failure::from);
                }
            }
        })
    }

    /// Uploads the image in the OCI image layout archive at `path` and points the tag
    /// `reference` at it, given `{"path": "...", "reference": "name:tag"}`. Its items are its
    /// log, a line a blob, as `virtx_client_download_image`'s are, then what it uploaded, as
    /// `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_upload_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageUploadArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageUploadArgs { path, reference } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(Failure::from(error));
                        return;
                    }
                };
                let mut upload = client.upload_image(path, reference);
                while let Some(event) = upload.next().await {
                    yield event.map_err(Failure::from);
                }
            }
        })
    }

    /// Starts making an image of a sandbox -- its image, with what it has written as one more
    /// layer -- to point a reference at, given `{"sandbox": "...", "reference": "name:tag"}`.
    /// Answers with the job, as `virtx_client_build_image` does.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_commit_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageCommitArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            client
                .commit_image(args.sandbox, args.reference)
                .await
                .map_err(Failure::from)
        })
    }

    /// Deletes each image no tag points at and no sandbox stands on, and what only such images
    /// held. Answers with `{"images": ["algorithm:hex", ...], "reclaimed": n}`, the digests deleted
    /// and the bytes freed.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_prune_image(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.prune_image().await.map_err(Failure::from) })
    }

    /// Answers with the volumes, as `[{"name": "...", "size": n}, ...]`.
    ///
    /// Every value here is JSON. A request that takes arguments takes them as `args`, the
    /// `args_len` bytes of one object.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_volume(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_volume().await.map_err(Failure::from) })
    }

    /// Keeps a new, empty volume under `name`, or a name the server picks if there is none,
    /// given `{"name": "..."}` or `{}`. Answers with the volume, as `virtx_client_list_volume`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<VolumeCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .create_volume(args?.name)
                .await
                .map_err(Failure::from)
        })
    }

    /// Answers with the volume `name`, given `{"name": "..."}`, as `virtx_client_list_volume`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.get_volume(args?.name).await.map_err(Failure::from)
        })
    }

    /// Deletes the volume `name` and everything in it, given `{"name": "..."}`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_volume(args?.name)
                .await
                .map_err(Failure::from)
        })
    }

    /// Given `{"volume": "..."}`, answers with the volume's mounts, as
    /// `[{"path": "...", "mount_type": "notion" or "gdrive" or "onedrive"}, ...]`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .list_volume_mount(args?.volume)
                .await
                .map_err(Failure::from)
        })
    }

    /// Given `{"volume": "...", "path": "...", "source": {...}}`, the source as
    /// `{"type": "notion", "api_key": "..."}`,
    /// `{"type": "gdrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}`
    /// or `{"type": "onedrive", "client_id": "...", "refresh_token": "..."}` (and
    /// `"client_secret"` if it has one), mounts it. Answers with the mount, as
    /// `virtx_client_list_volume_mount` answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Mount {
                volume,
                path,
                source,
            } = args?;
            let source = source.ok_or("missing field `source`")?;
            client
                .create_volume_mount(volume, path, source)
                .await
                .map_err(Failure::from)
        })
    }

    /// Given `{"volume": "...", "path": "..."}`, takes the mount away. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Mount { volume, path, .. } = args?;
            client
                .remove_volume_mount(volume, path)
                .await
                .map_err(Failure::from)
        })
    }

    /// The files of a volume or a sandbox: what is at `path`, given
    /// `{"volume": "...", "path": "..."}` or `{"sandbox": "...", "path": "..."}`, the path
    /// relative to its root as `dir/file.txt`. Answers with
    /// `{"is_dir": bool, "size": n, "modified": n, "etag": "..."}`, `modified` in seconds since
    /// the Unix epoch and `etag` as the server spells it, each only if the server says.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_stat_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.stat_file(&root, &path).await.map_err(Failure::from)
        })
    }

    /// Whether anything is at `path`, given as `virtx_client_stat_file` takes it. Answers
    /// with `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_file_exists(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.exists(&root, &path).await.map_err(Failure::from)
        })
    }

    /// Whether a file is at `path`, given as `virtx_client_stat_file` takes it. Answers with
    /// `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_is_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.is_file(&root, &path).await.map_err(Failure::from)
        })
    }

    /// Whether a directory is at `path`, given as `virtx_client_stat_file` takes it. Answers
    /// with `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_is_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.is_dir(&root, &path).await.map_err(Failure::from)
        })
    }

    /// The entries of the directory at `path`, given as `virtx_client_stat_file` takes it.
    /// Answers with `[{"name": "...", "metadata": {...}}, ...]`, each metadata as
    /// `virtx_client_stat_file` answers it.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.read_dir(&root, &path).await.map_err(Failure::from)
        })
    }

    /// The file at `path`, given `{"volume": "...", "path": "...", "offset": n, "length": n}`,
    /// `offset` and `length` optional: from `offset`, `length` bytes of it or all that is
    /// left. Answers with the bytes read as they are, not as JSON.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileReadArgs>(args, args_len) };
        unsafe { &(*client).0 }.future_raw(|client| async move {
            let FileReadArgs {
                root,
                path,
                offset,
                length,
            } = args?;
            client
                .read_file(&root, &path, offset, length)
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(Failure::from)
        })
    }

    /// The whole file at `path`, given as `virtx_client_stat_file` takes it, as text: refused
    /// unless it is UTF-8. Answers with the text as a JSON string.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_text(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.read_text(&root, &path).await.map_err(Failure::from)
        })
    }

    /// Writes the `data_len` bytes at `data` as the whole file at `path`, given
    /// `{"volume": "...", "path": "...", "if_match": "..."}`, `if_match` optional: the `etag`
    /// `virtx_client_stat_file` gave, which the file has to have still, or the write is refused
    /// with a `412`. The file is created if it does not exist; its parent directory must exist.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`; `data` is `data_len` readable bytes, not read after this
    /// returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_write_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
        data: *const u8,
        data_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileWriteArgs>(args, args_len) };
        let content = if data.is_null() {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(unsafe { slice::from_raw_parts(data, data_len) })
        };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileWriteArgs {
                root,
                path,
                if_match,
            } = args?;
            client
                .write_file(&root, &path, content, if_match.as_deref())
                .await
                .map_err(Failure::from)
        })
    }

    /// Creates the directory at `path`, given as `virtx_client_stat_file` takes it, whose
    /// parent must exist. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.create_dir(&root, &path).await.map_err(Failure::from)
        })
    }

    /// Has the directory at `path`, given as `virtx_client_stat_file` takes it, as `mkdir -p`:
    /// its parents are created as needed, and one there already is left as it is. Refused with
    /// a `405` when a file is at `path`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_dir_all(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client
                .create_dir_all(&root, &path)
                .await
                .map_err(Failure::from)
        })
    }

    /// Removes what is at `path`, given `{"volume": "...", "path": "...", "missing_ok": bool}`,
    /// `missing_ok` optional and false without it: a directory with everything in it. With
    /// `missing_ok`, nothing being there is fine, as `rm -f`; without it, a `404`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileRemoveArgs {
                root,
                path,
                missing_ok,
            } = args?;
            client
                .remove_file(&root, &path, missing_ok)
                .await
                .map_err(Failure::from)
        })
    }

    /// Moves what is at `from` to `to` in the same volume, given
    /// `{"volume": "...", "from": "...", "to": "...", "overwrite": bool}`, `overwrite` optional
    /// and false without it: whether to replace what is at `to`, which is otherwise refused.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_rename_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileMoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileMoveArgs {
                root,
                from,
                to,
                overwrite,
            } = args?;
            client
                .rename_file(&root, &from, &to, overwrite)
                .await
                .map_err(Failure::from)
        })
    }

    /// Copies what is at `from` to `to`, a directory with everything in it, given as
    /// `virtx_client_rename_file` takes it. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_copy_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileMoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileMoveArgs {
                root,
                from,
                to,
                overwrite,
            } = args?;
            client
                .copy_file(&root, &from, &to, overwrite)
                .await
                .map_err(Failure::from)
        })
    }

    /// Answers with the sandboxes, as
    /// `[{"id": "...", "name": "...", "image": "algorithm:hex", "state": "running",
    /// "ephemeral": bool, "auto_stop_minutes": n, "auto_delete_days": n, "used_at": "..."}, ...]`,
    /// `name` only for one that has one, `state` `"running"` or `"stopped"`, and
    /// `auto_delete_days` `null` for one kept until it is deleted.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_sandbox(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_sandbox().await.map_err(Failure::from) })
    }

    /// Makes a sandbox, given
    /// `{"image": {...}, "name": "...", "ephemeral": bool, "auto_stop_minutes": n,
    /// "auto_delete_days": n, "disk_gib": n, "network": bool, "ports": [n],
    /// "vcpus": n, "memory_mib": n, "gpu": bool, "gpu_memory_mib": n,
    /// "volumes": [{"volume": "...", "path": "/...", "readonly": bool}]}`,
    /// `auto_delete_days` `null` to keep it until it is deleted, the image as
    /// `{"type": "tag", "tag": "name:tag"}` or
    /// `{"type": "digest", "digest": "algorithm:hex"}` and all but it optional. Answers with the
    /// sandbox as `virtx_client_list_sandbox` answers each, and
    /// `"ports": [{"port": n, "url": "..."}]` if it was given some: here only.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<SandboxCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.create_sandbox(&args?).await.map_err(Failure::from)
        })
    }

    /// Answers with the sandbox `id`, given `{"id": "..."}`, as `virtx_client_list_sandbox`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.get_sandbox(args?.id).await.map_err(Failure::from)
        })
    }

    /// Answers with the sandbox named `name`, given `{"name": "..."}`, as
    /// `virtx_client_list_sandbox` answers each, or `null` if none is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_find_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .find_sandbox(args?.name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Starts the sandbox `id` if it is stopped, given `{"id": "..."}`. Answers once it runs,
    /// as `virtx_client_create_sandbox` does, with where its ports are now.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_start_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .start_sandbox(args?.id)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Stops the sandbox `id` if it runs, given `{"id": "..."}`: its jobs end, and its disk is
    /// kept -- or, for an ephemeral one, deleted with it. Answers once it has stopped, as
    /// `virtx_client_list_sandbox` answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_stop_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .stop_sandbox(args?.id)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Opens the port `port` in the sandbox `id`, given `{"id": "...", "port": n, "label"?: "...",
    /// "protocol"?: "tcp" | "http", "inbound"?: [{"source": "...", "description"?: "..."}]}`.
    /// Answers with `{"port": n, "url": "...", "label"?, "protocol"?, "address"?}`: where it is
    /// reached from now.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_open_sandbox_port(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct IdPort {
            id: String,
            port: u16,
            #[serde(default)]
            label: Option<String>,
            #[serde(default)]
            protocol: Option<crate::PortProtocol>,
            #[serde(default)]
            inbound: Option<Vec<InboundRule>>,
        }
        let args = unsafe { json::<IdPort>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            let open = crate::SandboxPortOpenArgs {
                label: args.label,
                protocol: args.protocol,
                inbound: args.inbound,
            };
            client
                .open_sandbox_port_with(args.id, args.port, &open)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Closes the port `port` in the sandbox `id`, given `{"id": "...", "port": n}`: its URL
    /// reaches it no more. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_close_sandbox_port(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<IdPort>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            client
                .close_sandbox_port(args.id, args.port)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Answers with the inbound rules of the port `port` in the sandbox `id`, given
    /// `{"id": "...", "port": n}`, as `{"port": n, "rules": [{"source": "...",
    /// "description"?: "..."}], "address"?: "..."}`: virtx cloud's.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_sandbox_port_inbound(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<IdPort>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            client
                .get_sandbox_port_inbound(args.id, args.port)
                .await
                .map_err(Failure::from)
        })
    }

    /// Makes the inbound rules of the port `port` in the sandbox `id` `rules`, all of them, given
    /// `{"id": "...", "port": n, "rules": [{"source": "...", "description"?: "..."}]}`. Answers
    /// with them as `virtx_client_get_sandbox_port_inbound` does.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_set_sandbox_port_inbound(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Rules {
            id: String,
            port: u16,
            rules: Vec<InboundRule>,
        }
        let args = unsafe { json::<Rules>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            client
                .set_sandbox_port_inbound(args.id, args.port, &args.rules)
                .await
                .map_err(Failure::from)
        })
    }

    /// Deletes the sandbox `id`, given `{"id": "..."}`: stopped first if it runs, and its disk
    /// removed. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.remove_sandbox(args?.id).await.map_err(Failure::from)
        })
    }

    /// Given `{"sandbox": "..."}`, answers with its jobs, each as `virtx_client_exec_sandbox`
    /// answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .list_sandbox_job(args?.sandbox)
                .await
                .map_err(Failure::from)
        })
    }

    /// Starts a process, given `{"sandbox": "...", "cmd": ["..."], "stdin": bool,
    /// "timeout_ms": n}`, `stdin` and `timeout_ms` optional. Answers with its job, as
    /// `{"id": "...", "kind": "exec", "pid": n, "cmd": ["..."], "running": bool, "code": n}`,
    /// `code` once it has ended. Its output is `virtx_client_sandbox_job_log`'s.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_exec_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Exec>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Exec {
                sandbox,
                cmd,
                stdin,
                timeout_ms,
            } = args?;
            let args = SandboxExecArgs {
                cmd,
                stdin,
                timeout_ms,
            };
            client
                .exec_sandbox(sandbox, &args)
                .await
                .map_err(Failure::from)
        })
    }

    /// Answers with the job `job` in the sandbox `sandbox`, given
    /// `{"sandbox": "...", "job": "..."}`, as `virtx_client_exec_sandbox` answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job { sandbox, job, .. } = args?;
            client
                .get_sandbox_job(sandbox, job.ok_or("missing field `job`")?)
                .await
                .map_err(Failure::from)
        })
    }

    /// Cancels the job if it runs, killing its process, and forgets it, given
    /// `{"sandbox": "...", "job": "..."}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job { sandbox, job, .. } = args?;
            client
                .remove_sandbox_job(sandbox, job.ok_or("missing field `job`")?)
                .await
                .map_err(Failure::from)
        })
    }

    /// Sends `signal`, a name such as `"SIGTERM"`, to the job's process and every process it
    /// started, given `{"sandbox": "...", "job": "...", "signal": "..."}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_signal_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job {
                sandbox,
                job,
                signal,
                ..
            } = args?;
            client
                .signal_sandbox_job(
                    sandbox,
                    job.ok_or("missing field `job`")?,
                    signal.ok_or("missing field `signal`")?,
                )
                .await
                .map_err(Failure::from)
        })
    }

    /// Writes `data` to the stdin of the job's process, then closes it if `eof`, given
    /// `{"sandbox": "...", "job": "...", "data": "...", "eof": bool}`, `data` and `eof`
    /// optional. Answers with `null` once it is written: the next write is sent only then.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_write_sandbox_job_stdin(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job {
                sandbox,
                job,
                data,
                eof,
                ..
            } = args?;
            client
                .write_sandbox_job_stdin(sandbox, job.ok_or("missing field `job`")?, data, eof)
                .await
                .map_err(Failure::from)
        })
    }

    /// The log of the job, given `{"sandbox": "...", "job": "...", "after": n}`, `after`
    /// optional: from its start, or after its `after`th entry. Its items are the entries, each
    /// with its number beside it: `{"id": n, "stdout": "..."}` or `{"id": n, "stderr": "..."}`,
    /// a line as it is written, `{"id": n, "truncated": {}}` once if the log reached the
    /// server's limit, then `{"id": n, "end": {"code": n}}` last. Freeing it leaves the job as
    /// it is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_sandbox_job_log(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let (sandbox, job, after) = match args {
                    Ok(Job { sandbox, job: Some(job), after, .. }) => (sandbox, job, after),
                    Ok(_) => {
                        yield Err(Failure::from("missing field `job`"));
                        return;
                    }
                    Err(error) => {
                        yield Err(Failure::from(error));
                        return;
                    }
                };
                let mut log = client.sandbox_job_log(&sandbox, &job, after);
                while let Some(entry) = log.next().await {
                    yield entry.map_err(Failure::from);
                }
            }
        })
    }

    /// Starts a process as `virtx_client_exec_sandbox` does, given the same, and reads its log:
    /// its first item is `{"job": {...}}`, the job as that answers it, and the rest are the
    /// log's entries as `virtx_client_sandbox_job_log` gives them. One request where the server
    /// answers an exec with its log, two elsewhere. Freeing it leaves the job as it is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_exec_sandbox_log(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<Exec>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let (sandbox, args) = match args {
                    Ok(Exec { sandbox, cmd, stdin, timeout_ms }) => {
                        (sandbox, SandboxExecArgs { cmd, stdin, timeout_ms })
                    }
                    Err(error) => {
                        yield Err(Failure::from(error));
                        return;
                    }
                };
                let mut items = client.exec_sandbox_with_log(&sandbox, &args);
                while let Some(item) = items.next().await {
                    yield item.map_err(Failure::from);
                }
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use std::ptr;

        use super::*;
        use crate::ffi::virtx_bytes_free;

        #[test]
        fn spawning_nothing_fails_with_a_message() {
            let mut client = ptr::null_mut();
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let program = c"/nonexistent/virtx-server";
            let status = unsafe {
                virtx_client_spawn(program.as_ptr(), ptr::null(), 0, &mut client, &mut error)
            };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            assert!(client.is_null());
            assert!(!error.data.is_null());
            unsafe { virtx_bytes_free(error) };
        }

        #[test]
        fn a_header_that_cannot_be_sent_is_refused() {
            let mut client = ptr::null_mut();
            let url = c"http://127.0.0.1:9/";
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    &mut client,
                    ptr::null_mut(),
                )
            };
            assert_eq!(status, VIRTX_OK);
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let headers = [c"not a name".as_ptr(), c"x".as_ptr()];
            let status =
                unsafe { virtx_client_set_headers(client, headers.as_ptr(), 1, &mut error) };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("not a name"));
            unsafe { virtx_bytes_free(error) };
            unsafe { virtx_client_release(client) };
        }

        #[test]
        fn an_auth_is_taken_as_json_and_refused_as_at_connect() {
            let mut client = ptr::null_mut();
            let url = c"http://127.0.0.1:9/";
            let bearer = br#"{"type": "bearer", "token": "t"}"#;
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    bearer.as_ptr(),
                    bearer.len(),
                    ptr::null(),
                    0,
                    &mut client,
                    ptr::null_mut(),
                )
            };
            assert_eq!(status, VIRTX_OK);
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let basic = br#"{"type": "basic", "username": "u", "password": "p"}"#;
            let status =
                unsafe { virtx_client_set_auth(client, basic.as_ptr(), basic.len(), &mut error) };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("basic"));
            unsafe { virtx_bytes_free(error) };
            unsafe { virtx_client_release(client) };

            let url = c"http://example.com/";
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    bearer.as_ptr(),
                    bearer.len(),
                    ptr::null(),
                    0,
                    &mut client,
                    &mut error,
                )
            };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("in the clear"));
            unsafe { virtx_bytes_free(error) };
        }
    }
}
