//! How an argument names an image: a ref in any of its spellings, an image ID or a digest.
//!
//! The server lists one row per tag; an [`Image`] is those rows gathered by digest, which is
//! what a user means by one image.

use serde::Serialize;

use crate::ImageListOutputItem;

/// One image the server holds, and every ref on it, as the server spells each.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Image {
    pub(crate) digest: String,
    pub(crate) refs: Vec<String>,
}

impl Image {
    /// The image with each ref as a user spells it, for showing.
    pub(crate) fn familiar(&self) -> Image {
        Image {
            digest: self.digest.clone(),
            refs: self.refs.iter().map(|r| familiar_ref(r)).collect(),
        }
    }
}

/// The listed tags gathered by digest, each image where its first ref is listed.
pub(crate) fn gather(listed: Vec<ImageListOutputItem>) -> Vec<Image> {
    let mut images: Vec<Image> = Vec::new();
    for ImageListOutputItem { reference, digest } in listed {
        match images.iter_mut().find(|image| image.digest == digest) {
            Some(image) => image.refs.push(reference),
            None => images.push(Image {
                digest,
                refs: vec![reference],
            }),
        }
    }
    images
}

/// What an argument named among the listed images.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Named<'a> {
    /// One ref on `image`, as the server spells it.
    Ref {
        image: &'a Image,
        reference: &'a str,
    },
    /// The image itself, by its ID or digest.
    Image(&'a Image),
    /// The image in one repository, by `name@digest`: `repository` as the server spells it.
    Digest {
        image: &'a Image,
        repository: &'a str,
    },
}

impl<'a> Named<'a> {
    pub(crate) fn image(&self) -> &'a Image {
        match self {
            Named::Ref { image, .. } | Named::Image(image) | Named::Digest { image, .. } => image,
        }
    }

    /// The reference the server takes for it: the ref, or `repository@digest`, in the
    /// repository of the image's first ref for an image named by its ID, since the server holds
    /// a digest per repository.
    pub(crate) fn reference(&self) -> String {
        match self {
            Named::Ref { reference, .. } => (*reference).to_string(),
            Named::Image(image) => format!("{}@{}", repository(&image.refs[0]), image.digest),
            Named::Digest { image, repository } => format!("{repository}@{}", image.digest),
        }
    }
}

/// What `name` names among `images`.
///
/// A hex string, with or without `sha256:`, that begins exactly one image's digest is that
/// image, as an ID from `images` names one; one that begins several is refused rather than
/// guessed. `name@digest` is the image of that digest in that repository, the digest being the
/// one the server lists, not a registry's. Anything else is a ref: the one spelled exactly so,
/// else the one it spells with what `docker` fills in (`alpine` is
/// `docker.io/library/alpine:latest`).
pub(crate) fn lookup<'a>(name: &str, images: &'a [Image]) -> anyhow::Result<Named<'a>> {
    let prefix = name.strip_prefix("sha256:").unwrap_or(name);
    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        let matched: Vec<&Image> = images
            .iter()
            .filter(|image| {
                image
                    .digest
                    .strip_prefix("sha256:")
                    .is_some_and(|hex| hex.starts_with(prefix))
            })
            .collect();
        match matched.as_slice() {
            [image] => return Ok(Named::Image(image)),
            [] => {}
            _ => anyhow::bail!("{name} begins {} image IDs", matched.len()),
        }
    }

    if let Some((_, digest)) = name.split_once('@') {
        let wanted = canonical_ref(name);
        let found = images
            .iter()
            .filter(|image| image.digest == digest)
            .find_map(|image| {
                image
                    .refs
                    .iter()
                    .map(|reference| repository(reference))
                    .find(|repository| canonical_ref(&format!("{repository}@{digest}")) == wanted)
                    .map(|repository| Named::Digest { image, repository })
            });
        return found.ok_or_else(|| anyhow::anyhow!("{name} is not an image the server holds"));
    }

    let refs = || {
        images
            .iter()
            .flat_map(|image| image.refs.iter().map(move |reference| (image, reference)))
    };
    if let Some((image, reference)) = refs().find(|(_, reference)| *reference == name) {
        return Ok(Named::Ref { image, reference });
    }
    let canonical = canonical_ref(name);
    let matched: Vec<_> = refs()
        .filter(|(_, reference)| canonical_ref(reference) == canonical)
        .collect();
    match matched.as_slice() {
        [(image, reference)] => Ok(Named::Ref { image, reference }),
        [] => anyhow::bail!("{name} is not an image the server holds"),
        _ => {
            let spelled: Vec<&str> = matched.iter().map(|(_, r)| r.as_str()).collect();
            anyhow::bail!("{name} could be any of {}", spelled.join(", "))
        }
    }
}

/// The first 12 hex digits of a digest: what `docker images` shows, and enough to name one.
pub(crate) fn image_id(digest: &str) -> &str {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    &hex[..hex.len().min(12)]
}

/// The repository of a `name:tag`: `ghcr.io/owner/app` of `ghcr.io/owner/app:1`.
pub(crate) fn repository(reference: &str) -> &str {
    match reference.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => name,
        _ => reference,
    }
}

/// `reference` with `:latest` when it has no tag, as `docker` reads a bare name; the server
/// takes only `name:tag`.
pub(crate) fn with_tag(reference: &str) -> String {
    if reference.contains('@') || repository(reference) != reference {
        reference.to_string()
    } else {
        format!("{reference}:latest")
    }
}

