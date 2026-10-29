//! The files endpoint's client: each request `PROTOCOL.md` lists for the files of a volume or
//! a snapshot, as a WebDAV request to `/volumes/{name}/` or `/snapshots/{name}/`, over either
//! transport.

use std::time::UNIX_EPOCH;

use bytes::Bytes;
use http::{Method, StatusCode, header};
use http_body_util::BodyExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use super::{Body, TransportClient, full};
use crate::{
    Error,
    files::{DirEntry, FileRoot, Metadata},
};

/// What a `PROPFIND` asks for: the properties `PROTOCOL.md` says every server answers.
const PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:getetag/></D:prop></D:propfind>"#;

/// The WebDAV namespace.
const DAV: &str = "DAV:";

/// What a path segment keeps as it is: the unreserved characters of RFC 3986.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl TransportClient {
    /// What is at `path` in `root`.
    pub async fn stat(&self, root: &FileRoot, path: &str) -> Result<Metadata, Error> {
        let url = uri(root, path);
        let responses = self.propfind(&url, "0").await?;
        responses
            .into_iter()
            .next()
            .map(|(_, metadata)| metadata)
            .ok_or_else(|| Error::Transport("a PROPFIND answered no response".into()))
    }

    /// The entries of the directory at `path` in `root`.
    pub async fn read_dir(&self, root: &FileRoot, path: &str) -> Result<Vec<DirEntry>, Error> {
        let url = uri(root, path);
        let own = href_path(&url);
        let own = normalize(&own);
        let mut entries = Vec::new();
        for (href, metadata) in self.propfind(&url, "1").await? {
            // The directory itself is one of the responses: its path, under whatever base the
            // server is mounted at.
            let href = href_path(&href);
            let href = normalize(&href);
            if href.ends_with(own) {
                continue;
            }
            let name = href.rsplit('/').next().unwrap_or("").to_owned();
            entries.push(DirEntry { name, metadata });
        }
        Ok(entries)
    }

    /// The file at `path` in `root`, from `offset`, `length` bytes of it or all that is left.
    pub async fn read(
        &self,
        root: &FileRoot,
        path: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Bytes, Error> {
        if length == Some(0) {
            return Ok(Bytes::new());
        }
        let mut request = http::Request::get(uri(root, path));
        let ranged = offset > 0 || length.is_some();
        if ranged {
            let range = match length {
                Some(length) => format!("bytes={offset}-{}", offset + length - 1),
                None => format!("bytes={offset}-"),
            };
            request = request.header(header::RANGE, range);
        }
        let (status, body) = self.files_request(request.body(full(Bytes::new()))).await?;
        if status == StatusCode::OK && ranged {
            // The server sent the whole file instead of the range asked for.
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(body.len());
            let end = length
                .and_then(|length| usize::try_from(length).ok())
                .and_then(|length| start.checked_add(length))
                .unwrap_or(body.len())
                .min(body.len());
            return Ok(body.slice(start..end));
        }
        Ok(body)
    }

    /// Write `content` as the whole file at `path` in `root`, creating it if it does not
    /// exist. Its parent directory must exist. With `if_match`, the `etag` a [`Metadata`] of
    /// the file gave, the write goes only while the file still has it, and is refused with a
    /// `412` otherwise.
    pub async fn write(
        &self,
        root: &FileRoot,
        path: &str,
        content: impl Into<Bytes>,
        if_match: Option<&str>,
    ) -> Result<(), Error> {
        let mut request = http::Request::put(uri(root, path));
        if let Some(etag) = if_match {
            request = request.header(header::IF_MATCH, etag);
        }
        self.files_request(request.body(full(content)))
            .await
            .map(drop)
    }

    /// Create the directory at `path` in `root`, whose parent must exist.
    pub async fn create_dir(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        let request = http::Request::builder()
            .method("MKCOL")
            .uri(uri(root, path))
            .body(full(Bytes::new()));
        self.files_request(request).await.map(drop)
    }

    /// Have the directory at `path` in `root`, as `mkdir -p`: its parents are created as
    /// needed, and one there already is left as it is. Refused with a `405` when a file is at
    /// `path`.
    pub async fn create_dir_all(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return Ok(());
        }
        match self.create_dir(root, path).await {
            Err(Error::Status(409, _)) => {}
            other => return self.created_or_there(root, path, other).await,
        }
        // A parent is missing: from the top down, whichever are not there yet. A parent that is
        // a file is not checked: the `MKCOL` under it is refused anyway.
        for end in 1..segments.len() {
            match self.create_dir(root, &segments[..end].join("/")).await {
                Ok(()) | Err(Error::Status(405, _)) => {}
                Err(error) => return Err(error),
            }
        }
        let created = self.create_dir(root, path).await;
        self.created_or_there(root, path, created).await
    }

    /// `created`, an `MKCOL` at `path`, as `Ok` also when it answered `405` for a directory
    /// there already.
    async fn created_or_there(
        &self,
        root: &FileRoot,
        path: &str,
        created: Result<(), Error>,
    ) -> Result<(), Error> {
        let Err(Error::Status(405, _)) = &created else {
            return created;
        };
        if self.stat(root, path).await?.is_dir {
            Ok(())
        } else {
            created
        }
    }

    /// Remove what is at `path` in `root`: a directory with everything in it.
    pub async fn remove(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        let request = http::Request::delete(uri(root, path)).body(full(Bytes::new()));
        self.files_request(request).await.map(drop)
    }

    /// Move what is at `from` in `root` to `to` in it, replacing what is at `to` if
    /// `overwrite`, and refused if not and something is there.
    pub async fn rename(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        self.relocate(
            Method::from_bytes(b"MOVE").unwrap(),
            root,
            from,
            to,
            overwrite,
        )
        .await
    }

    /// Copy what is at `from` in `root` to `to` in it, a directory with everything in it, as
    /// [`rename`](Self::rename) replaces what is at `to`.
    pub async fn copy(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        self.relocate(
            Method::from_bytes(b"COPY").unwrap(),
            root,
            from,
            to,
            overwrite,
        )
        .await
    }

    async fn relocate(
        &self,
        method: Method,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        let request = http::Request::builder()
            .method(method)
            .uri(uri(root, from))
            .header("Destination", uri(root, to))
            .header("Overwrite", if overwrite { "T" } else { "F" })
            .body(full(Bytes::new()));
        self.files_request(request).await.map(drop)
    }

    /// Send a request to the files endpoint, and read its body. A status outside `2xx` is an
    /// [`Error::Status`].
    async fn files_request(
        &self,
        request: http::Result<http::Request<Body>>,
    ) -> Result<(StatusCode, Bytes), Error> {
        let request = request.map_err(|e| Error::Transport(format!("not a request: {e}")))?;
        // What a `207` means depends on the request: a `PROPFIND` succeeds with one, and these
        // report what failed in a directory, per path.
        let per_path = matches!(request.method().as_str(), "DELETE" | "COPY" | "MOVE");
        let response = self.send(request).await?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::Transport(format!("reading the response: {e}")))?
            .to_bytes();
        if !status.is_success() {
            let body = String::from_utf8_lossy(&body).trim().to_owned();
            return Err(Error::Status(status.as_u16(), body));
        }
        if status == StatusCode::MULTI_STATUS && per_path {
            // A `207` whose paths all succeeded is a success.
            let xml = std::str::from_utf8(&body)
                .map_err(|e| Error::Transport(format!("a 207 answered no XML: {e}")))?;
            if let Some((status, path)) = failure_in(xml) {
                return Err(Error::Status(status, path));
            }
        }
        Ok((status, body))
    }

    /// `PROPFIND` `url` at `depth`: each response's `href` and what its properties say.
    async fn propfind(&self, url: &str, depth: &str) -> Result<Vec<(String, Metadata)>, Error> {
        let request = http::Request::builder()
            .method("PROPFIND")
            .uri(url)
            .header("Depth", depth)
            .header(header::CONTENT_TYPE, "application/xml; charset=utf-8")
            .body(full(PROPFIND));
        let (_, body) = self.files_request(request).await?;
        let body = std::str::from_utf8(&body)
            .map_err(|e| Error::Transport(format!("a PROPFIND answered no XML: {e}")))?;
        multistatus(body).map_err(|e| Error::Transport(format!("a PROPFIND answered: {e}")))
    }
}

