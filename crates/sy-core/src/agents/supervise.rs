//! The supervisor's pure parts (spec 0004): run limits and their clocks.
//!
//! Limits come from the first source with a valid value (AC-2): the
//! `dev.agent_run` override, the project's settings, the global settings,
//! then the default. They are fixed for the run's life.
//!
//! The clocks are monotonic (`std::time::Instant`, which on macOS does not
//! advance while the Mac sleeps), and one tick adds at most [`MAX_TICK`], so
//! neither sleep, a clock change nor a stalled driver can trip a limit
//! (AC-3, AC-4).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::ports::{ProcInfo, ProcState};
use crate::model::{JsonValue, LimitKind};

/// The idle (hang) limit's setting key.
pub const KEY_IDLE_MS: &str = "limits.run_idle_ms";
/// The active time limit's setting key.
pub const KEY_ACTIVE_MS: &str = "limits.run_active_ms";
/// The output limit's setting key.
pub const KEY_OUTPUT_BYTES: &str = "limits.run_output_bytes";

/// The idle limit's default: 30 minutes.
pub const DEFAULT_IDLE_MS: u64 = 30 * 60 * 1000;
/// The active time limit's default: 4 hours.
pub const DEFAULT_ACTIVE_MS: u64 = 4 * 60 * 60 * 1000;
/// The output limit's default: 512 MiB.
pub const DEFAULT_OUTPUT_BYTES: u64 = 512 * 1024 * 1024;

/// The largest valid limit value, 2^53 (a JSON integer every client reads exactly).
pub const MAX_LIMIT: u64 = 1 << 53;

/// The most one tick adds to any clock.
pub const MAX_TICK: Duration = Duration::from_secs(2);

/// The three setting keys, in the order output, time, idle.
pub const LIMIT_KEYS: [&str; 3] = [KEY_OUTPUT_BYTES, KEY_ACTIVE_MS, KEY_IDLE_MS];

/// A run's resolved limits. `None` means that limit is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The idle limit, ms.
    pub idle_ms: Option<u64>,
    /// The active time limit, ms.
    pub active_ms: Option<u64>,
    /// The output limit, bytes.
    pub output_bytes: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            idle_ms: Some(DEFAULT_IDLE_MS),
            active_ms: Some(DEFAULT_ACTIVE_MS),
            output_bytes: Some(DEFAULT_OUTPUT_BYTES),
        }
    }
}

/// The highest limit source, `dev.agent_run.limits` (devtools only). Each
/// value was already checked to be an integer from 0 to [`MAX_LIMIT`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LimitOverride {
    /// The idle limit, ms (0 = off).
    pub idle_ms: Option<u64>,
    /// The active time limit, ms (0 = off).
    pub active_ms: Option<u64>,
    /// The output limit, bytes (0 = off).
    pub output_bytes: Option<u64>,
}

/// Where a limit setting was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingScope {
    /// `project_setting`.
    Project,
    /// The global `setting`.
    Global,
}

impl SettingScope {
    /// The scope as log text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// A limit setting that was skipped because its value is not valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadSetting {
    /// The setting key.
    pub key: &'static str,
    /// Where it was read from.
    pub scope: SettingScope,
    /// The value, as debug text.
    pub value: String,
}

/// A valid limit value: a JSON integer from 0 to [`MAX_LIMIT`].
pub fn valid_limit(value: &JsonValue) -> Option<u64> {
    value.as_u64().filter(|v| *v <= MAX_LIMIT)
}

