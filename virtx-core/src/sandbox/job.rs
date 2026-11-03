//! Sandbox jobs: a process run in a sandbox, what starting one takes, and the entries of its
//! log.

use serde::{Deserialize, Serialize};

/// A sandbox job, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxJob {
    /// The job's id, which every request about it names it by.
    pub id: String,

    pub kind: SandboxJobKind,

    /// Its process's pid in the sandbox. Not what names the job: a pid the sandbox gives again
    /// never names another.
    pub pid: u32,

    /// The argv it was started with.
    pub cmd: Vec<String>,

    /// Whether its process runs still.
    pub running: bool,

    /// Once it has ended, its exit status; `128 + signal` for one a signal ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

/// What a sandbox job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxJobKind {
    /// Runs a process.
    Exec,
}

/// What starting a process takes: `POST /sandboxes/{id}/jobs/exec/`'s body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxExecArgs {
    /// The argv, with no shell: `["sh", "-c", "..."]` for one. An empty one is refused.
    pub cmd: Vec<String>,

    /// Whether the client writes to its stdin. Without it, its stdin is empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stdin: bool,

    /// Milliseconds before it is killed, with no grace period. Without it, no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl SandboxExecArgs {
    /// `cmd`, with an empty stdin and no limit.
    pub fn new(cmd: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            cmd: cmd.into_iter().map(Into::into).collect(),
            stdin: false,
            timeout_ms: None,
        }
    }
}

/// What signalling a process takes: `POST .../jobs/{job}/signal`'s body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSignalArgs {
    /// A name as Linux gives it, such as `SIGTERM`.
    pub signal: String,
}

/// What writing to a process's stdin takes: `POST .../jobs/{job}/stdin`'s body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxStdinArgs {
    /// Written as it is, as UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,

    /// Close the stdin, after `data`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub eof: bool,
}

/// One entry of a sandbox job's log. As JSON, an object of one member, as `{"stdout": "..."}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxJobEntry {
    /// A line the process wrote to stdout, without its line ending.
    Stdout(String),

    /// A line the process wrote to stderr, without its line ending.
    Stderr(String),

    /// The log reached the server's limit: nothing it wrote after is kept or sent.
    Truncated {},

    /// How it ended, last.
    End(SandboxJobEnd),
}

/// How a process ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxJobEnd {
    /// Its exit status; `128 + signal` for one a signal ended.
    pub code: i32,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_entry_is_an_object_of_one_member() {
        let cases = [
            (
                SandboxJobEntry::Stdout("hi".into()),
                json!({"stdout": "hi"}),
            ),
            (SandboxJobEntry::Truncated {}, json!({"truncated": {}})),
            (
                SandboxJobEntry::End(SandboxJobEnd { code: 137 }),
                json!({"end": {"code": 137}}),
            ),
        ];
        for (entry, value) in cases {
            assert_eq!(serde_json::to_value(&entry).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<SandboxJobEntry>(value).unwrap(),
                entry
            );
        }
    }

    #[test]
    fn an_exec_says_only_what_it_asked_for() {
        let value = serde_json::to_value(SandboxExecArgs::new(["true"])).unwrap();
        assert_eq!(value, json!({"cmd": ["true"]}));
    }
}

/// What an exec read with its log yields ([`Client::exec_sandbox_with_log`]): its job, once,
/// first; then each entry of its log, until its end.
///
/// [`Client::exec_sandbox_with_log`]: crate::Client::exec_sandbox_with_log
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxExecItem {
    Job(SandboxJob),
    Log(crate::LogEntry<SandboxJobEntry>),
}
