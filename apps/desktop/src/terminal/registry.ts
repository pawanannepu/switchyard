import { engine } from "../engine/client.ts";
import type { RunTerminal } from "./RunTerminal.ts";

/** Routes `run.output` and `run.resync_needed` to the terminal showing that run. */
const terminals = new Map<string, RunTerminal>();

engine.on("run.output", (chunk) => terminals.get(chunk.run_id)?.handleOutput(chunk));
engine.on("run.resync_needed", ({ run_id }) => terminals.get(run_id)?.handleResync());

export function registerTerminal(terminal: RunTerminal): () => void {
  terminals.set(terminal.runId, terminal);
  return () => {
    if (terminals.get(terminal.runId) === terminal) terminals.delete(terminal.runId);
  };
}
