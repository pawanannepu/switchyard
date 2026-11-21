//! `dev.*` methods, compiled only with the `devtools` feature.
//!
//! `dev.synthetic_run` creates fake runs that stream ANSI colored log lines at
//! a set rate and, with `echo`, write every `run.input` straight back as
//! output. The scaffold's load check uses it until feature 4's fake agent lands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sy_core::runs::{RunControl, RunHub, RunWriter};
use sy_protocol::{ErrorCode, RpcError, SyntheticRunParams, SyntheticRunResult};
use tokio::sync::mpsc;

use crate::engine::protocol_run_id;

const MAX_RUNS: u32 = 64;
const MAX_BYTES_PER_SEC: u64 = 64 * 1024 * 1024;
const TICK: Duration = Duration::from_millis(16);

const FILLER: &str =
    "the quick brown fox jumps over the lazy dog while switchyard keeps every agent in its lane ";

pub fn synthetic_run(
    hub: &Arc<RunHub>,
    p: &SyntheticRunParams,
) -> Result<SyntheticRunResult, RpcError> {
    if p.count == 0 || p.count > MAX_RUNS {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!("count must be 1 to {MAX_RUNS}"),
        ));
    }
    if p.bytes_per_sec > MAX_BYTES_PER_SEC {
        return Err(RpcError::new(
            ErrorCode::InvalidParams,
            format!("bytes_per_sec must be at most {MAX_BYTES_PER_SEC}"),
        ));
    }
    let run_ids = (0..p.count)
        .map(|_| {
            let (writer, control) = hub.create_run();
            let id = protocol_run_id(writer.id());
            tokio::spawn(produce(writer, control, p.bytes_per_sec, p.echo));
            id
        })
        .collect();
    tracing::info!(
        count = p.count,
        bytes_per_sec = p.bytes_per_sec,
        echo = p.echo,
        "synthetic runs started"
    );
    Ok(SyntheticRunResult { run_ids })
}

async fn produce(
    writer: RunWriter,
    mut control: mpsc::Receiver<RunControl>,
    bytes_per_sec: u64,
    echo: bool,
) {
    let started = Instant::now();
    let mut last = started;
    let mut budget = 0.0_f64;
    let mut line_no = 0_u64;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    writer.write(
        format!(
            "\x1b[1msynthetic run {}\x1b[0m ({bytes_per_sec} B/s, echo {echo})\r\n",
            writer.id()
        )
        .as_bytes(),
    );

    loop {
        tokio::select! {
            _ = tick.tick(), if bytes_per_sec > 0 => {
                let now = Instant::now();
                let earned = bytes_per_sec as f64 * now.duration_since(last).as_secs_f64();
                budget += earned;
                last = now;
                let mut buf = String::new();
                while (buf.len() as f64) < budget {
                    line_no += 1;
                    let start = usize::try_from(line_no % 40).unwrap_or(0);
                    let filler = &FILLER[start..start + 40];
                    buf.push_str(&format!(
                        "\x1b[2m{:>9.3}s\x1b[0m \x1b[36m{}\x1b[0m \x1b[32mINFO\x1b[0m line {line_no:>7} {filler}\r\n",
                        started.elapsed().as_secs_f64(),
                        writer.id(),
                    ));
                }
                let written = buf.len() as f64;
                budget -= written;
                writer.write(buf.as_bytes());
            }
            message = control.recv() => match message {
                Some(RunControl::Input(data)) if echo => writer.write(&data),
                Some(_) => {}
                None => break,
            }
        }
    }
    tracing::debug!(run = %writer.id(), bytes = writer.total(), "synthetic run stopped");
}
