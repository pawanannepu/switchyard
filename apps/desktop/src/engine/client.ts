import { createEngineClient, webSocketTransport, type Transport } from "@switchyard/protocol";
import { restartEngineViaHost, tauriTransport } from "./tauriTransport.ts";

const inTauri = "__TAURI_INTERNALS__" in window;

/**
 * Inside the app: the Tauri channel. In a plain browser (dev and Playwright):
 * the dev bridge, from `?bridge=<port>&token=<token>` on the page URL.
 */
function pickTransport(): Transport {
  if (inTauri) return tauriTransport();
  const query = new URLSearchParams(window.location.search);
  const port = query.get("bridge");
  const token = query.get("token");
  if (!port || !token) {
    throw new Error("Not running inside switchyard. Open this page with ?bridge=<port>&token=<token> from sy-devbridge.");
  }
  return webSocketTransport(`ws://127.0.0.1:${port}/?token=${encodeURIComponent(token)}`);
}

export const engine = createEngineClient(pickTransport());

/** Only the app host can start the engine; the dev bridge never does. */
export const canRestartEngine = inTauri;

export function restartEngine(): Promise<void> {
  return canRestartEngine ? restartEngineViaHost() : Promise.resolve();
}