/// The path of `path` in `root`, under the server's base: each segment percent-encoded,
/// empty segments dropped.
fn uri(root: &FileRoot, path: &str) -> String {
    let (kind, name) = match root {
        FileRoot::Volume(name) => ("volumes", name),
        FileRoot::Snapshot(name) => ("snapshots", name),
    };
    let mut uri = format!("/{kind}/{}/", utf8_percent_encode(name, SEGMENT));
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    if let Some(first) = segments.next() {
        uri.push_str(&utf8_percent_encode(first, SEGMENT).to_string());
        for segment in segments {
            uri.push('/');
            uri.push_str(&utf8_percent_encode(segment, SEGMENT).to_string());
        }
    }
    uri
}

/// The path of `href`, an absolute path or a full URL, percent-decoded.
fn href_path(href: &str) -> String {
    let path = if href.starts_with('/') {
        href
    } else {
        match reqwest::Url::parse(href) {
            Ok(url) => {
                return percent_decode_str(url.path())
                    .decode_utf8_lossy()
                    .into_owned();
            }
            Err(_) => href,
        }
    };
    percent_decode_str(path).decode_utf8_lossy().into_owned()
}

/// `path` without its trailing slash, so a directory's two spellings are one.
fn normalize(path: &str) -> &str {
    path.strip_suffix('/').unwrap_or(path)
}

