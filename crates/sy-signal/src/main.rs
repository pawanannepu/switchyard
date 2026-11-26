//! `sy-signal`: the agent hook helper (spec 0003, AC-7).
//!
//! ```text
//! sy-signal --socket <path> --token <token> --hook <name>
//!           --fallback-exit <n> --fallback-stdout-b64 <b64> -- [agent args...]
//! ```
//!
//! An agent's hook config runs this once per hook call (the adapter builds
//! the command with `sy_adapter_api::hook_command`). It reads its stdin
//! (capped at 256 KiB), says `session.hello` as a `signal` client on the
//! engine socket, sends one `signal.hook`, writes the reply's stdout bytes
//! and exits with its exit code. The reply may take as long as an approval
//! does; the agent's own hook timeout bounds it.
//!
//! If anything goes wrong (no engine, a failed hello, any error reply, input
//! over the cap, a reply it can't decode) it writes the fallback stdout from
//! its own arguments and exits with the fallback code. Adapters pick a
//! fallback that leaves the agent on its own prompt, so an unreachable engine
//! never approves anything. It never logs: its arguments hold the token.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use base64::Engine as _;
use sy_client::Connection;
use sy_protocol::{
    ClientKind, MAX_SIGNAL_PAYLOAD_BYTES, SignalHookParams, SignalHookResult, methods,
};

/// The exit code when even the fallback arguments are unusable.
const USAGE: i32 = 2;

/// What to write and exit with.
struct Reply {
    stdout: Vec<u8>,
    exit_code: i32,
}

/// The parsed command line. Everything but the fallback is optional here, so
/// a bad call still answers with its fallback.
struct Args {
    fallback: Reply,
    socket: Option<PathBuf>,
    token: Option<String>,
    hook: Option<String>,
    /// The agent's own arguments, after `--`.
    rest: Vec<String>,
}

fn decode(b64: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()
}

fn parse(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut socket, mut token, mut hook, mut exit, mut stdout) = (None, None, None, None, None);
    let mut rest = Vec::new();
    while let Some(flag) = argv.next() {
        if flag == "--" {
            rest.extend(argv.by_ref());
            break;
        }
        let value = argv.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--token" => token = Some(value),
            "--hook" => hook = Some(value),
            "--fallback-exit" => {
                exit = Some(
                    value
                        .parse::<i32>()
                        .map_err(|_| "--fallback-exit must be a number".to_owned())?,
                );
            }
            "--fallback-stdout-b64" => {
                stdout =
                    Some(decode(&value).ok_or("--fallback-stdout-b64 must be base64".to_owned())?);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        fallback: Reply {
            stdout: stdout.ok_or("--fallback-stdout-b64 is required")?,
            exit_code: exit.ok_or("--fallback-exit is required")?,
        },
        socket,
        token,
        hook,
        rest,
    })
}

/// Stdin, up to one byte over the cap (so "over" is visible). A terminal on
/// stdin is no payload: an agent pipes its hook input.
fn read_stdin() -> std::io::Result<Vec<u8>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(Vec::new());
    }
    let mut data = Vec::new();
    stdin
        .lock()
        .take(u64::try_from(MAX_SIGNAL_PAYLOAD_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut data)?;
    Ok(data)
}

/// The engine's reply, or `None` for the fallback.
async fn relay(socket: PathBuf, params: SignalHookParams) -> Option<Reply> {
    let (conn, _notifications) = Connection::connect(&socket).await.ok()?;
    conn.hello(ClientKind::Signal, env!("CARGO_PKG_VERSION"))
        .await
        .ok()?;
    let result: SignalHookResult = conn.call(methods::SIGNAL_HOOK, &params).await.ok()?;
    Some(Reply {
        stdout: decode(&result.stdout_b64)?,
        exit_code: result.exit_code,
    })
}

/// The engine's reply for this call, or `None` for the fallback.
fn answer(args: Args) -> Option<Reply> {
    let (Some(socket), Some(token), Some(hook)) = (args.socket, args.token, args.hook) else {
        return None;
    };
    let stdin = read_stdin().ok()?;
    let size = stdin.len() + args.rest.iter().map(String::len).sum::<usize>();
    if size > MAX_SIGNAL_PAYLOAD_BYTES {
        return None;
    }
    let params = SignalHookParams {
        token,
        hook,
        args: args.rest,
        stdin_b64: base64::engine::general_purpose::STANDARD.encode(&stdin),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(relay(socket, params))
}

fn main() {
    let mut args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("sy-signal: {err}");
            std::process::exit(USAGE);
        }
    };
    let fallback = std::mem::replace(
        &mut args.fallback,
        Reply {
            stdout: Vec::new(),
            exit_code: 0,
        },
    );
    let reply = answer(args).unwrap_or(fallback);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(&reply.stdout)
        .and_then(|()| stdout.flush());
    // `ExitCode` only carries a u8; exit with the reply's full code.
    std::process::exit(reply.exit_code)
}