/// Resolve a run's limits (AC-2). `project` is `None` when its read failed,
/// so that source is skipped. Returns the limits and every value skipped as
/// invalid, for the caller to warn about once.
pub fn resolve_limits(
    over: &LimitOverride,
    project: Option<&BTreeMap<String, JsonValue>>,
    global: &BTreeMap<String, JsonValue>,
) -> (Limits, Vec<BadSetting>) {
    let mut bad = Vec::new();
    let mut one = |key: &'static str, over: Option<u64>, default: u64| -> Option<u64> {
        let value = over.or_else(|| {
            let sources = [
                (SettingScope::Project, project.and_then(|p| p.get(key))),
                (SettingScope::Global, global.get(key)),
            ];
            sources.into_iter().find_map(|(scope, value)| {
                let value = value?;
                let valid = valid_limit(value);
                if valid.is_none() {
                    bad.push(BadSetting {
                        key,
                        scope,
                        value: format!("{value:?}"),
                    });
                }
                valid
            })
        });
        match value.unwrap_or(default) {
            0 => None,
            v => Some(v),
        }
    };
    let limits = Limits {
        idle_ms: one(KEY_IDLE_MS, over.idle_ms, DEFAULT_IDLE_MS),
        active_ms: one(KEY_ACTIVE_MS, over.active_ms, DEFAULT_ACTIVE_MS),
        output_bytes: one(KEY_OUTPUT_BYTES, over.output_bytes, DEFAULT_OUTPUT_BYTES),
    };
    (limits, bad)
}

/// A limit a run reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitHit {
    /// Which limit.
    pub kind: LimitKind,
    /// The resolved limit.
    pub threshold: u64,
    /// The clock's value at the tick that hit it.
    pub observed: u64,
}

/// What the PTY reader thread shares with the driver: every byte it read
/// and when it last read any, on the lossless reader path (never the
/// droppable decoder lane), so dropped decoder chunks never fool the idle or
/// output clocks.
#[derive(Debug)]
pub struct OutputMeter {
    base: Instant,
    bytes: AtomicU64,
    /// Nanoseconds after `base` of the last read, or `u64::MAX` for none.
    last_ns: AtomicU64,
}

impl OutputMeter {
    /// A meter with nothing read, its times counted from `base`.
    pub const fn new(base: Instant) -> Self {
        Self {
            base,
            bytes: AtomicU64::new(0),
            last_ns: AtomicU64::new(u64::MAX),
        }
    }

    /// The reader read `n` bytes just now.
    pub fn read(&self, n: usize) {
        self.bytes
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        let ns = u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX - 1);
        self.last_ns.store(ns, Ordering::Release);
    }

    /// Every byte read so far.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// When the reader last read anything.
    pub fn last_read(&self) -> Option<Instant> {
        match self.last_ns.load(Ordering::Acquire) {
            u64::MAX => None,
            ns => Some(self.base + Duration::from_nanos(ns)),
        }
    }
}

/// A run's idle and active clocks and its output count (AC-3 to AC-5).
#[derive(Debug, Clone)]
pub struct LimitClocks {
    /// The last tick (or the spawn).
    last_tick: Instant,
    /// The latest activity: output, a hook call, leaving `waiting_input`.
    last_activity: Instant,
    idle: Duration,
    active: Duration,
    output: u64,
}

impl LimitClocks {
    /// Clocks starting at the spawn.
    pub const fn new(spawned: Instant) -> Self {
        Self {
            last_tick: spawned,
            last_activity: spawned,
            idle: Duration::ZERO,
            active: Duration::ZERO,
            output: 0,
        }
    }

    /// Activity at `at`: the idle clock goes back to zero. An activity older
    /// than the latest one changes nothing.
    pub fn activity(&mut self, at: Instant) {
        if at > self.last_activity {
            self.last_activity = at;
            self.idle = Duration::ZERO;
        }
    }

    /// One tick at `now`. `counting` is true while the run is `starting` or
    /// `running`: only then do the clocks advance. `output` is every byte
    /// read so far.
    pub fn tick(&mut self, now: Instant, counting: bool, output: u64) {
        self.output = output;
        let delta = now.saturating_duration_since(self.last_tick).min(MAX_TICK);
        let since_activity = now
            .saturating_duration_since(self.last_activity)
            .min(MAX_TICK);
        self.last_tick = self.last_tick.max(now);
        if counting {
            self.active += delta;
            self.idle += delta.min(since_activity);
        }
    }

    /// The idle clock.
    pub const fn idle(&self) -> Duration {
        self.idle
    }

    /// The active clock.
    pub const fn active(&self) -> Duration {
        self.active
    }