/// Each `response` of a `multistatus`: its `href`, and the metadata of its `200` properties.
fn multistatus(xml: &str) -> Result<Vec<(String, Metadata)>, String> {
    let document = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = document.root_element();
    if !is(root, "multistatus") {
        return Err(format!(
            "not a multistatus but `{}`",
            root.tag_name().name()
        ));
    }
    let mut responses = Vec::new();
    for response in root.children().filter(|node| is(*node, "response")) {
        let href = response
            .children()
            .find(|node| is(*node, "href"))
            .and_then(|node| node.text())
            .ok_or("a response without an href")?
            .trim()
            .to_owned();
        let mut metadata = Metadata {
            is_dir: false,
            size: 0,
            modified: None,
            etag: None,
        };
        for propstat in response.children().filter(|node| is(*node, "propstat")) {
            let ok = propstat
                .children()
                .find(|node| is(*node, "status"))
                .and_then(|node| node.text())
                .is_some_and(|status| status.contains(" 200 "));
            if !ok {
                continue;
            }
            let Some(prop) = propstat.children().find(|node| is(*node, "prop")) else {
                continue;
            };
            for property in prop.children().filter(|node| node.is_element()) {
                let text = property.text().map(str::trim).unwrap_or("");
                match property.tag_name().name() {
                    "resourcetype" if property.tag_name().namespace() == Some(DAV) => {
                        metadata.is_dir = property.children().any(|node| is(node, "collection"));
                    }
                    "getcontentlength" if property.tag_name().namespace() == Some(DAV) => {
                        metadata.size = text.parse().unwrap_or(0);
                    }
                    "getlastmodified" if property.tag_name().namespace() == Some(DAV) => {
                        metadata.modified = httpdate::parse_http_date(text)
                            .ok()
                            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                            .map(|since| since.as_secs());
                    }
                    "getetag" if property.tag_name().namespace() == Some(DAV) => {
                        metadata.etag = (!text.is_empty()).then(|| quoted(text));
                    }
                    _ => {}
                }
            }
        }
        responses.push((href, metadata));
    }
    Ok(responses)
}

/// The first `response` of `xml`, a `multistatus`, with a `status` of its own outside `2xx`:
/// that status, and the path of its `href`. `None` where there is none, or `xml` is not a
/// `multistatus`.
fn failure_in(xml: &str) -> Option<(u16, String)> {
    let document = roxmltree::Document::parse(xml).ok()?;
    let root = document.root_element();
    if !is(root, "multistatus") {
        return None;
    }
    root.children()
        .filter(|node| is(*node, "response"))
        .find_map(|response| {
            let status = response
                .children()
                .find(|node| is(*node, "status"))?
                .text()?
                .split_whitespace()
                .nth(1)?
                .parse::<u16>()
                .ok()?;
            if (200..300).contains(&status) {
                return None;
            }
            let href = response
                .children()
                .find(|node| is(*node, "href"))
                .and_then(|node| node.text())
                .map(|href| href_path(href.trim()))
                .unwrap_or_default();
            Some((status, href))
        })
}

