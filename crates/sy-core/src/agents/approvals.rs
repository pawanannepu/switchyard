//! Approvals on agent runs (spec 0003, AC-9 to AC-11): what a decoder's
//! request becomes as a row, what the engine announces, and why
//! `approval.respond` can fail. Every change itself happens on the run's
//! driver task (see `driver`).

use sy_adapter_api::serde_json::{self, Map, Value};
use sy_adapter_api::{ApprovalDetail, ApprovalRequest};

use crate::ids::{ApprovalId, RunId};
use crate::model::{APPROVAL_DETAIL_BYTES, Approval, ApprovalKind, NewApproval, cut_summary};
use crate::ports::StoreError;

/// An approval change for the engine to send to every client (not `signal`
/// connections). Sent after its commit, so it follows every event committed before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalNotice {
    /// `approval.requested`.
    Requested {
        /// The new approval.
        approval: Approval,
        /// Its `approval_requested` event.
        event_id: Option<i64>,
    },
    /// `approval.decided`.
    Decided {
        /// The approval, out of `pending`.
        approval: Approval,
        /// Its `approval_decided` event; `None` when the change was queued
        /// while storage is degraded.
        event_id: Option<i64>,
    },
}

/// Why `approval.respond` changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RespondError {
    /// No approval has that id.
    #[error("approval not found")]
    NotFound,
    /// It already left `pending` (another answer, the run's end, its hook closing).
    #[error("approval is no longer pending")]
    NotPending,
    /// The store failed.
    #[error(transparent)]
    Store(StoreError),
}

/// The row for a decoder's request: the kind from the detail's variant, the
/// summary cut to 200 characters, and the detail's typed fields plus `raw`,
/// with `raw` cut (and `raw_truncated: true`) so the JSON fits 64 KiB.
pub fn new_approval(run: &RunId, request: ApprovalRequest) -> NewApproval {
    let kind = ApprovalKind::parse(request.detail.kind()).unwrap_or(ApprovalKind::Other);
    NewApproval {
        id: ApprovalId::new(),
        run_id: run.clone(),
        kind,
        summary: cut_summary(&request.summary),
        detail: detail_json(request.detail, request.raw),
    }
}

fn typed_fields(detail: ApprovalDetail) -> Map<String, Value> {
    let mut map = Map::new();
    let mut put = |key: &str, value: Value| {
        map.insert(key.to_owned(), value);
    };
    match detail {
        ApprovalDetail::Command { command, cwd } => {
            put("command", Value::String(command));
            if let Some(cwd) = cwd {
                put("cwd", Value::String(cwd));
            }
        }
        ApprovalDetail::FileEdit { path, diff } => {
            put("path", Value::String(path));
            if let Some(diff) = diff {
                put("diff", Value::String(diff));
            }
        }
        ApprovalDetail::Fetch { url } => put("url", Value::String(url)),
        ApprovalDetail::McpTool {
            server,
            tool,
            input,
        } => {
            put("server", Value::String(server));
            put("tool", Value::String(tool));
            put("input", input);
        }
        ApprovalDetail::Other { tool, input } => {
            put("tool", Value::String(tool));
            put("input", input);
        }
    }
    map
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut_bytes(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn detail_json(detail: ApprovalDetail, raw: Value) -> Value {
    let mut map = typed_fields(detail);
    map.insert("raw".to_owned(), raw);
    let whole = Value::Object(map);
    if whole.to_string().len() <= APPROVAL_DETAIL_BYTES {
        return whole;
    }
    let Value::Object(mut map) = whole else {
        return whole;
    };
    // `raw` becomes the start of its own JSON text, kept as a string.
    let raw_text = map.remove("raw").unwrap_or(Value::Null).to_string();
    map.insert("raw_truncated".to_owned(), Value::Bool(true));
    map.insert("raw".to_owned(), Value::String(String::new()));
    let base = Value::Object(map.clone()).to_string().len();
    let Some(room) = APPROVAL_DETAIL_BYTES.checked_sub(base) else {
        // The typed fields alone are over: keep them, drop `raw`.
        map.insert("raw".to_owned(), Value::Null);
        return Value::Object(map);
    };
    // Bytes `raw_text[..n]` takes inside its quotes once escaped; grows with `n`.
    let encoded = |n: usize| {
        serde_json::to_string(cut_bytes(&raw_text, n)).map_or(usize::MAX, |s| s.len() - 2)
    };
    let (mut lo, mut hi) = (0, raw_text.len());
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if encoded(mid) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    map.insert(
        "raw".to_owned(),
        Value::String(cut_bytes(&raw_text, lo).to_owned()),
    );
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sy_adapter_api::serde_json::json;

    fn request(detail: ApprovalDetail, raw: Value) -> NewApproval {
        new_approval(
            &RunId::new(),
            ApprovalRequest {
                key: "k".into(),
                summary: "é".repeat(250),
                detail,
                raw,
            },
        )
    }

    #[test]
    fn each_detail_variant_should_keep_its_typed_fields_and_raw() {
        let made = [
            request(
                ApprovalDetail::Command {
                    command: "ls".into(),
                    cwd: Some("/w".into()),
                },
                json!({ "id": 1 }),
            ),
            request(
                ApprovalDetail::FileEdit {
                    path: "a.rs".into(),
                    diff: None,
                },
                Value::Null,
            ),
            request(
                ApprovalDetail::McpTool {
                    server: "gh".into(),
                    tool: "pr".into(),
                    input: json!({ "n": 2 }),
                },
                json!([]),
            ),
        ];
        assert_eq!(
            made.map(|a| (a.kind, a.detail, a.summary.chars().count())),
            [
                (
                    ApprovalKind::Command,
                    json!({ "command": "ls", "cwd": "/w", "raw": { "id": 1 } }),
                    200
                ),
                (
                    ApprovalKind::FileEdit,
                    json!({ "path": "a.rs", "raw": null }),
                    200
                ),
                (
                    ApprovalKind::McpTool,
                    json!({ "server": "gh", "tool": "pr", "input": { "n": 2 }, "raw": [] }),
                    200
                ),
            ]
        );
    }

    #[test]
    fn a_raw_payload_over_64_kib_should_be_cut_to_fit_and_flagged() {
        // Quotes and newlines grow when written as a JSON string.
        for filler in ["x", "\"", "\n", "é"] {
            let raw = json!({ "text": filler.repeat(70 * 1024) });
            let made = request(
                ApprovalDetail::Fetch {
                    url: "https://example.com".into(),
                },
                raw.clone(),
            );
            let text = made.detail.to_string();
            let kept = made.detail["raw"].as_str().unwrap_or_default();
            assert!(
                text.len() <= APPROVAL_DETAIL_BYTES,
                "{filler:?}: {}",
                text.len()
            );
            assert!(
                raw.to_string().starts_with(kept) && kept.len() > 1024,
                "{filler:?}"
            );
            assert_eq!(
                (&made.detail["url"], &made.detail["raw_truncated"]),
                (&json!("https://example.com"), &json!(true))
            );
        }
    }
}
