import { create } from "zustand";
import type { EngineInfo, HostConnection } from "@switchyard/protocol";
import { engine } from "./client.ts";

const INFO_POLL_MS = 3000;

interface EngineState {
  /** Latest `host.connection` from the app host (or dev bridge). */
  connection: HostConnection | null;
  /** Latest `engine.info`, while connected. */
  info: EngineInfo | null;
}

export const useEngineStore = create<EngineState>(() => ({ connection: null, info: null }));

let poll: ReturnType<typeof setInterval> | undefined;

async function refreshInfo(): Promise<void> {
  try {
    const info = await engine.call("engine.info", {});
    useEngineStore.setState({ info });
  } catch {
    // The connection state notification will explain a failure.
  }
}

engine.on("host.connection", (connection) => {
  useEngineStore.setState({ connection, info: connection.state === "connected" ? useEngineStore.getState().info : null });
  clearInterval(poll);
  if (connection.state === "connected") {
    void refreshInfo();
    poll = setInterval(() => void refreshInfo(), INFO_POLL_MS);
  }
});