/// `tag` as an `ETag` header spells it: in quotes, which a `getetag` is to carry already
/// (RFC 4918) and some servers leave off.
fn quoted(tag: &str) -> String {
    if tag.starts_with('"') || tag.starts_with("W/\"") {
        tag.to_owned()
    } else {
        format!("\"{tag}\"")
    }
}

/// Whether `node` is the `DAV:` element `name`.
fn is(node: roxmltree::Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name && node.tag_name().namespace() == Some(DAV)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_s_segments_are_percent_encoded_under_the_root() {
        let volume = FileRoot::Volume("my vol".into());
        assert_eq!(uri(&volume, ""), "/volumes/my%20vol/");
        assert_eq!(
            uri(&volume, "/dir//a b.txt/"),
            "/volumes/my%20vol/dir/a%20b.txt"
        );
        assert_eq!(
            uri(&volume, "한글/#?"),
            "/volumes/my%20vol/%ED%95%9C%EA%B8%80/%23%3F"
        );
        let snapshot = FileRoot::Snapshot("s".into());
        assert_eq!(uri(&snapshot, "a"), "/snapshots/s/a");
    }

    #[test]
    fn an_href_is_its_decoded_path() {
        assert_eq!(href_path("/volumes/v/a%20b/"), "/volumes/v/a b/");
        assert_eq!(
            href_path("http://host/base/volumes/v/a%20b"),
            "/base/volumes/v/a b"
        );
    }

    #[test]
    fn an_etag_is_quoted_as_a_header_is() {
        assert_eq!(quoted("5-1a"), "\"5-1a\"");
        assert_eq!(quoted("\"5-1a\""), "\"5-1a\"");
        assert_eq!(quoted("W/\"5-1a\""), "W/\"5-1a\"");
    }

    #[test]
    fn a_multistatus_names_what_failed_in_it() {
        let failed = r#"<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/volumes/v/dir/</D:href><D:status>HTTP/1.1 204 No Content</D:status></D:response><D:response><D:href>/volumes/v/dir/a%20b</D:href><D:status>HTTP/1.1 403 Forbidden</D:status></D:response></D:multistatus>"#;
        assert_eq!(
            failure_in(failed),
            Some((403, "/volumes/v/dir/a b".to_owned()))
        );
        let all_well = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/x</D:href><D:status>HTTP/1.1 201 Created</D:status></D:response></D:multistatus>"#;
        assert_eq!(failure_in(all_well), None);
        // A PROPFIND's responses carry their statuses in propstats, which are not failures.
        let propfind = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/x</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(failure_in(propfind), None);
        assert_eq!(failure_in("not xml"), None);
    }

    #[test]
    fn a_multistatus_reads_into_metadata() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/volumes/v/dir/</D:href>
    <D:propstat>
      <D:prop><D:resourcetype><D:collection/></D:resourcetype><D:getcontentlength>4096</D:getcontentlength></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/volumes/v/dir/file.txt</D:href>
    <D:propstat>
      <D:prop><D:resourcetype/><D:getcontentlength>5</D:getcontentlength><D:getlastmodified>Thu, 01 Jan 1970 00:01:40 GMT</D:getlastmodified></D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
    <D:propstat>
      <D:prop><D:getcontentlength>999</D:getcontentlength></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>"#;
        let responses = multistatus(xml).unwrap();
        assert_eq!(
            responses,
            [
                (
                    "/volumes/v/dir/".to_owned(),
                    Metadata {
                        is_dir: true,
                        size: 4096,
                        modified: None,
                        etag: None,
                    }
                ),
                (
                    "/volumes/v/dir/file.txt".to_owned(),
                    Metadata {
                        is_dir: false,
                        size: 5,
                        modified: Some(100),
                        etag: None,
                    }
                ),
            ]
        );
        assert!(multistatus("<D:prop xmlns:D=\"DAV:\"/>").is_err());
        assert!(multistatus("not xml").is_err());
    }
}