    /// The first limit at or over its value, in the order output, time, idle (AC-6).
    pub fn check(&self, limits: &Limits) -> Option<LimitHit> {
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        [
            (LimitKind::Output, limits.output_bytes, self.output),
            (LimitKind::Time, limits.active_ms, ms(self.active)),
            (LimitKind::Idle, limits.idle_ms, ms(self.idle)),
        ]
        .into_iter()
        .find_map(|(kind, limit, observed)| {
            let threshold = limit?;
            (observed >= threshold).then_some(LimitHit {
                kind,
                threshold,
                observed,
            })
        })
    }
}

// ---- process tree tracking (AC-7) ----

/// The most processes one run tracks, its main process included.
pub const TRACK_CAP: usize = 256;

/// One process, named by its pid and start time.
pub type ProcKey = (i64, i64);

/// A process to record in `run_process`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTracked {
    /// Process id.
    pub pid: i64,
    /// Start time, Unix ms.
    pub started_at: i64,
    /// The short executable name from its first sighting.
    pub name: String,
}

/// The run's descendants in `scan` (AC-7): every non zombie process owned
/// by the engine's real user that (a) is in the run's session (its session
/// id is the main pid) and started no earlier than the main process, or
/// (b) has a parent chain reaching one of `roots` (matched by pid and start
/// time), each child starting no earlier than its parent. `main_started` is
/// `None` when the main's start time could not be read; the held zombie
/// still keeps its session id from being reused. The main itself is never
/// in the result.
pub fn descendants(
    scan: &[ProcInfo],
    main_pid: i64,
    main_started: Option<i64>,
    roots: &[ProcKey],
) -> Vec<ProcInfo> {
    let by_pid: HashMap<i64, &ProcInfo> = scan.iter().map(|p| (p.pid, p)).collect();
    let eligible = |p: &ProcInfo| p.uid_is_self && !p.zombie && p.pid != main_pid && p.pid > 1;
    let is_root = |p: &ProcInfo| roots.contains(&(p.pid, p.started_at_ms));
    let in_session =
        |p: &ProcInfo| p.sid == Some(main_pid) && main_started.is_none_or(|m| p.started_at_ms >= m);
    let chain_reaches_root = |p: &ProcInfo| {
        let mut cur = p;
        // A parent chain is short; the bound only guards a cycle.
        for _ in 0..64 {
            let Some(parent) = cur.ppid.and_then(|pp| by_pid.get(&pp)) else {
                return false;
            };
            if parent.started_at_ms > cur.started_at_ms {
                return false;
            }
            if is_root(parent) {
                return true;
            }
            cur = parent;
        }
        false
    };
    scan.iter()
        .filter(|p| eligible(p) && (in_session(p) || chain_reaches_root(p)))
        .cloned()
        .collect()
}

/// What one scan means for one run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Seen in two scans in a row: insert their rows (`INSERT OR IGNORE`).
    pub insert: Vec<NewTracked>,
    /// Tracked but missing from the scan (or changed): check each directly.
    pub recheck: Vec<ProcKey>,
    /// Descendants not recorded because the run is at [`TRACK_CAP`].
    pub over_cap: usize,
    /// True the first time this run went over the cap (warn once).
    pub warn_cap: bool,
}

/// One run's tracked processes (AC-7), shared by the scanner and the run's
/// monitor behind one mutex.
#[derive(Debug, Clone)]
pub struct Tracker {
    main_pid: i64,
    main_started: Option<i64>,
    /// Seen in the last scan, not tracked yet, with the name of the first sighting.
    seen_once: HashMap<ProcKey, String>,
    /// Tracked: has (or is getting) a `run_process` row.
    tracked: HashMap<ProcKey, String>,
    closed: bool,
    cap_warned: bool,
}

impl Tracker {
    /// A tracker for a run whose main process is `main_pid`. With a start
    /// time, the main process is tracked from the start (its row is
    /// inserted at the spawn, AC-1).
    pub fn new(main_pid: i64, main_started: Option<i64>, main_name: &str) -> Self {
        let mut tracked = HashMap::new();
        if let Some(started) = main_started {
            tracked.insert((main_pid, started), main_name.to_owned());
        }
        Self {
            main_pid,
            main_started,
            seen_once: HashMap::new(),
            tracked,
            closed: false,
            cap_warned: false,
        }
    }

