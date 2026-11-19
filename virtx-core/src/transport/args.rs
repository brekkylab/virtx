//! A request's JSON body read as the args it carries, and what a server says of one that does not
//! read as them: the member it failed at, what that member takes and what was sent, as
//! `cmd: a list, not "ls"` -- in its own words, not serde's.

use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_path_to_error::Segment;

/// The most of a value sent that a refusal shows, in characters.
const SHOWN: usize = 60;

/// `body` read as a `T`; or why it does not read as one, naming the member it failed at.
pub fn from_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err("the body: a JSON object, and none was sent".to_owned());
    }
    let not_json = || format!("the body: JSON, not {}", shown_text(body));
    let mut json = serde_json::Deserializer::from_slice(body);
    let failed = match serde_path_to_error::deserialize(&mut json) {
        Ok(args) => return json.end().map(|()| args).map_err(|_| not_json()),
        Err(failed) => failed,
    };
    let Ok(sent) = serde_json::from_slice::<Value>(body) else {
        return Err(not_json());
    };

    let path: Vec<&Segment> = failed.path().iter().collect();
    let name = named(&path);
    let found = path.iter().try_fold(&sent, |value, segment| match segment {
        Segment::Seq { index } => value.get(index),
        Segment::Map { key } => value.get(key),
        Segment::Enum { .. } | Segment::Unknown => None,
    });
    let not = found
        .map(|value| format!(", not {}", shown_value(value)))
        .unwrap_or_default();

    // serde's words, without where in the text it failed: what the type it read said.
    let said = failed.inner().to_string();
    let said = match said.rfind(" at line ") {
        Some(at) => &said[..at],
        None => &said,
    };
    let member = |field: &str| match name.as_str() {
        "the body" => field.to_owned(),
        parent => format!("{parent}.{field}"),
    };
    Err(if let Some(field) = quoted(said, "missing field ") {
        format!("{}: required, and not sent", member(field))
    } else if let Some(field) = quoted(said, "duplicate field ") {
        format!("{}: sent twice", member(field))
    } else if quoted(said, "unknown field ").is_some() {
        let members = said
            .split_once(", expected one of ")
            .or_else(|| said.split_once(", expected "))
            .map_or("none", |(_, members)| members);
        let members = members.replace("there are no fields", "none");
        // Its path is the member's own, read to tell it from the members.
        format!("{name}: not a member; the members are {members}")
    } else if let Some((_, kinds)) = said
        .strip_prefix("unknown variant ")
        .and_then(|rest| rest.split_once(", expected "))
    {
        let kinds = kinds.strip_prefix("one of ").unwrap_or(kinds);
        format!("{name}: one of {kinds}{not}")
    } else if let Some(expected) = ["invalid type: ", "invalid value: ", "invalid length "]
        .iter()
        .find_map(|prefix| said.strip_prefix(prefix))
        .and_then(|rest| rest.split_once(", expected "))
        .map(|(_, expected)| expected)
    {
        format!("{name}: {}{not}", described_type(expected))
    } else {
        format!("{name}: {said}{not}")
    })
}

/// What a value of the Rust type `type_name` is, or one serde says it expected, as a client is
/// told it: `u16` is "a whole number up to 65535".
pub fn described_type(type_name: &str) -> String {
    let bare = type_name.rsplit("::").next().unwrap_or(type_name);
    match bare {
        "u8" => "a whole number up to 255".to_owned(),
        "u16" => "a whole number up to 65535".to_owned(),
        "u32" => "a whole number up to 4294967295".to_owned(),
        "u64" | "usize" => "a whole number".to_owned(),
        "i8" | "i16" | "i32" | "i64" | "isize" => "a whole number".to_owned(),
        "f32" | "f64" => "a number".to_owned(),
        "bool" | "a boolean" => "true or false".to_owned(),
        "String" | "str" | "a string" | "a borrowed string" | "a character" => {
            "a string".to_owned()
        }
        "Uuid" => "a UUID".to_owned(),
        _ if bare.starts_with("a sequence") || bare.starts_with("tuple") => "a list".to_owned(),
        _ if [
            "a map",
            "struct ",
            "internally tagged enum ",
            "adjacently tagged enum ",
        ]
        .iter()
        .any(|object| bare.starts_with(object)) =>
        {
            "an object".to_owned()
        }
        _ => type_name.to_owned(),
    }
}

