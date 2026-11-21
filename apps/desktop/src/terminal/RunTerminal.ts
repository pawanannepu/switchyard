import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { LIMITS, decodeBase64, encodeBase64, type EngineClient, type RunOutput } from "@switchyard/protocol";

/** More than this many resyncs inside the window pauses the stream instead of looping. */
const RESYNC_LIMIT = 3;
const RESYNC_WINDOW_MS = 10_000;

export type StreamState = "attaching" | "live" | "paused";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/**
 * One run's terminal. Bytes go straight from the transport into xterm.js,
 * never through React state.
 *
 * Offsets: `run.attach` returns the replay and `end_offset`; every live chunk
 * carries its starting offset, and anything below the cursor is dropped, so
 * there is never a gap or a duplicate.
 */
export class RunTerminal {
  readonly term: Terminal;
  private readonly fit = new FitAddon();
  private cursor = 0;
  private state: StreamState = "attaching";
  private early: RunOutput[] = [];
  private resyncs: number[] = [];
  private readonly probes = new Map<string, number>();
  private disposed = false;
  bytesDrawn = 0;

  onStateChange?: (state: StreamState) => void;
  onProbe?: (latencyMs: number) => void;

  constructor(
    private readonly client: EngineClient,
    readonly runId: string,
    host: HTMLElement,
  ) {
    this.term = new Terminal({
      fontSize: 11,
      scrollback: 2000,
      cursorBlink: false,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
      screenReaderMode: false,
    });
    this.term.loadAddon(this.fit);
    this.term.open(host);
    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      this.term.loadAddon(webgl);
    } catch {
      // The DOM renderer is the fallback.
    }
    this.fit.fit();
    this.term.onData((data) => this.sendInput(data));
    this.term.onResize(({ cols, rows }) => {
      void this.client.call("run.resize", { run_id: this.runId, cols, rows }).catch(() => {});
    });
  }

  private setState(state: StreamState): void {
    this.state = state;
    this.onStateChange?.(state);
  }

  async attach(): Promise<void> {
    this.setState("attaching");
    this.early = [];
    const { replay_b64, end_offset } = await this.client.call("run.attach", { run_id: this.runId });
    if (this.disposed) return;
    this.term.reset();
    this.term.write(decodeBase64(replay_b64));
    this.cursor = end_offset;
    this.setState("live");
    for (const chunk of this.early.splice(0)) this.handleOutput(chunk);
  }

  handleOutput(chunk: RunOutput): void {
    if (this.state === "attaching") {
      this.early.push(chunk);
      return;
    }
    if (this.state === "paused") return;
    const bytes = decodeBase64(chunk.data_b64);
    const end = chunk.offset + bytes.length;
    if (end <= this.cursor) return;
    if (chunk.offset > this.cursor) {
      // A gap should be impossible; recover by attaching again.
      void this.attach();
      return;
    }
    const fresh = chunk.offset < this.cursor ? bytes.subarray(this.cursor - chunk.offset) : bytes;
    this.cursor = end;
    this.bytesDrawn += fresh.length;

    const hits = this.matchProbes(fresh);
    if (hits.length === 0) {
      this.term.write(fresh);
      return;
    }
    this.term.write(fresh, () => {
      const drawn = performance.now();
      for (const sent of hits) this.onProbe?.(drawn - sent);
    });
  }

  handleResync(): void {
    const now = performance.now();
    this.resyncs = this.resyncs.filter((t) => now - t < RESYNC_WINDOW_MS);
    this.resyncs.push(now);
    if (this.resyncs.length > RESYNC_LIMIT) {
      this.setState("paused");
      return;
    }
    void this.attach();
  }

  resume(): void {
    this.resyncs = [];
    void this.attach();
  }

  sendInput(data: string): void {
    const bytes = encoder.encode(data);
    for (let i = 0; i < bytes.length; i += LIMITS.max_input_bytes) {
      const part = bytes.subarray(i, i + LIMITS.max_input_bytes);
      void this.client.call("run.input", { run_id: this.runId, data_b64: encodeBase64(part) }).catch(() => {});
    }
  }

  /** Send a unique marker as input; with an echo run, `onProbe` reports when it is drawn. */
  probe(sequence: number): void {
    const token = `[[p${sequence}]]`;
    this.probes.set(token, performance.now());
    this.sendInput(`${token}\r\n`);
  }

  private matchProbes(bytes: Uint8Array): number[] {
    if (this.probes.size === 0) return [];
    const text = decoder.decode(bytes);
    const hits: number[] = [];
    for (const [token, sent] of this.probes) {
      if (text.includes(token)) {
        hits.push(sent);
        this.probes.delete(token);
      }
    }
    return hits;
  }

  dispose(): void {
    this.disposed = true;
    this.term.dispose();
  }
}