    /// The main process's pid.
    pub const fn main_pid(&self) -> i64 {
        self.main_pid
    }

    /// The main process's start time, when it was read.
    pub const fn main_started(&self) -> Option<i64> {
        self.main_started
    }

    /// The main process, as a tracked key.
    pub fn main_key(&self) -> Option<ProcKey> {
        self.main_started.map(|s| (self.main_pid, s))
    }

    /// Mark the run ended: the scanner never writes for it again (AC-8).
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// True once the run reached its terminal transition.
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Every tracked process with its name.
    pub fn tracked(&self) -> Vec<(ProcKey, String)> {
        self.tracked.iter().map(|(k, n)| (*k, n.clone())).collect()
    }

    /// The parent chain roots: the main process and every tracked process.
    pub fn roots(&self) -> Vec<ProcKey> {
        let mut roots: Vec<ProcKey> = self.tracked.keys().copied().collect();
        if let Some(main) = self.main_key()
            && !roots.contains(&main)
        {
            roots.push(main);
        }
        roots
    }

    /// Apply one successful scan. A closed tracker changes nothing.
    pub fn observe(&mut self, scan: &[ProcInfo]) -> Observed {
        let mut out = Observed::default();
        if self.closed {
            return out;
        }
        let found = descendants(scan, self.main_pid, self.main_started, &self.roots());
        let mut seen_now = HashMap::new();
        for p in &found {
            let key = (p.pid, p.started_at_ms);
            if self.tracked.contains_key(&key) {
                continue;
            }
            let name = self.seen_once.get(&key).cloned();
            match name {
                Some(name) if self.tracked.len() < TRACK_CAP => {
                    self.tracked.insert(key, name.clone());
                    out.insert.push(NewTracked {
                        pid: key.0,
                        started_at: key.1,
                        name,
                    });
                }
                Some(name) => {
                    // Reconsidered on the next scan, once a slot is free.
                    out.over_cap += 1;
                    seen_now.insert(key, name);
                }
                None => {
                    seen_now.insert(key, p.name.clone());
                }
            }
        }
        self.seen_once = seen_now;
        if out.over_cap > 0 && !self.cap_warned {
            self.cap_warned = true;
            out.warn_cap = true;
        }
        let alive: HashSet<ProcKey> = scan
            .iter()
            .filter(|p| !p.zombie)
            .map(|p| (p.pid, p.started_at_ms))
            .collect();
        out.recheck = self
            .tracked
            .keys()
            .filter(|k| !alive.contains(k))
            .copied()
            .collect();
        out
    }

    /// Processes confirmed gone by a direct check: no longer tracked.
    /// Returns the keys whose rows to delete (none once closed: the sweep
    /// owns the rows then).
    pub fn confirm_gone(&mut self, gone: &[ProcKey]) -> Vec<ProcKey> {
        if self.closed {
            return Vec::new();
        }
        gone.iter()
            .filter(|k| self.tracked.remove(k).is_some())
            .copied()
            .collect()
    }
}

