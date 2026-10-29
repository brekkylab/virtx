use serde::{Deserialize, Serialize};

/// An image, by a tag on it or by its digest.
///
/// Tagged rather than told apart by spelling, since `sha256:0123abcd` reads as both a digest
/// and the tag `sha256:0123abcd`.
///
/// As JSON:
///
/// ```text
/// {"type": "tag", "tag": "myimg:latest"}
/// {"type": "digest", "digest": "sha256:0123abcd"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageName {
    /// A tag, as `name:tag`.
    Tag { tag: String },

    /// The digest of the image's manifest, as `algorithm:hex`.
    Digest { digest: String },
}

impl ImageName {
    /// Reads `s` as a digest when it is `sha256:` or `sha512:` followed by exactly as many hex
    /// digits as that algorithm's digest has, and as a tag otherwise.
    ///
    /// A tag spelled that way is read as a digest; build [`ImageName::Tag`] directly
    /// to name one.
    pub fn parse(s: &str) -> Self {
        if is_digest(s) {
            Self::Digest {
                digest: s.to_owned(),
            }
        } else {
            Self::Tag { tag: s.to_owned() }
        }
    }
}

/// Whether `s` is a digest, as [`ImageName::parse`] reads one.
pub(crate) fn is_digest(s: &str) -> bool {
    let Some((algorithm, hex)) = s.split_once(':') else {
        return false;
    };
    let len = match algorithm {
        "sha256" => 64,
        "sha512" => 128,
        _ => return false,
    };
    hex.len() == len && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl std::str::FromStr for ImageName {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl From<&str> for ImageName {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<String> for ImageName {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_full_digest_is_a_digest() {
        assert_eq!(
            ImageName::from(SHA256),
            ImageName::Digest {
                digest: SHA256.to_owned()
            }
        );
    }

    #[test]
    fn anything_else_is_a_tag() {
        for s in [
            "myimg:latest",
            "myimg",
            "sha256:0123abcd",
            "md5:0123456789abcdef0123456789abcdef",
            &SHA256.to_uppercase(),
            &format!("myimg@{SHA256}"),
        ] {
            assert_eq!(
                ImageName::from(s),
                ImageName::Tag { tag: s.to_owned() },
                "{s}"
            );
        }
    }

    #[test]
    fn parse_and_from_string_agree() {
        let parsed: ImageName = "myimg:latest".parse().unwrap();
        assert_eq!(parsed, ImageName::from("myimg:latest".to_owned()));
    }
}
