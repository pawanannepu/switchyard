import { Channel, invoke } from "@tauri-apps/api/core";
import type { Transport } from "@switchyard/protocol";

/**
 * The shipped transport: frames go to the app host with `engine_send`; engine
 * frames come back on one Tauri channel registered with `engine_subscribe`.
 * The host rewrites request ids and does `session.hello` itself.
 */
export function tauriTransport(): Transport {
  const handlers = new Set<(message: unknown) => void>();
  const channel = new Channel<unknown>();
  channel.onmessage = (message) => {
    for (const handler of handlers) handler(message);
  };
  const subscribed = invoke("engine_subscribe", { onMessage: channel });

  return {
    send(frame) {
      void subscribed.then(() => invoke("engine_send", { frame }));
    },
    onMessage(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
  };
}

export function restartEngineViaHost(): Promise<void> {
  return invoke("engine_restart");
}
