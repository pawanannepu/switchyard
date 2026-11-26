import "@xterm/xterm/css/xterm.css";
import { useCallback, useEffect, useRef, useState } from "react";
import { RpcError, ErrorCode } from "@switchyard/protocol";
import { engine } from "../engine/client.ts";
import { RunTerminal, type StreamState } from "../terminal/RunTerminal.ts";
import { registerTerminal } from "../terminal/registry.ts";
import { strings } from "../strings.ts";

const s = strings.load;
const RUNS = 12;
const BYTES_PER_SEC = 51_200;
const PROBE_EVERY_MS = 250;
const BUDGET_MS = 100;

interface Stats {
  samples: number;
  p50: number;
  p95: number;
  max: number;
  kibPerSec: number;
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))] ?? 0;
}

function TerminalCell({
  runId,
  onTerminal,
}: {
  runId: string;
  onTerminal: (runId: string, terminal: RunTerminal | null) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<StreamState>("attaching");
  const terminalRef = useRef<RunTerminal | null>(null);

  useEffect(() => {
    if (!host.current) return;
    const terminal = new RunTerminal(engine, runId, host.current);
    terminal.onStateChange = setState;
    terminalRef.current = terminal;
    const unregister = registerTerminal(terminal);
    void terminal.attach();
    onTerminal(runId, terminal);
    return () => {
      onTerminal(runId, null);
      unregister();
      terminal.dispose();
    };
  }, [runId, onTerminal]);

  return (
    <div className="terminal-cell">
      <div className="terminal-host" ref={host} role="region" aria-label={s.terminal(runId)} />
      {state === "paused" && (
        <div className="terminal-paused" role="alert">
          <p>{s.paused}</p>
          <button type="button" onClick={() => terminalRef.current?.resume()}>
            {s.resume}
          </button>
        </div>
      )}
    </div>
  );
}

/** The fake agent floods output at the rate and echoes every input byte (spec 0003). */
const FLOOD_SCRIPT = [{ flood: { bytes_per_sec: BYTES_PER_SEC, echo: true } }];

/** Stop each run; a run that already ended is fine. */
function stopRuns(runIds: readonly string[]): Promise<unknown> {
  return Promise.all(runIds.map((run_id) => engine.call("run.stop", { run_id }).catch(() => {})));
}

export function LoadCheck() {
  const [runIds, setRunIds] = useState<string[]>([]);
  const live = useRef<string[]>([]);
  const [stats, setStats] = useState<Stats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const terminals = useRef(new Map<string, RunTerminal>());
  const latencies = useRef<number[]>([]);

  const onTerminal = useCallback((runId: string, terminal: RunTerminal | null) => {
    if (terminal) {
      terminal.onProbe = (ms) => latencies.current.push(ms);
      terminals.current.set(runId, terminal);
    } else {
      terminals.current.delete(runId);
    }
  }, []);

  useEffect(() => {
    if (runIds.length === 0) return;
    let sequence = 0;
    const startedAt = performance.now();
    const probe = setInterval(() => {
      for (const terminal of terminals.current.values()) terminal.probe(sequence++);
    }, PROBE_EVERY_MS);
    const report = setInterval(() => {
      const sorted = [...latencies.current].sort((a, b) => a - b);
      let drawn = 0;
      for (const terminal of terminals.current.values()) drawn += terminal.bytesDrawn;
      const seconds = (performance.now() - startedAt) / 1000;
      setStats({
        samples: sorted.length,
        p50: percentile(sorted, 50),
        p95: percentile(sorted, 95),
        max: sorted.at(-1) ?? 0,
        kibPerSec: drawn / 1024 / Math.max(seconds, 1),
      });
    }, 1000);
    return () => {
      clearInterval(probe);
      clearInterval(report);
    };
  }, [runIds]);

  async function start() {
    setError(null);
    latencies.current = [];
    setStats(null);
    try {
      const { run_ids } = await engine.call("dev.agent_run", {
        agent_id: "fake",
        count: RUNS,
        options: { script: FLOOD_SCRIPT },
      });
      live.current = run_ids;
      setRunIds(run_ids);
    } catch (err) {
      const missing = err instanceof RpcError && err.code === ErrorCode.MethodNotFound;
      setError(missing ? s.unavailable : String(err));
    }
  }

  async function stop() {
    const runs = live.current;
    live.current = [];
    setRunIds([]);
    await stopRuns(runs);
  }

  useEffect(() => () => void stopRuns(live.current), []);

  const ms = (n: number) => `${n.toFixed(1)} ms`;

  return (
    <section className="panel load-check" aria-labelledby="load-heading">
      <h2 id="load-heading">{s.heading}</h2>
      <p>{s.intro}</p>
      <div className="actions">
        {runIds.length === 0 ? (
          <button type="button" onClick={() => void start()}>
            {s.start}
          </button>
        ) : (
          <button type="button" onClick={() => void stop()}>
            {s.stop}
          </button>
        )}
      </div>
      {error && (
        <p className="message" role="alert">
          {error}
        </p>
      )}
      {stats && (
        <div role="status" aria-live="polite">
          <dl className="facts">
            <dt>{s.samples}</dt>
            <dd data-testid="probe-samples">{stats.samples}</dd>
            <dt>{s.p50}</dt>
            <dd>{ms(stats.p50)}</dd>
            <dt>{s.p95}</dt>
            <dd data-testid="probe-p95">{ms(stats.p95)}</dd>
            <dt>{s.max}</dt>
            <dd>{ms(stats.max)}</dd>
            <dt>{s.throughput}</dt>
            <dd>{stats.kibPerSec.toFixed(0)} KiB/s</dd>
          </dl>
          {stats.samples > 0 && (
            <p className={stats.p95 < BUDGET_MS ? "verdict pass" : "verdict fail"}>
              {stats.p95 < BUDGET_MS ? s.pass : s.fail}
            </p>
          )}
        </div>
      )}
      <div className="terminal-grid">
        {runIds.map((runId) => (
          <TerminalCell key={runId} runId={runId} onTerminal={onTerminal} />
        ))}
      </div>
    </section>
  );
}