/// Whether a direct check says the process `key` is gone: absent, a
/// different start time (a reused pid), or a zombie.
pub fn is_gone(key: ProcKey, state: Option<ProcState>) -> bool {
    state.is_none_or(|s| s.zombie || s.started_at_ms != key.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> JsonValue {
        JsonValue::Int(n)
    }

    fn map(pairs: &[(&str, JsonValue)]) -> BTreeMap<String, JsonValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn the_override_should_beat_project_beat_global_beat_default() {
        let project = map(&[(KEY_IDLE_MS, int(20)), (KEY_ACTIVE_MS, int(21))]);
        let global = map(&[
            (KEY_IDLE_MS, int(30)),
            (KEY_ACTIVE_MS, int(31)),
            (KEY_OUTPUT_BYTES, int(32)),
        ]);
        let over = LimitOverride {
            idle_ms: Some(10),
            ..LimitOverride::default()
        };
        let (limits, bad) = resolve_limits(&over, Some(&project), &global);
        assert_eq!(
            limits,
            Limits {
                idle_ms: Some(10),
                active_ms: Some(21),
                output_bytes: Some(32),
            }
        );
        assert!(bad.is_empty());
        let (limits, _) = resolve_limits(&LimitOverride::default(), None, &BTreeMap::new());
        assert_eq!(limits, Limits::default());
    }

    #[test]
    fn zero_should_turn_a_limit_off() {
        let global = map(&[(KEY_OUTPUT_BYTES, int(0))]);
        let over = LimitOverride {
            idle_ms: Some(0),
            ..LimitOverride::default()
        };
        let (limits, _) = resolve_limits(&over, None, &global);
        assert_eq!(limits.idle_ms, None);
        assert_eq!(limits.output_bytes, None);
        assert_eq!(limits.active_ms, Some(DEFAULT_ACTIVE_MS));
    }

    #[test]
    fn invalid_values_should_be_skipped_and_reported() {
        let project = map(&[
            (KEY_IDLE_MS, int(-5)),
            (KEY_ACTIVE_MS, JsonValue::String("x".into())),
            (KEY_OUTPUT_BYTES, JsonValue::Float(1.5)),
        ]);
        let global = map(&[
            (KEY_IDLE_MS, int(7)),
            (KEY_ACTIVE_MS, JsonValue::UInt(MAX_LIMIT + 1)),
        ]);
        let (limits, bad) = resolve_limits(&LimitOverride::default(), Some(&project), &global);
        assert_eq!(limits.idle_ms, Some(7));
        assert_eq!(limits.active_ms, Some(DEFAULT_ACTIVE_MS));
        assert_eq!(limits.output_bytes, Some(DEFAULT_OUTPUT_BYTES));
        let keys: Vec<_> = bad.iter().map(|b| (b.key, b.scope)).collect();
        assert_eq!(
            keys,
            vec![
                (KEY_IDLE_MS, SettingScope::Project),
                (KEY_ACTIVE_MS, SettingScope::Project),
                (KEY_ACTIVE_MS, SettingScope::Global),
                (KEY_OUTPUT_BYTES, SettingScope::Project),
            ]
        );
        assert_eq!(valid_limit(&JsonValue::UInt(MAX_LIMIT)), Some(MAX_LIMIT));
    }

    #[test]
    fn a_failed_project_read_should_fall_through_to_global() {
        let global = map(&[(KEY_IDLE_MS, int(9))]);
        let (limits, _) = resolve_limits(&LimitOverride::default(), None, &global);
        assert_eq!(limits.idle_ms, Some(9));
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_long_gap_between_ticks_should_add_at_most_two_seconds() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 10 * 60 * 1000), true, 0);
        assert_eq!(clocks.active(), MAX_TICK);
        assert_eq!(clocks.idle(), MAX_TICK);
    }

    #[test]
    fn activity_should_reset_idle_but_not_active() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 1000), true, 0);
        clocks.activity(at(base, 1500));
        clocks.tick(at(base, 2000), true, 0);
        assert_eq!(clocks.active(), Duration::from_millis(2000));
        assert_eq!(clocks.idle(), Duration::from_millis(500));
    }

    #[test]
    fn clocks_should_pause_while_not_counting() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 1000), true, 0);
        clocks.tick(at(base, 2000), false, 0);
        clocks.tick(at(base, 2500), true, 0);
        assert_eq!(clocks.active(), Duration::from_millis(1500));
        assert_eq!(clocks.idle(), Duration::from_millis(1500));
    }

    #[test]
    fn check_should_take_output_then_time_then_idle_at_or_over() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 1000), true, 100);
        let limits = Limits {
            idle_ms: Some(1000),
            active_ms: Some(1000),
            output_bytes: Some(100),
        };
        assert_eq!(
            clocks.check(&limits),
            Some(LimitHit {
                kind: LimitKind::Output,
                threshold: 100,
                observed: 100
            })
        );
        let no_output = Limits {
            output_bytes: None,
            ..limits
        };
        assert_eq!(
            clocks.check(&no_output).map(|h| h.kind),
            Some(LimitKind::Time)
        );
        let idle_only = Limits {
            idle_ms: Some(1000),
            active_ms: Some(1001),
            output_bytes: Some(101),
        };
        assert_eq!(
            clocks.check(&idle_only).map(|h| h.kind),
            Some(LimitKind::Idle)
        );
        let none = Limits {
            idle_ms: None,
            active_ms: None,
            output_bytes: None,
        };
        assert_eq!(clocks.check(&none), None);
    }

    fn proc(pid: i64, ppid: i64, sid: i64, started: i64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: Some(ppid),
            sid: Some(sid),
            started_at_ms: started,
            uid_is_self: true,
            zombie: false,
            name: format!("p{pid}"),
        }
    }

    const MAIN: i64 = 100;

    fn tracker() -> Tracker {
        Tracker::new(MAIN, Some(10_000), "agent")
    }

    #[test]
    fn descendants_should_follow_the_session_then_the_parent_chain() {
        let scan = vec![
            proc(MAIN, 50, MAIN, 10_000),
            proc(101, MAIN, MAIN, 11_000),
            // Reparented to launchd, still in the session.
            proc(102, 1, MAIN, 12_000),
            // A `setsid` escapee under the main.
            proc(103, MAIN, 103, 13_000),
            // Its own child.
            proc(104, 103, 103, 14_000),
            // In the session but started before the main: not ours.
            proc(105, 1, MAIN, 9_000),
            // Claims a parent that started after it: not ours.
            proc(106, 103, 106, 12_500),
            // Someone else's.
            proc(107, 1, 107, 15_000),
            ProcInfo {
                uid_is_self: false,
                ..proc(108, MAIN, MAIN, 15_000)
            },
            ProcInfo {
                zombie: true,
                ..proc(109, MAIN, MAIN, 15_000)
            },
        ];
        let found: Vec<i64> = descendants(&scan, MAIN, Some(10_000), &[(MAIN, 10_000)])
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, vec![101, 102, 103, 104]);
    }

    #[test]
    fn a_process_should_need_two_scans_in_a_row_and_keep_its_first_name() {
        let mut t = tracker();
        let child = proc(101, MAIN, MAIN, 11_000);
        let main = proc(MAIN, 50, MAIN, 10_000);
        assert!(t.observe(&[main.clone(), child.clone()]).insert.is_empty());
        let renamed = ProcInfo {
            name: "later".into(),
            ..child.clone()
        };
        let second = t.observe(&[main.clone(), renamed]);
        assert_eq!(
            second.insert,
            vec![NewTracked {
                pid: 101,
                started_at: 11_000,
                name: "p101".into()
            }]
        );
        // Seen once, missed, seen again: starts over.
        let other = proc(102, MAIN, MAIN, 12_000);
        t.observe(&[main.clone(), other.clone()]);
        t.observe(std::slice::from_ref(&main));
        assert!(t.observe(&[main, other]).insert.is_empty());
    }

    #[test]
    fn a_reparented_escapee_should_stay_tracked_and_its_children_found() {
        let mut t = tracker();
        let main = proc(MAIN, 50, MAIN, 10_000);
        let escapee = proc(103, MAIN, 103, 13_000);
        t.observe(&[main.clone(), escapee.clone()]);
        t.observe(&[main, escapee.clone()]);
        // The main is gone; the escapee is reparented to launchd.
        let moved = ProcInfo {
            ppid: Some(1),
            ..escapee
        };
        let grandchild = proc(104, 103, 103, 14_000);
        let out = t.observe(&[moved.clone(), grandchild.clone()]);
        assert!(!out.recheck.contains(&(103, 13_000)));
        let out = t.observe(&[moved, grandchild]);
        assert_eq!(out.insert.len(), 1);
        assert_eq!(out.insert[0].pid, 104);
    }

    #[test]
    fn the_257th_process_should_wait_for_a_free_slot_with_one_warning() {
        let mut t = tracker();
        let main = proc(MAIN, 50, MAIN, 10_000);
        let mut scan = vec![main];
        scan.extend((0..256).map(|i| proc(1000 + i, MAIN, MAIN, 11_000)));
        t.observe(&scan);
        let out = t.observe(&scan);
        // 255 children fit beside the main process.
        assert_eq!(
            (out.insert.len(), out.over_cap, out.warn_cap),
            (255, 1, true)
        );
        let again = t.observe(&scan);
        assert_eq!((again.over_cap, again.warn_cap), (1, false));
        // One tracked child goes; the waiting one takes its slot.
        let gone = (1000, 11_000);
        scan.retain(|p| (p.pid, p.started_at_ms) != gone);
        let out = t.observe(&scan);
        assert_eq!(out.recheck, vec![gone]);
        assert_eq!(t.confirm_gone(&out.recheck), vec![gone]);
        let out = t.observe(&scan);
        assert_eq!((out.insert.len(), out.over_cap), (1, 0));
    }

    #[test]
    fn rechecks_should_cover_missing_changed_and_zombie_processes() {
        let mut t = tracker();
        let main = proc(MAIN, 50, MAIN, 10_000);
        let a = proc(101, MAIN, MAIN, 11_000);
        let b = proc(102, MAIN, MAIN, 11_000);
        let c = proc(103, MAIN, MAIN, 11_000);
        let all = vec![main.clone(), a.clone(), b.clone(), c.clone()];
        t.observe(&all);
        t.observe(&all);
        let reused = ProcInfo {
            started_at_ms: 20_000,
            ..b
        };
        let zombie = ProcInfo { zombie: true, ..c };
        let mut out = t.observe(&[main, reused, zombie]);
        out.recheck.sort_unstable();
        assert_eq!(
            out.recheck,
            vec![(101, 11_000), (102, 11_000), (103, 11_000)]
        );
        // A direct check that finds one alive keeps it.
        assert!(!is_gone(
            (101, 11_000),
            Some(ProcState {
                started_at_ms: 11_000,
                zombie: false
            })
        ));
        assert!(is_gone(
            (102, 11_000),
            Some(ProcState {
                started_at_ms: 20_000,
                zombie: false
            })
        ));
        assert!(is_gone(
            (103, 11_000),
            Some(ProcState {
                started_at_ms: 11_000,
                zombie: true
            })
        ));
        assert!(is_gone((101, 11_000), None));
    }

    // covers: AC-2
    #[test]
    fn an_override_should_hide_a_bad_lower_value_without_reporting_it() {
        let project = map(&[(KEY_IDLE_MS, JsonValue::String("x".into()))]);
        let global = map(&[(KEY_IDLE_MS, JsonValue::Bool(true))]);
        let over = LimitOverride {
            idle_ms: Some(5),
            ..LimitOverride::default()
        };
        let (limits, bad) = resolve_limits(&over, Some(&project), &global);
        assert_eq!(limits.idle_ms, Some(5));
        // The lower sources are never read, so nothing is warned about.
        assert!(bad.is_empty(), "{bad:?}");
    }

    // covers: AC-2
    #[test]
    fn a_valid_project_value_should_stop_the_search_before_a_bad_global_one() {
        let project = map(&[(KEY_ACTIVE_MS, int(42))]);
        let global = map(&[(KEY_ACTIVE_MS, int(-1))]);
        let (limits, bad) = resolve_limits(&LimitOverride::default(), Some(&project), &global);
        assert_eq!(limits.active_ms, Some(42));
        assert!(bad.is_empty(), "{bad:?}");
    }

    // covers: AC-2
    #[test]
    fn only_json_integers_from_zero_to_two_to_the_53_should_be_valid() {
        assert_eq!(valid_limit(&int(0)), Some(0));
        assert_eq!(valid_limit(&JsonValue::UInt(MAX_LIMIT)), Some(MAX_LIMIT));
        assert_eq!(valid_limit(&JsonValue::UInt(MAX_LIMIT + 1)), None);
        assert_eq!(valid_limit(&int(-1)), None);
        assert_eq!(valid_limit(&JsonValue::Float(2.0)), None);
        assert_eq!(valid_limit(&JsonValue::Bool(true)), None);
        assert_eq!(valid_limit(&JsonValue::Null), None);
        assert_eq!(valid_limit(&JsonValue::String("10".into())), None);
    }

    // covers: AC-3
    #[test]
    fn an_activity_older_than_the_latest_should_not_reset_idle() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.activity(at(base, 1000));
        clocks.tick(at(base, 1800), true, 0);
        // A late report of something that happened before the last activity.
        clocks.activity(at(base, 500));
        clocks.tick(at(base, 2000), true, 0);
        assert_eq!(clocks.idle(), Duration::from_millis(1000));
    }

    // covers: AC-3, AC-4
    #[test]
    fn a_tick_from_the_past_should_add_nothing() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 1000), true, 0);
        clocks.tick(at(base, 400), true, 0);
        assert_eq!(clocks.active(), Duration::from_millis(1000));
        assert_eq!(clocks.idle(), Duration::from_millis(1000));
    }

    // covers: AC-4, AC-6
    #[test]
    fn a_clock_just_under_its_limit_should_not_hit() {
        let base = Instant::now();
        let mut clocks = LimitClocks::new(base);
        clocks.tick(at(base, 999), true, 99);
        let limits = Limits {
            idle_ms: Some(1000),
            active_ms: Some(1000),
            output_bytes: Some(100),
        };
        assert_eq!(clocks.check(&limits), None);
    }

    // covers: AC-3, AC-5
    #[test]
    fn the_output_meter_should_add_up_reads_and_note_the_last_one() {
        let base = Instant::now();
        let meter = OutputMeter::new(base);
        assert_eq!((meter.bytes(), meter.last_read()), (0, None));
        meter.read(10);
        let first = meter.last_read().unwrap();
        meter.read(5);
        assert_eq!(meter.bytes(), 15);
        let last = meter.last_read().unwrap();
        assert!(first >= base && last >= first);
    }

    // covers: AC-7
    #[test]
    fn with_no_main_start_time_the_session_alone_should_find_descendants() {
        let scan = vec![
            // Any start time counts: there is none to compare against.
            proc(101, 1, MAIN, 1),
            proc(102, 1, 102, 5_000),
        ];
        let found: Vec<i64> = descendants(&scan, MAIN, None, &[])
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, vec![101]);
        let t = Tracker::new(MAIN, None, "agent");
        assert_eq!((t.main_key(), t.roots()), (None, Vec::new()));
        assert!(t.tracked().is_empty());
    }

    // covers: AC-7, AC-9
    #[test]
    fn pid_1_and_the_main_itself_should_never_be_descendants() {
        let scan = vec![proc(MAIN, MAIN, MAIN, 10_000), proc(1, 0, MAIN, 10_000)];
        assert!(descendants(&scan, MAIN, Some(10_000), &[(MAIN, 10_000)]).is_empty());
    }

    // covers: AC-7
    #[test]
    fn a_parent_cycle_should_end_the_walk_without_a_match() {
        // 201 and 202 name each other as parents, same start time.
        let scan = vec![proc(201, 202, 201, 12_000), proc(202, 201, 202, 12_000)];
        assert!(descendants(&scan, MAIN, Some(10_000), &[(MAIN, 10_000)]).is_empty());
    }

    // covers: AC-7
    #[test]
    fn a_tracked_root_should_find_children_outside_the_session() {
        let child = proc(301, 300, 300, 12_000);
        let found = descendants(
            std::slice::from_ref(&child),
            MAIN,
            Some(10_000),
            &[(300, 11_000)],
        );
        // The parent 300 is not in this scan, so the chain can't reach the root.
        assert!(found.is_empty());
        let parent = proc(300, MAIN, 300, 11_000);
        let found: Vec<i64> = descendants(&[parent, child], MAIN, Some(10_000), &[(300, 11_000)])
            .iter()
            .map(|p| p.pid)
            .collect();
        // The root itself is tracked already, so only its child is new.
        assert_eq!(found, vec![301]);
    }

    #[test]
    fn a_closed_tracker_should_never_yield_writes() {
        let mut t = tracker();
        let main = proc(MAIN, 50, MAIN, 10_000);
        let a = proc(101, MAIN, MAIN, 11_000);
        t.observe(&[main.clone(), a.clone()]);
        t.close();
        assert_eq!(t.observe(&[main, a]), Observed::default());
        assert!(t.confirm_gone(&[(MAIN, 10_000)]).is_empty());
    }
}