#[cfg(all(test, feature = "testing"))]
mod round_trips {
    use super::*;
    use crate::rpc::tests::clients;

    fn volume() -> FileRoot {
        FileRoot::Volume("v".into())
    }

    /// The status `result` was refused with.
    fn status<T: std::fmt::Debug>(result: Result<T, Error>) -> u16 {
        match result {
            Err(Error::Status(status, _)) => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_file_written_is_read_back_whole_or_in_part() {
        for client in clients().await {
            let root = volume();
            client
                .write(&root, "hello.txt", "hello world", None)
                .await
                .unwrap();
            let metadata = client.stat(&root, "hello.txt").await.unwrap();
            assert!(!metadata.is_dir);
            assert_eq!(metadata.size, 11);
            assert!(metadata.modified.is_some());

            assert_eq!(
                client.read(&root, "hello.txt", 0, None).await.unwrap(),
                "hello world"
            );
            assert_eq!(
                client.read(&root, "hello.txt", 6, None).await.unwrap(),
                "world"
            );
            assert_eq!(
                client.read(&root, "hello.txt", 2, Some(3)).await.unwrap(),
                "llo"
            );
            assert_eq!(
                client.read(&root, "hello.txt", 2, Some(0)).await.unwrap(),
                ""
            );

            client.write(&root, "hello.txt", "bye", None).await.unwrap();
            assert_eq!(
                client.read(&root, "hello.txt", 0, None).await.unwrap(),
                "bye"
            );
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_directory_lists_what_is_in_it_and_goes_with_it() {
        for client in clients().await {
            let root = volume();
            client.create_dir(&root, "dir").await.unwrap();
            client.create_dir(&root, "dir/sub").await.unwrap();
            client
                .write(&root, "dir/a.txt", "aaaa", None)
                .await
                .unwrap();
            assert!(client.stat(&root, "dir").await.unwrap().is_dir);

            let mut entries = client.read_dir(&root, "dir").await.unwrap();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
            assert_eq!(names, ["a.txt", "sub"]);
            assert!(!entries[0].metadata.is_dir);
            assert_eq!(entries[0].metadata.size, 4);
            assert!(entries[1].metadata.is_dir);

            let names: Vec<_> = client
                .read_dir(&root, "")
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert!(names.contains(&"dir".to_owned()), "{names:?}");

            client.remove(&root, "dir").await.unwrap();
            assert_eq!(status(client.stat(&root, "dir").await), 404);
            assert_eq!(status(client.stat(&root, "dir/a.txt").await), 404);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_directory_is_had_with_its_parents_and_as_often_as_asked() {
        for client in clients().await {
            let root = volume();
            client.create_dir_all(&root, "a/b/c").await.unwrap();
            assert!(client.stat(&root, "a/b/c").await.unwrap().is_dir);
            // Some parents there, some not; all there, spelled otherwise; nothing to make.
            client.create_dir_all(&root, "a/x/y").await.unwrap();
            assert!(client.stat(&root, "a/x/y").await.unwrap().is_dir);
            client.create_dir_all(&root, "a/b/c").await.unwrap();
            client.create_dir_all(&root, "/a//b/").await.unwrap();
            client.create_dir_all(&root, "").await.unwrap();

            client.write(&root, "a/file", "x", None).await.unwrap();
            assert_eq!(status(client.create_dir_all(&root, "a/file").await), 405);
            // Under a file: the `409` RFC 4918 names, or the `403` it also allows.
            let under_file = status(client.create_dir_all(&root, "a/file/sub").await);
            assert!(matches!(under_file, 403 | 409), "{under_file}");
            client.close().await;
        }
    }

    #[tokio::test]
    async fn what_rfc_4918_refuses_is_refused_with_its_status() {
        for client in clients().await {
            let root = volume();
            // A parent that does not exist.
            assert_eq!(status(client.create_dir(&root, "no/dir").await), 409);
            assert_eq!(status(client.write(&root, "no/file", "x", None).await), 409);
            // Nothing to read, list or remove.
            assert_eq!(status(client.read(&root, "missing", 0, None).await), 404);
            assert_eq!(status(client.read_dir(&root, "missing").await), 404);
            assert_eq!(status(client.remove(&root, "missing").await), 404);
            // A root that does not exist, and a snapshot in use.
            let nowhere = FileRoot::Volume("nowhere".into());
            assert_eq!(status(client.stat(&nowhere, "").await), 404);
            let locked = FileRoot::Snapshot("locked".into());
            assert_eq!(status(client.read_dir(&locked, "").await), 423);
            assert_eq!(status(client.write(&locked, "a", "x", None).await), 423);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_write_with_an_etag_goes_only_over_the_file_that_has_it() {
        for client in clients().await {
            let root = volume();
            client.write(&root, "a", "A", None).await.unwrap();
            let etag = client.stat(&root, "a").await.unwrap().etag.unwrap();
            client.write(&root, "a", "BB", Some(&etag)).await.unwrap();
            assert_eq!(
                status(client.write(&root, "a", "C", Some(&etag)).await),
                412
            );
            assert_eq!(client.read(&root, "a", 0, None).await.unwrap(), "BB");
            assert_ne!(client.stat(&root, "a").await.unwrap().etag.unwrap(), etag);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_rename_and_a_copy_replace_only_when_told_to() {
        for client in clients().await {
            let root = FileRoot::Snapshot("s".into());
            client.write(&root, "a", "A", None).await.unwrap();
            client.write(&root, "b", "B", None).await.unwrap();

            assert_eq!(status(client.rename(&root, "a", "b", false).await), 412);
            assert_eq!(client.read(&root, "a", 0, None).await.unwrap(), "A");
            client.rename(&root, "a", "b", true).await.unwrap();
            assert_eq!(status(client.stat(&root, "a").await), 404);
            assert_eq!(client.read(&root, "b", 0, None).await.unwrap(), "A");

            client.create_dir(&root, "dir").await.unwrap();
            client.write(&root, "dir/c", "C", None).await.unwrap();
            client.copy(&root, "dir", "copy", false).await.unwrap();
            assert_eq!(client.read(&root, "copy/c", 0, None).await.unwrap(), "C");
            assert_eq!(client.read(&root, "dir/c", 0, None).await.unwrap(), "C");
            assert_eq!(status(client.copy(&root, "dir", "copy", false).await), 412);
            client.copy(&root, "b", "copy/c", true).await.unwrap();
            assert_eq!(client.read(&root, "copy/c", 0, None).await.unwrap(), "A");

            // A directory moves with everything in it, onto one there only when told to.
            client.create_dir_all(&root, "dir/sub").await.unwrap();
            assert_eq!(
                status(client.rename(&root, "dir", "copy", false).await),
                412
            );
            client.rename(&root, "dir", "moved", false).await.unwrap();
            assert_eq!(status(client.stat(&root, "dir").await), 404);
            assert_eq!(client.read(&root, "moved/c", 0, None).await.unwrap(), "C");
            assert!(client.stat(&root, "moved/sub").await.unwrap().is_dir);
            client.rename(&root, "moved", "copy", true).await.unwrap();
            assert_eq!(client.read(&root, "copy/c", 0, None).await.unwrap(), "C");
            assert!(client.stat(&root, "copy/sub").await.unwrap().is_dir);
            client.close().await;
        }
    }

    #[tokio::test]
    async fn a_name_that_needs_encoding_round_trips() {
        for client in clients().await {
            let root = volume();
            let name = "한글 & spaces #1.txt";
            client.create_dir(&root, "dir 2").await.unwrap();
            client
                .write(&root, &format!("dir 2/{name}"), "x", None)
                .await
                .unwrap();
            let entries = client.read_dir(&root, "dir 2").await.unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, name);
            assert_eq!(
                client
                    .read(&root, &format!("dir 2/{name}"), 0, None)
                    .await
                    .unwrap(),
                "x"
            );
            client.close().await;
        }
    }
}
