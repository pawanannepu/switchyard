import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { HostConnection } from "@switchyard/protocol";
import { useEngineStore } from "../engine/store.ts";
import { strings } from "../strings.ts";
import { Home } from "./Home.tsx";

// The real client picks a transport at import time; the screen only needs these.
const host = vi.hoisted(() => ({ canRestart: true, restart: vi.fn(() => Promise.resolve()) }));
vi.mock("../engine/client.ts", () => ({
  engine: { on: () => () => undefined, call: () => new Promise(() => undefined) },
  get canRestartEngine() {
    return host.canRestart;
  },
  restartEngine: host.restart,
}));

const s = strings.connection;
const hello = { engine_version: "0.1.0", pid: 4242, protocol: { major: 1, minor: 0 } };

function showConnection(connection: HostConnection | null) {
  useEngineStore.setState({ connection, info: null });
}

describe("Home", () => {
  beforeEach(() => {
    host.canRestart = true;
    host.restart.mockClear();
  });
  afterEach(cleanup);

  it("should announce connecting before the host reports anything", () => {
    showConnection(null);
    render(<Home />);
    expect(screen.getByRole("status")).toHaveProperty("textContent", s.connecting);
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("should show the engine's hello facts once connected", () => {
    showConnection({ state: "connected", hello });
    render(<Home />);
    expect(screen.getByRole("status")).toHaveProperty("textContent", s.connected);
    expect(screen.getByTestId("engine-version")).toHaveProperty("textContent", "0.1.0");
    expect(screen.getByTestId("engine-protocol")).toHaveProperty("textContent", "1.0");
    expect(screen.getByTestId("engine-pid")).toHaveProperty("textContent", "4242");
  });

  it("should restart the engine from the keyboard when it stopped", async () => {
    showConnection({ state: "stopped", message: "engine exited", logs_dir: "/tmp/sy-logs" });
    render(<Home />);
    const user = userEvent.setup();
    await user.tab();
    const button = screen.getByRole("button", { name: s.restart });
    expect(document.activeElement).toBe(button);
    await user.keyboard("{Enter}");
    expect(host.restart).toHaveBeenCalledOnce();
    expect(screen.getByText("/tmp/sy-logs")).toBeDefined();
  });

  it("should offer a retry, not a restart, when the engine failed to start", () => {
    showConnection({ state: "failed" });
    render(<Home />);
    expect(screen.getByRole("button", { name: s.retry })).toBeDefined();
  });

  it("should explain how to restart when the page has no host to ask", () => {
    host.canRestart = false;
    showConnection({ state: "stopped" });
    render(<Home />);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByText(s.restartUnavailable)).toBeDefined();
  });
});
