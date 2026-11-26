//! The fake agent's script steps. `sy-fake-agent` keeps its own copy of
//! this shape (it depends on no workspace crate), so a change here goes there too.

use serde::{Deserialize, Serialize};
use sy_adapter_api::serde_json::Value;
use sy_adapter_api::{AgentEvent, ApprovalDetail, FileChange, LiveStatus, MessageRole};

/// The most a `flood` step may write per second.
pub const MAX_FLOOD_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;

/// One step. Each waits on an event (its own work, or input), never the clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Write this text to the terminal.
    Print(String),
    /// Write these base64 bytes to the terminal.
    PrintB64(String),
    /// Report a status through `sy-signal --hook status`.
    Status(Status),
    /// Report the agent's session id through `sy-signal --hook session`.
    Session(String),
    /// Journal an event through `sy-signal --hook event`.
    Event(Event),
    /// Ask for permission through `sy-signal --hook approval` and block,
    /// then run the branch for the answer.
    Approval(Approval),
    /// Read input until a line contains this literal.
    ExpectInput(String),
    /// Echo every input byte until this literal is seen.
    EchoUntil(String),
    /// Write seeded printable bytes at a rate.
    Flood(Flood),
    /// Write a file inside the cwd.
    WriteFile(WriteFile),
    /// Delete a file inside the cwd.
    DeleteFile(DeleteFile),
    /// Block forever and ignore input (`true` only).
    Hang(bool),
    /// SIGHUP no longer ends the agent (`true` only).
    IgnoreSighup(bool),
    /// Send itself this signal.
    DieBySignal(Signal),
    /// Exit with this code.
    Exit(i32),
}

/// A `status` step's value, and the `status` hook's stdin (as JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Working.
    Running,
    /// Wants you.
    WaitingInput,
}

impl From<Status> for LiveStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Running => Self::Running,
            Status::WaitingInput => Self::WaitingInput,
        }
    }
}

/// An `event` step's value, and the `event` hook's stdin (as JSON): spec
/// 0002's payload shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// A message in the conversation.
    Message {
        /// Who wrote it.
        role: MessageRole,
        /// The text.
        text: String,
    },
    /// A command the agent ran.
    CommandRun {
        /// The argument array.
        argv: Vec<String>,
        /// Its exit code, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// A file the agent changed.
    FileChanged {
        /// Its path in the worktree.
        path: String,
        /// How it changed.
        change: FileChange,
        /// The old path, for a rename.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_path: Option<String>,
    },
}

impl From<Event> for AgentEvent {
    fn from(event: Event) -> Self {
        match event {
            Event::Message { role, text } => Self::Message { role, text },
            Event::CommandRun { argv, exit_code } => Self::CommandRun { argv, exit_code },
            Event::FileChanged {
                path,
                change,
                from_path,
            } => Self::FileChanged {
                path,
                change,
                from_path,
            },
        }
    }
}

/// An `approval` step's value. The hook's stdin is its `kind`, `summary`
/// and `detail` (see [`ApprovalAsk`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// `command`, `file_edit`, `fetch`, `mcp_tool` or `other`.
    pub kind: String,
    /// One line for people.
    pub summary: String,
    /// The kind's fields.
    #[serde(default)]
    pub detail: Value,
    /// Steps to run when approved.
    #[serde(default)]
    pub on_approve: Vec<Step>,
    /// Steps to run when denied.
    #[serde(default)]
    pub on_deny: Vec<Step>,
}

/// The `approval` hook's stdin (as JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAsk {
    /// See [`Approval::kind`].
    pub kind: String,
    /// One line for people.
    pub summary: String,
    /// The kind's fields.
    #[serde(default)]
    pub detail: Value,
}

fn field(detail: &Value, name: &str) -> Result<String, String> {
    detail
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("detail.{name} must be a string"))
}

fn optional(detail: &Value, name: &str) -> Option<String> {
    detail.get(name).and_then(Value::as_str).map(str::to_owned)
}

/// The typed detail for an approval's `kind` and `detail`.
pub fn approval_detail(kind: &str, detail: &Value) -> Result<ApprovalDetail, String> {
    let input = || detail.get("input").cloned().unwrap_or(Value::Null);
    Ok(match kind {
        "command" => ApprovalDetail::Command {
            command: field(detail, "command")?,
            cwd: optional(detail, "cwd"),
        },
        "file_edit" => ApprovalDetail::FileEdit {
            path: field(detail, "path")?,
            diff: optional(detail, "diff"),
        },
        "fetch" => ApprovalDetail::Fetch {
            url: field(detail, "url")?,
        },
        "mcp_tool" => ApprovalDetail::McpTool {
            server: field(detail, "server")?,
            tool: field(detail, "tool")?,
            input: input(),
        },
        "other" => ApprovalDetail::Other {
            tool: field(detail, "tool")?,
            input: input(),
        },
        other => return Err(format!("unknown approval kind {other:?}")),
    })
}

/// `flood` step fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flood {
    /// The output rate.
    pub bytes_per_sec: u64,
    /// Stop after this many bytes; forever when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    /// Write input straight back as output.
    #[serde(default)]
    pub echo: bool,
}

/// `write_file` step fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteFile {
    /// A relative path inside the cwd.
    pub path: String,
    /// Its text.
    pub contents: String,
}

/// `delete_file` step fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteFile {
    /// A relative path inside the cwd.
    pub path: String,
}

/// The signals `die_by_signal` may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// A segmentation fault.
    #[serde(rename = "SIGSEGV")]
    Segv,
    /// Kill.
    #[serde(rename = "SIGKILL")]
    Kill,
    /// Terminate.
    #[serde(rename = "SIGTERM")]
    Term,
}

fn inside_cwd(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('/') && !path.split('/').any(|part| part == "..")
}

/// Checks serde can't: paths stay inside the cwd, flags are `true`, rates fit.
pub fn validate(steps: &[Step]) -> Result<(), String> {
    for (i, step) in steps.iter().enumerate() {
        let problem = match step {
            Step::WriteFile(WriteFile { path, .. }) | Step::DeleteFile(DeleteFile { path })
                if !inside_cwd(path) =>
            {
                Some(format!("path {path:?} must be relative with no `..`"))
            }
            Step::Hang(false) | Step::IgnoreSighup(false) => Some("must be true".to_owned()),
            Step::Flood(f) if f.bytes_per_sec == 0 || f.bytes_per_sec > MAX_FLOOD_BYTES_PER_SEC => {
                Some(format!(
                    "bytes_per_sec must be 1 to {MAX_FLOOD_BYTES_PER_SEC}"
                ))
            }
            Step::Approval(a) => approval_detail(&a.kind, &a.detail)
                .err()
                .or_else(|| validate(&a.on_approve).err())
                .or_else(|| validate(&a.on_deny).err()),
            _ => None,
        };
        if let Some(problem) = problem {
            return Err(format!("step {i}: {problem}"));
        }
    }
    Ok(())
}
