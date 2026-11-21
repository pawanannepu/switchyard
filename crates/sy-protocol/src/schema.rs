//! Builds `protocol.schema.json`: every wire type under `$defs`, plus the
//! method table, error codes and protocol version as `x-` extensions.

use schemars::{JsonSchema, SchemaGenerator, generate::SchemaSettings};
use serde_json::{Map, Value, json};

use crate::methods::{self, Direction, Kind};
use crate::types::*;
use crate::{ErrorCode, PROTOCOL, RpcError};

fn add<T: JsonSchema>(generator: &mut SchemaGenerator) {
    generator.subschema_for::<T>();
}

/// The full protocol schema as JSON.
pub fn generate() -> Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    add::<ProtocolVersion>(&mut generator);
    add::<ClientKind>(&mut generator);
    add::<Empty>(&mut generator);
    add::<RpcError>(&mut generator);
    add::<HelloParams>(&mut generator);
    add::<HelloResult>(&mut generator);
    add::<EngineInfo>(&mut generator);
    add::<RunId>(&mut generator);
    add::<RunAttachParams>(&mut generator);
    add::<RunAttachResult>(&mut generator);
    add::<RunOutput>(&mut generator);
    add::<RunInputParams>(&mut generator);
    add::<RunResizeParams>(&mut generator);
    add::<RunResyncNeeded>(&mut generator);
    add::<SyntheticRunParams>(&mut generator);
    add::<SyntheticRunResult>(&mut generator);
    add::<StopRunsResult>(&mut generator);
    add::<HostConnectionState>(&mut generator);
    add::<HostConnection>(&mut generator);
    let defs = generator.take_definitions(true);

    let methods: Vec<Value> = methods::ALL
        .iter()
        .map(|m| {
            json!({
                "name": m.name,
                "kind": match m.kind { Kind::Request => "request", Kind::Notification => "notification" },
                "direction": match m.direction {
                    Direction::ClientToEngine => "client_to_engine",
                    Direction::EngineToClient => "engine_to_client",
                    Direction::HostToClient => "host_to_client",
                },
                "params": m.params,
                "result": m.result,
                "stable": m.stable,
                "dev_only": m.dev_only,
            })
        })
        .collect();

    let mut codes = Map::new();
    for code in ErrorCode::ALL {
        codes.insert(code.name().into(), Value::from(code.code()));
    }

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://switchyard.dev/protocol.schema.json",
        "title": "switchyard engine protocol",
        "description": "JSON-RPC 2.0, one UTF-8 message per line, over a user only local socket. Generated from crates/sy-protocol; do not edit.",
        "x-protocol": { "major": PROTOCOL.major, "minor": PROTOCOL.minor },
        "x-limits": {
            "max_line_bytes": crate::MAX_LINE_BYTES,
            "max_input_bytes": crate::MAX_INPUT_BYTES,
            "max_replay_bytes": crate::MAX_REPLAY_BYTES,
            "client_queue_bytes": crate::CLIENT_QUEUE_BYTES,
        },
        "x-methods": methods,
        "x-error-codes": codes,
        "$defs": defs,
    })
}

/// The schema as pretty JSON text with a trailing newline, exactly as committed.
pub fn generate_pretty() -> String {
    let mut text = serde_json::to_string_pretty(&generate()).unwrap_or_default();
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_type_should_exist_in_defs() {
        let schema = generate();
        let defs = &schema["$defs"];
        let missing: Vec<&str> = methods::ALL
            .iter()
            .flat_map(|m| std::iter::once(m.params).chain(m.result))
            .filter(|name| defs.get(*name).is_none())
            .collect();
        assert!(missing.is_empty(), "missing $defs: {missing:?}");
    }

    #[test]
    fn committed_schema_should_match_the_rust_types() {
        let committed = include_str!("../protocol.schema.json");
        assert_eq!(
            committed,
            generate_pretty(),
            "run `pnpm protocol:gen` and commit the result"
        );
    }
}
