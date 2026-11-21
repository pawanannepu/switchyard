import type { Notifications, Requests } from "./generated/methods.ts";

/**
 * Carries frames between the UI and its host. The shipped app uses the Tauri
 * channel; dev and test builds can use a WebSocket to `sy-devbridge`.
 */
export interface Transport {
  /** Send one JSON-RPC frame (a JSON text). */
  send(frame: string): void;
  /** Receive parsed frames. Returns an unsubscribe function. */
  onMessage(handler: (message: unknown) => void): () => void;
}

/** A JSON-RPC error answer from the engine (or the host relaying to it). */
export class RpcError extends Error {
  /** The error code; compare it against the `ErrorCode` table. */
  readonly code: number;
  /** Optional extra detail the engine attached. */
  readonly data: unknown;

  constructor(code: number, message: string, data?: unknown) {
    super(message);
    this.name = "RpcError";
    this.code = code;
    this.data = data;
  }
}

/** The name of any request method, such as `run.attach`. */
export type RequestMethod = keyof Requests;
/** The name of any notification, such as `run.output`. */
export type NotificationName = keyof Notifications;
/** The params type of request `M`. */
export type ParamsOf<M extends RequestMethod> = Requests[M]["params"];
/** The result type of request `M`. */
export type ResultOf<M extends RequestMethod> = Requests[M]["result"];

/** A typed JSON-RPC client for the engine protocol. */
export interface EngineClient {
  /** Call `method` and resolve with its result, or reject with an {@link RpcError}. */
  call<M extends RequestMethod>(method: M, params: ParamsOf<M>): Promise<ResultOf<M>>;
  /** Listen for notification `name`. Returns an unsubscribe function. */
  on<N extends NotificationName>(name: N, handler: (params: Notifications[N]) => void): () => void;
  /** Stop listening and reject everything in flight. */
  close(): void;
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (error: unknown) => void;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** Creates a client that speaks the engine protocol over `transport`. */
export function createEngineClient(transport: Transport): EngineClient {
  let nextId = 1;
  const pending = new Map<number, Pending>();
  const listeners = new Map<string, Set<(params: never) => void>>();

  const unsubscribe = transport.onMessage((message) => {
    if (!isObject(message)) return;
    const { id, method } = message;
    if (typeof method === "string" && id === undefined) {
      for (const handler of listeners.get(method) ?? []) {
        (handler as (params: unknown) => void)(message.params);
      }
      return;
    }
    if (typeof id !== "number") return;
    const waiter = pending.get(id);
    if (!waiter) return;
    pending.delete(id);
    const error = message.error;
    if (isObject(error)) {
      waiter.reject(new RpcError(Number(error.code), String(error.message), error.data));
    } else {
      waiter.resolve(message.result);
    }
  });

  return {
    call(method, params) {
      const id = nextId++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
        transport.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
      });
    },
    on(name, handler) {
      let set = listeners.get(name);
      if (!set) {
        set = new Set();
        listeners.set(name, set);
      }
      set.add(handler);
      return () => set.delete(handler);
    },
    close() {
      unsubscribe();
      for (const waiter of pending.values()) waiter.reject(new Error("engine client closed"));
      pending.clear();
      listeners.clear();
    },
  };
}
