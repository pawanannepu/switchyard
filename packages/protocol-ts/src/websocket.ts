import type { Transport } from "./client.ts";

/**
 * A transport to `sy-devbridge` (dev and test only). `url` includes the
 * token, for example `ws://127.0.0.1:52011/?token=…`. Frames sent before the
 * socket opens are queued.
 */
export function webSocketTransport(url: string): Transport {
  const socket = new WebSocket(url);
  const queue: string[] = [];
  const handlers = new Set<(message: unknown) => void>();

  socket.addEventListener("open", () => {
    for (const frame of queue.splice(0)) socket.send(frame);
  });
  socket.addEventListener("message", (event: MessageEvent<string>) => {
    let message: unknown;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    for (const handler of handlers) handler(message);
  });

  return {
    send(frame) {
      if (socket.readyState === WebSocket.OPEN) socket.send(frame);
      else queue.push(frame);
    },
    onMessage(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
  };
}
