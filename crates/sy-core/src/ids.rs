//! Typed ids: `<prefix>_<32 lowercase hex chars of a UUIDv7>` (spec 0002, AC-16).
//!
//! Ids are opaque. They sort roughly by creation time because UUIDv7 starts
//! with a millisecond timestamp, but nothing may depend on that.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a string is not a valid id of the expected kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The prefix before `_` is not the one this id kind uses.
    #[error("expected an id starting with `{expected}_`")]
    WrongPrefix {
        /// The prefix this id kind uses.
        expected: &'static str,
    },
    /// The part after the prefix is not 32 lowercase hex characters.
    #[error("id must end in 32 lowercase hex characters")]
    BadHex,
}

const HEX_LEN: usize = 32;

fn parse_with_prefix(prefix: &'static str, text: &str) -> Result<String, IdError> {
    let hex = text
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .ok_or(IdError::WrongPrefix { expected: prefix })?;
    let valid = hex.len() == HEX_LEN
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
        return Err(IdError::BadHex);
    }
    Ok(text.to_owned())
}

macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            /// The prefix every id of this kind starts with (before the `_`).
            pub const PREFIX: &'static str = $prefix;

            /// A fresh id from a new UUIDv7.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::now_v7().simple()))
            }

            /// Parse an id, checking its prefix and hex part.
            pub fn parse(text: &str) -> Result<Self, IdError> {
                parse_with_prefix($prefix, text).map(Self)
            }

            /// The id as text, as stored and sent on the wire.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(text: &str) -> Result<Self, IdError> {
                Self::parse(text)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let text = String::deserialize(d)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }
    };
}

typed_id!(
    /// Identifies a project (one git repository).
    ProjectId,
    "prj"
);
typed_id!(
    /// Identifies a task (the intent: a prompt against a base ref).
    TaskId,
    "tsk"
);
typed_id!(
    /// Identifies a workspace (one worktree, branch and agent for a task).
    WorkspaceId,
    "wsp"
);
typed_id!(
    /// Identifies a run (one process and its terminal output).
    RunId,
    "run"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_id_should_round_trip_through_parse() {
        let id = RunId::new();
        assert_eq!(RunId::parse(id.as_str()), Ok(id));
    }

    #[test]
    fn new_id_should_be_prefix_plus_32_lowercase_hex() {
        let id = WorkspaceId::new().to_string();
        let hex = id.strip_prefix("wsp_").unwrap();
        assert!(
            hex.len() == 32
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn parse_should_reject_another_kinds_prefix() {
        let task = TaskId::new();
        assert_eq!(
            ProjectId::parse(task.as_str()),
            Err(IdError::WrongPrefix { expected: "prj" })
        );
    }

    #[test]
    fn parse_should_reject_bad_hex() {
        let cases = [
            "run_",
            "run_0123",
            "run_0123456789ABCDEF0123456789abcdef",
            "run_0123456789abcdef0123456789abcdeg",
            "run_0123456789abcdef0123456789abcdef0",
        ];
        assert!(
            cases
                .iter()
                .all(|c| RunId::parse(c) == Err(IdError::BadHex))
        );
    }

    #[test]
    fn parse_should_reject_the_old_scaffold_ids() {
        assert!(RunId::parse("run-1").is_err());
    }
}
