// Every UI string lives here, ready for translation. The real catalog format
// (and its tooling) comes with feature 7; keep new strings in this one file.

export const strings = {
  app: {
    name: "switchyard",
    tagline: "Run and manage many AI coding agents at once.",
  },
  connection: {
    heading: "Engine",
    connecting: "Connecting to the engine…",
    connected: "Connected",
    stopped: "Engine stopped",
    failed: "The engine didn't start",
    incompatible: "The running engine needs a restart",
    version: "Version",
    protocol: "Protocol",
    pid: "Process id",
    running: "Agents running",
    logs: "Logs",
    restart: "Restart the engine",
    retry: "Try again",
    restartUnavailable: "Restart the engine from a terminal: this page is connected through the dev bridge.",
  },
  nav: {
    home: "Home",
    loadCheck: "Load check",
    main: "Main",
  },
  load: {
    heading: "Load check",
    intro:
      "Starts 12 synthetic runs at 50 KiB/s each, renders them all, and measures the time from sending input to the echo being drawn. The target is a p95 under 100 ms.",
    start: "Start 12 runs",
    stop: "Stop runs",
    running: "Measuring…",
    samples: "Samples",
    p50: "p50",
    p95: "p95",
    max: "Max",
    throughput: "Output drawn",
    pass: "Pass: p95 is under 100 ms",
    fail: "Over budget: p95 is 100 ms or more",
    paused: "Stream paused because this view fell behind.",
    resume: "Resume",
    terminal: (runId: string) => `Terminal for ${runId}`,
    unavailable: "This engine was built without devtools, so it has no synthetic runs.",
  },
} as const;