/// The member `path` leads to, as a client spells it: `volumes[0].at`, or "the body".
fn named(path: &[&Segment]) -> String {
    let mut name = String::new();
    for segment in path {
        match segment {
            Segment::Seq { index } => name.push_str(&format!("[{index}]")),
            Segment::Map { key } if name.is_empty() => name.push_str(key),
            Segment::Map { key } => name.push_str(&format!(".{key}")),
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    match name.is_empty() {
        true => "the body".to_owned(),
        false => name,
    }
}

/// The name in backquotes that `said` starts with after `prefix`.
fn quoted<'a>(said: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = said.strip_prefix(prefix)?.strip_prefix('`')?;
    rest.split_once('`').map(|(name, _)| name)
}

/// `value` as JSON, cut at [`SHOWN`] characters.
fn shown_value(value: &Value) -> String {
    let json = value.to_string();
    match json.chars().count() > SHOWN {
        true => format!("{}...", json.chars().take(SHOWN).collect::<String>()),
        false => json,
    }
}

/// `body`, which is not JSON, as a string, cut at [`SHOWN`] characters.
fn shown_text(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    match text.chars().count() > SHOWN {
        true => format!("{:?}...", text.chars().take(SHOWN).collect::<String>()),
        false => format!("{text:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SandboxCreateArgs, SandboxExecArgs, SandboxSignalArgs};

    fn refused<T: DeserializeOwned + std::fmt::Debug>(body: &str) -> String {
        from_body::<T>(body.as_bytes()).expect_err(body)
    }

    #[test]
    fn a_body_that_reads_as_its_args_is_them() {
        let read: SandboxExecArgs = from_body(br#"{"cmd": ["ls", "-l"]}"#).unwrap();
        assert_eq!(read, SandboxExecArgs::new(["ls", "-l"]));
    }

    #[test]
    fn a_body_that_is_not_json_is_refused_saying_so() {
        assert_eq!(
            refused::<SandboxExecArgs>(""),
            "the body: a JSON object, and none was sent"
        );
        assert_eq!(
            refused::<SandboxExecArgs>("not json"),
            r#"the body: JSON, not "not json""#
        );
        assert_eq!(
            refused::<SandboxExecArgs>(r#"{"cmd": ["ls"]} {}"#),
            r#"the body: JSON, not "{\"cmd\": [\"ls\"]} {}""#
        );
    }

    #[test]
    fn a_member_that_does_not_read_is_named_with_what_it_takes_and_what_was_sent() {
        assert_eq!(
            refused::<SandboxExecArgs>("[]"),
            "the body: an object, not []"
        );
        assert_eq!(
            refused::<SandboxExecArgs>(r#"{"cmd": "ls"}"#),
            r#"cmd: a list, not "ls""#
        );
        assert_eq!(
            refused::<SandboxExecArgs>(r#"{"cmd": ["ls", 1]}"#),
            "cmd[1]: a string, not 1"
        );
        assert_eq!(
            refused::<SandboxExecArgs>(r#"{"cmd": ["ls"], "timeout_ms": -1}"#),
            "timeout_ms: a whole number, not -1"
        );
        assert_eq!(
            refused::<SandboxCreateArgs>(r#"{"image": "alpine"}"#),
            r#"image: an object, not "alpine""#
        );
        let image = r#""image": {"type": "tag", "tag": "alpine"}"#;
        assert_eq!(
            refused::<SandboxCreateArgs>(&format!(r#"{{{image}, "ports": ["5900"]}}"#)),
            r#"ports[0]: a whole number up to 65535, not "5900""#
        );
        assert_eq!(
            refused::<SandboxCreateArgs>(&format!(r#"{{{image}, "ports": [70000]}}"#)),
            "ports[0]: a whole number up to 65535, not 70000"
        );
    }

    #[test]
    fn a_member_missing_or_unknown_is_named() {
        assert_eq!(
            refused::<SandboxExecArgs>("{}"),
            "cmd: required, and not sent"
        );
        assert_eq!(
            refused::<SandboxSignalArgs>(r#"{"signl": "TERM"}"#),
            "signl: not a member; the members are `signal`"
        );
        assert_eq!(
            refused::<SandboxExecArgs>(r#"{"cmd": ["ls"], "cmd": ["ls"]}"#),
            "cmd: sent twice"
        );
        assert_eq!(
            refused::<SandboxCreateArgs>(r#"{"image": {"tag": "alpine"}}"#),
            "image.type: required, and not sent"
        );
        assert_eq!(
            refused::<SandboxCreateArgs>(r#"{"image": {"type": "name", "tag": "alpine"}}"#),
            r#"image.type: one of `tag` or `digest`, not "name""#
        );
    }

    #[test]
    fn a_type_is_told_in_words() {
        assert_eq!(described_type("u16"), "a whole number up to 65535");
        assert_eq!(described_type("uuid::Uuid"), "a UUID");
        assert_eq!(described_type("alloc::string::String"), "a string");
        assert_eq!(
            described_type("struct SandboxExecArgs with 3 elements"),
            "an object"
        );
    }
}
