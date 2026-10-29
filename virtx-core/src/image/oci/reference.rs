//! A reference to an image in the server's registry, read into the repository and the tag or
//! digest the registry's paths take.

use std::fmt;

use crate::{Error, image::is_digest};

/// An image in one repository: `name:tag` or `name@algorithm:hex`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reference {
    /// The repository, as `alpine` or `ghcr.io/owner/name`.
    pub(crate) name: String,
    pub(crate) target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Tag(String),
    Digest(String),
}

impl Reference {
    /// `s` as `name:tag` or `name@algorithm:hex`. A name or tag the registry's paths could not
    /// carry as it is, such as one with `..` or `?` in it, is refused.
    pub(crate) fn parse(s: &str) -> Result<Reference, Error> {
        let refused =
            |why: &str| Error::Registry(format!("{s:?} is not an image reference: {why}"));
        let (name, target) = if let Some((name, digest)) = s.split_once('@') {
            if !is_digest(digest) {
                return Err(refused("the digest is not `algorithm:hex`"));
            }
            (name, Target::Digest(digest.to_owned()))
        } else {
            match s.rsplit_once(':') {
                Some((name, tag)) if !tag.contains('/') => {
                    if !is_tag(tag) {
                        return Err(refused("the tag is not one"));
                    }
                    (name, Target::Tag(tag.to_owned()))
                }
                _ => return Err(refused("it has neither a tag nor a digest")),
            }
        };
        if !is_name(name) {
            return Err(refused("the name is not a repository's"));
        }
        Ok(Reference {
            name: name.to_owned(),
            target,
        })
    }

    /// `s` as `name:tag`, refusing a digest.
    pub(crate) fn parse_tag(s: &str) -> Result<(String, String), Error> {
        match Reference::parse(s)? {
            Reference {
                name,
                target: Target::Tag(tag),
            } => Ok((name, tag)),
            _ => Err(Error::Registry(format!("{s:?} is not `name:tag`"))),
        }
    }

    /// The tag or digest, as a manifest's path ends with it.
    pub(crate) fn target(&self) -> &str {
        match &self.target {
            Target::Tag(tag) => tag,
            Target::Digest(digest) => digest,
        }
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target {
            Target::Tag(tag) => write!(f, "{}:{tag}", self.name),
            Target::Digest(digest) => write!(f, "{}@{digest}", self.name),
        }
    }
}

/// Whether `name` is a repository's: components of lower-case letters, digits and the
/// separators `.`, `_` and `-`, joined by `/`, each starting and ending with a letter or digit.
fn is_name(name: &str) -> bool {
    let alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    !name.is_empty()
        && name.split('/').all(|component| {
            let bytes = component.as_bytes();
            !bytes.is_empty()
                && alphanumeric(bytes[0])
                && alphanumeric(bytes[bytes.len() - 1])
                && bytes
                    .iter()
                    .all(|&b| alphanumeric(b) || matches!(b, b'.' | b'_' | b'-'))
                && !component.contains("..")
        })
}

/// Whether `tag` is one: `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`.
fn is_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    (1..=128).contains(&bytes.len())
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_reference_is_a_name_and_a_tag_or_a_digest() {
        let cases = [
            ("alpine:3.20", "alpine", Target::Tag("3.20".into())),
            (
                "ghcr.io/owner/jq:latest",
                "ghcr.io/owner/jq",
                Target::Tag("latest".into()),
            ),
            (
                &format!("team/base@{DIGEST}"),
                "team/base",
                Target::Digest(DIGEST.into()),
            ),
        ];
        for (s, name, target) in cases {
            let reference = Reference::parse(s).unwrap();
            assert_eq!(reference.name, name, "{s}");
            assert_eq!(reference.target, target, "{s}");
            assert_eq!(reference.to_string(), s);
        }
    }

    #[test]
    fn what_a_path_could_not_carry_is_refused() {
        for s in [
            "alpine",
            "host:5000/alpine",
            "Alpine:3.20",
            "../alpine:3.20",
            "a//b:1",
            "alpine:3.20?n=1",
            "alpine:.hidden",
            "alpine@sha256:0123",
            ":latest",
        ] {
            assert!(Reference::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn a_tag_is_asked_for_by_name() {
        assert_eq!(
            Reference::parse_tag("alpine:3.20").unwrap(),
            ("alpine".into(), "3.20".into())
        );
        assert!(Reference::parse_tag(&format!("alpine@{DIGEST}")).is_err());
    }
}