/// A ref without what [`canonical_ref`] fills in, as `docker` shows one:
/// `docker.io/library/alpine:3.20` is `alpine:3.20`, and `docker.io/user/app:1` is `user/app:1`.
pub(crate) fn familiar_ref(reference: &str) -> String {
    let Some(rest) = reference.strip_prefix("docker.io/") else {
        return reference.to_string();
    };
    match rest.strip_prefix("library/") {
        Some(name) if !name.contains('/') => name.to_string(),
        _ => rest.to_string(),
    }
}

/// A ref as a registry client fills it in: `registry/path:tag`, or `registry/path@digest`.
///
/// A name without a tag or digest is `latest`; without a registry it is on `docker.io`, where
/// a lone name is under `library/`. The first component is a registry when it has a `.` or a
/// `:` or is `localhost`, as `docker` tells `ghcr.io/x` from `user/x`.
pub(crate) fn canonical_ref(reference: &str) -> String {
    let (name, suffix) = match reference.split_once('@') {
        Some((name, digest)) => (name, format!("@{digest}")),
        None => {
            let name = repository(reference);
            match &reference[name.len()..] {
                "" => (name, ":latest".to_string()),
                tag => (name, tag.to_string()),
            }
        }
    };
    let path = match name.split_once('/') {
        Some((registry, rest))
            if registry.contains('.') || registry.contains(':') || registry == "localhost" =>
        {
            if registry == "docker.io" && !rest.contains('/') {
                format!("docker.io/library/{rest}")
            } else {
                name.to_string()
            }
        }
        Some(_) => format!("docker.io/{name}"),
        None => format!("docker.io/library/{name}"),
    };
    format!("{path}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(reference: &str, digest: &str) -> ImageListOutputItem {
        ImageListOutputItem {
            reference: reference.into(),
            digest: digest.into(),
        }
    }

    fn image(digest: &str, refs: &[&str]) -> Image {
        Image {
            digest: digest.into(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn tags_are_gathered_by_digest() {
        let listed = vec![
            item("alpine:3.20", "sha256:aa"),
            item("jq:1", "sha256:bb"),
            item("jq:latest", "sha256:bb"),
            item("mine:1", "sha256:aa"),
        ];
        assert_eq!(
            gather(listed),
            [
                image("sha256:aa", &["alpine:3.20", "mine:1"]),
                image("sha256:bb", &["jq:1", "jq:latest"]),
            ]
        );
    }

    #[test]
    fn a_ref_is_shown_as_a_user_spells_it() {
        assert_eq!(familiar_ref("docker.io/library/alpine:3.20"), "alpine:3.20");
        assert_eq!(familiar_ref("docker.io/user/app:1"), "user/app:1");
        assert_eq!(
            familiar_ref("docker.io/library/deep/app:1"),
            "library/deep/app:1"
        );
        assert_eq!(familiar_ref("ghcr.io/user/app:1"), "ghcr.io/user/app:1");
        for reference in ["alpine:3.20", "user/app:1", "ghcr.io/user/app:latest"] {
            assert_eq!(familiar_ref(&canonical_ref(reference)), reference);
        }
    }

    #[test]
    fn a_name_is_found_as_spelled_or_as_docker_fills_it_in() {
        let images = [
            image("sha256:aa", &["alpine:3.20", "mine:1"]),
            image("sha256:bb", &["docker.io/library/jq:latest"]),
        ];
        // What each name reaches, as the server takes it.
        for (name, reached) in [
            ("mine:1", Some("mine:1")),
            ("jq", Some("docker.io/library/jq:latest")),
            ("docker.io/library/alpine:3.20", Some("alpine:3.20")),
            ("alpine", None),
            (
                "docker.io/library/alpine@sha256:aa",
                Some("alpine@sha256:aa"),
            ),
            // In the repository named, not that of the image's first ref.
            ("mine@sha256:aa", Some("mine@sha256:aa")),
            ("jq@sha256:bb", Some("docker.io/library/jq@sha256:bb")),
            // Not in that repository; a digest is whole, not a prefix; not a digest.
            ("jq@sha256:aa", None),
            ("alpine@sha256:a", None),
            ("app@foo", None),
        ] {
            let found = lookup(name, &images).ok().map(|named| named.reference());
            assert_eq!(found.as_deref(), reached, "{name}");
        }
    }

    #[test]
    fn two_spellings_of_one_ref_are_refused_unless_one_is_exact() {
        let images = [
            image("sha256:aa", &["alpine:3.20"]),
            image("sha256:bb", &["docker.io/library/alpine:3.20"]),
        ];
        assert_eq!(lookup("alpine:3.20", &images).unwrap().image(), &images[0]);
        assert!(lookup("docker.io/alpine:3.20", &images).is_err());
    }

    #[test]
    fn an_image_id_is_a_prefix_that_begins_one_digest() {
        let images = [
            image("sha256:45e09956dc667c5eff", &["alpine:3.20"]),
            image("sha256:45e0aaaa", &["team/base:1", "other:1"]),
            image("sha256:ab96ea8ff122", &["abc:1"]),
        ];
        assert_eq!(lookup("ab96", &images).unwrap(), Named::Image(&images[2]));
        assert_eq!(
            lookup("sha256:45e0aaaa", &images).unwrap(),
            Named::Image(&images[1])
        );
        assert!(lookup("45e0", &images).is_err());
        // Hex, but begins no digest: a ref.
        assert!(lookup("abc", &images).is_err());
        // An image by its digest is reached in the repository of its first ref.
        assert_eq!(
            lookup("45e0a", &images).unwrap().reference(),
            "team/base@sha256:45e0aaaa"
        );
    }
}
