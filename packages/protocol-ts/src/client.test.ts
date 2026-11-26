import { describe, expect, it } from "vitest";
import { createEngineClient, RpcError, type Transport } from "./client.ts";
import { ErrorCode } from "./generated/methods.ts";

/** An in memory transport: records what the client sends, lets a test push frames back. */
function fakeTransport() {
  const sent: unknown[] = [];
  const handlers = new Set<(message: unknown) => void>();
  const transport: Transport = {
    send(frame) {
      sent.push(JSON.parse(frame));
    },
    onMessage(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
  };
  const push = (message: unknown) => {
    for (const handler of handlers) handler(message);
  };
  return { transport, sent, push, listening: () => handlers.size };
}

const hello = { engine_version: "0.1.0", pid: 42, protocol: { major: 1, minor: 0 } };

describe("createEngineClient", () => {
  it("should send a JSON-RPC 2.0 request with a fresh numeric id", () => {
    const { transport, sent } = fakeTransport();
    const client = createEngineClient(transport);
    void client.call("engine.info", {});
    void client.call("run.attach", { run_id: "r1" });
    expect(sent).toEqual([
      { jsonrpc: "2.0", id: 1, method: "engine.info", params: {} },
      { jsonrpc: "2.0", id: 2, method: "run.attach", params: { run_id: "r1" } },
    ]);
  });

  it("should resolve each call with the result that carries its id", async () => {
    const { transport, push } = fakeTransport();
    const client = createEngineClient(transport);
    const first = client.call("engine.info", {});
    const second = client.call("run.stop", { run_id: "run_1" });
    push({ jsonrpc: "2.0", id: 2, result: {} });
    push({ jsonrpc: "2.0", id: 1, result: { ...hello, running_agents: 0 } });
    await expect(second).resolves.toEqual({});
    await expect(first).resolves.toMatchObject({ pid: 42, running_agents: 0 });
  });

  it("should reject with an RpcError carrying the engine's code", async () => {
    const { transport, push } = fakeTransport();
    const client = createEngineClient(transport);
    const call = client.call("run.attach", { run_id: "missing" });
    push({ jsonrpc: "2.0", id: 1, error: { code: ErrorCode.RunNotFound, message: "run not found" } });
    const error: unknown = await call.catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RpcError);
    expect(error).toMatchObject({ code: ErrorCode.RunNotFound, message: "run not found" });
  });

  it("should deliver notifications to listeners until they unsubscribe", () => {
    const { transport, push } = fakeTransport();
    const client = createEngineClient(transport);
    const seen: string[] = [];
    const off = client.on("run.resync_needed", (params) => seen.push(params.run_id));
    push({ jsonrpc: "2.0", method: "run.resync_needed", params: { run_id: "r1" } });
    off();
    push({ jsonrpc: "2.0", method: "run.resync_needed", params: { run_id: "r2" } });
    expect(seen).toEqual(["r1"]);
  });

  it("should ignore frames it cannot match", async () => {
    const { transport, push } = fakeTransport();
    const client = createEngineClient(transport);
    const call = client.call("engine.info", {});
    push("not an object");
    push(null);
    push({ jsonrpc: "2.0", id: 99, result: {} });
    push({ jsonrpc: "2.0", id: "1", result: {} });
    push({ jsonrpc: "2.0", id: 1, result: { ...hello, running_agents: 1 } });
    await expect(call).resolves.toMatchObject({ running_agents: 1 });
  });

  it("should reject in flight calls and stop listening on close", async () => {
    const { transport, listening } = fakeTransport();
    const client = createEngineClient(transport);
    const call = client.call("engine.info", {});
    client.close();
    await expect(call).rejects.toThrow("engine client closed");
    expect(listening()).toBe(0);
  });
});
