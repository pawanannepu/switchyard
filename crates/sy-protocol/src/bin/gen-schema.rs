//! Prints `protocol.schema.json` to stdout. Used by `pnpm protocol:gen` and `pnpm protocol:check`.

use std::io::Write;

fn main() -> std::io::Result<()> {
    std::io::stdout().write_all(sy_protocol::schema::generate_pretty().as_bytes())
}
