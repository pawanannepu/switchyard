import type { HostConnection } from "@switchyard/protocol";
import { canRestartEngine, restartEngine } from "../engine/client.ts";
import { useEngineStore } from "../engine/store.ts";
import { strings } from "../strings.ts";

const s = strings.connection;

function stateLabel(connection: HostConnection | null): string {
  switch (connection?.state) {
    case undefined:
    case "connecting":
      return s.connecting;
    case "connected":
      return s.connected;
    case "stopped":
      return s.stopped;
    case "failed":
      return s.failed;
    case "incompatible":
      return s.incompatible;
  }
}

export function Home() {
  const connection = useEngineStore((state) => state.connection);
  const info = useEngineStore((state) => state.info);
  const hello = connection?.hello;
  const canAct = connection?.state === "stopped" || connection?.state === "failed";

  return (
    <section className="panel" aria-labelledby="engine-heading">
      <h2 id="engine-heading">{s.heading}</h2>
      <p className={`status status-${connection?.state ?? "connecting"}`} role="status" aria-live="polite">
        {stateLabel(connection)}
      </p>
      {connection?.message && <p className="message">{connection.message}</p>}

      {hello && (
        <dl className="facts">
          <dt>{s.version}</dt>
          <dd data-testid="engine-version">{hello.engine_version}</dd>
          <dt>{s.protocol}</dt>
          <dd data-testid="engine-protocol">
            {hello.protocol.major}.{hello.protocol.minor}
          </dd>
          <dt>{s.pid}</dt>
          <dd data-testid="engine-pid">{hello.pid}</dd>
          {info && (
            <>
              <dt>{s.running}</dt>
              <dd>{info.running_agents}</dd>
            </>
          )}
        </dl>
      )}

      {canAct &&
        (canRestartEngine ? (
          <button type="button" onClick={() => void restartEngine()}>
            {connection.state === "failed" ? s.retry : s.restart}
          </button>
        ) : (
          <p className="message">{s.restartUnavailable}</p>
        ))}

      {canAct && connection.logs_dir && (
        <p className="message">
          {s.logs}: <code>{connection.logs_dir}</code> (<code>engine.log</code>, <code>engine.stderr.log</code>)
        </p>
      )}
    </section>
  );
}
