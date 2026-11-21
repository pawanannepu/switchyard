//! JSON-RPC 2.0 framing helpers.
//!
//! Incoming lines are classified by shape ([`Message::parse`]) rather than by
//! deserializing into one big enum, so `result: null` and an absent `params`
//! are both handled correctly.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ErrorCode;

/// A JSON-RPC error object. Named `RpcErrorObject` in the schema so the TS
/// `RpcError` class can keep the short name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "RpcErrorObject")]
pub struct RpcError {
    /// A JSON-RPC error code; app codes are listed in [`ErrorCode`].
    pub code: i32,
    /// A short, human readable description.
    pub message: String,
    /// Optional extra detail for the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with `code` and `message` and no extra data.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.code(),
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// One parsed incoming line.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A call that expects a response.
    Request {
        /// Correlates the response; any JSON value the caller chose.
        id: Value,
        /// Method name, such as `run.attach`.
        method: String,
        /// Params, or `Value::Null` when absent.
        params: Value,
    },
    /// A one way message with no `id`.
    Notification {
        /// Notification name, such as `run.output`.
        method: String,
        /// Params, or `Value::Null` when absent.
        params: Value,
    },
    /// The answer to an earlier request.
    Response {
        /// The `id` of the request this answers.
        id: Value,
        /// The `result` on success, or the `error` object.
        outcome: Result<Value, RpcError>,
    },
}

/// Why a line is not a valid JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseFailure {
    /// Not JSON at all.
    Json(String),
    /// JSON, but not a JSON-RPC 2.0 message. Carries the id when one was readable.
    Shape {
        /// The message's `id`, or `Value::Null` when none was readable.
        id: Value,
        /// What was wrong with it.
        reason: &'static str,
    },
}

impl Message {
    /// Parses one line (without its newline).
    pub fn parse(line: &str) -> Result<Self, ParseFailure> {
        let value: Value =
            serde_json::from_str(line).map_err(|e| ParseFailure::Json(e.to_string()))?;
        Self::from_value(value)
    }

    /// Classifies an already parsed JSON value.
    pub fn from_value(value: Value) -> Result<Self, ParseFailure> {
        let Value::Object(mut obj) = value else {
            return Err(ParseFailure::Shape {
                id: Value::Null,
                reason: "message must be an object",
            });
        };
        let id = obj.remove("id");
        if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(ParseFailure::Shape {
                id: id.unwrap_or(Value::Null),
                reason: "jsonrpc must be \"2.0\"",
            });
        }
        match obj.remove("method") {
            Some(Value::String(method)) => {
                let params = obj.remove("params").unwrap_or(Value::Null);
                Ok(match id {
                    Some(id) => Message::Request { id, method, params },
                    None => Message::Notification { method, params },
                })
            }
            Some(_) => Err(ParseFailure::Shape {
                id: id.unwrap_or(Value::Null),
                reason: "method must be a string",
            }),
            None => {
                let id = id.ok_or(ParseFailure::Shape {
                    id: Value::Null,
                    reason: "response without id",
                })?;
                let outcome = match obj.remove("error") {
                    Some(err) => {
                        Err(
                            serde_json::from_value(err).map_err(|_| ParseFailure::Shape {
                                id: id.clone(),
                                reason: "malformed error object",
                            })?,
                        )
                    }
                    None => Ok(obj.remove("result").unwrap_or(Value::Null)),
                };
                Ok(Message::Response { id, outcome })
            }
        }
    }
}

fn envelope() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("jsonrpc".into(), Value::from("2.0"));
    m
}

fn finish(m: Map<String, Value>) -> String {
    // Serializing a Value map cannot fail; fall back to an empty object defensively.
    let mut line = serde_json::to_string(&Value::Object(m)).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    line
}

/// A request line (with trailing newline).
pub fn request_line(id: &Value, method: &str, params: &Value) -> String {
    let mut m = envelope();
    m.insert("id".into(), id.clone());
    m.insert("method".into(), Value::from(method));
    if !params.is_null() {
        m.insert("params".into(), params.clone());
    }
    finish(m)
}

/// A notification line (with trailing newline).
pub fn notification_line(method: &str, params: &impl Serialize) -> String {
    let mut m = envelope();
    m.insert("method".into(), Value::from(method));
    m.insert(
        "params".into(),
        serde_json::to_value(params).unwrap_or(Value::Null),
    );
    finish(m)
}

/// A response line (with trailing newline).
pub fn response_line(id: &Value, outcome: &Result<Value, RpcError>) -> String {
    let mut m = envelope();
    m.insert("id".into(), id.clone());
    match outcome {
        Ok(result) => m.insert("result".into(), result.clone()),
        Err(err) => m.insert(
            "error".into(),
            serde_json::to_value(err).unwrap_or(Value::Null),
        ),
    };
    finish(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_should_read_null_result_as_success() {
        let msg = Message::parse(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        assert_eq!(
            msg,
            Ok(Message::Response {
                id: json!(1),
                outcome: Ok(Value::Null)
            })
        );
    }

    #[test]
    fn parse_should_classify_missing_id_as_notification() {
        let msg = Message::parse(r#"{"jsonrpc":"2.0","method":"run.output","params":{}}"#);
        assert_eq!(
            msg,
            Ok(Message::Notification {
                method: "run.output".into(),
                params: json!({})
            })
        );
    }

    #[test]
    fn parse_should_reject_wrong_version() {
        let msg = Message::parse(r#"{"jsonrpc":"1.0","id":7,"method":"x"}"#);
        assert!(matches!(msg, Err(ParseFailure::Shape { id, .. }) if id == json!(7)));
    }

    #[test]
    fn response_line_should_round_trip() {
        let line = response_line(
            &json!("a"),
            &Err(RpcError::new(ErrorCode::MethodNotFound, "nope")),
        );
        let parsed = Message::parse(line.trim_end());
        assert!(
            matches!(parsed, Ok(Message::Response { outcome: Err(e), .. }) if e.code == -32601)
        );
    }
}
