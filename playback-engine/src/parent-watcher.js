// Parent-loss detection and self-termination (task 12.3, Requirement 10.7).
//
// The Playback_Engine is a disposable child of the Rust TUI parent. If the TUI
// terminates WITHOUT running its exit cleanup — e.g. SIGKILL or a crash — it
// never sends the `shutdown` IPC request, so the engine would otherwise linger
// and leak the Chromium/Electron memory budget. This module detects that case
// and tears the engine (and all of its child processes) down within 5 seconds
// (design "Playback_Engine" / "Error Handling").
//
// Two independent signals are combined so a single stuck channel cannot keep the
// engine alive:
//   1. IPC disconnect — the TUI is the sole client of the Unix domain socket.
//      When that socket closes and does not reconnect, the parent is gone.
//   2. Parent-PID liveness — the parent PID is passed at launch (env
//      `AMT_PARENT_PID`, falling back to `process.ppid`). When that PID no
//      longer names a live process, the parent is gone. This catches the case
//      where the socket is half-open and never emits `close`.
//
// The decision is factored into a PURE function (`shouldTerminate`) and the
// polling/debounce lives in a small `ParentWatcher` class that takes injected
// clock/liveness/terminate functions, so both can be unit-tested with
// `node --test` without spawning Electron.

'use strict';

// Environment variable the parent TUI sets to its own PID when it spawns the
// engine. Keeping it explicit (rather than relying only on process.ppid) means
// detection still works if the engine is ever re-parented (e.g. to init after
// the TUI dies) — a re-parented process gets ppid 1, which we treat as "lost".
const PARENT_PID_ENV = 'AMT_PARENT_PID';

// How long after the parent disappears we must have terminated (Requirement
// 10.7). We poll well inside this budget and terminate on the first confirmation
// so the full teardown completes comfortably under 5 seconds.
const TERMINATION_DEADLINE_MS = 5000;

// Poll cadence for parent-PID liveness. 1s gives up to ~1s detection latency
// for the PID path, leaving ample margin under the 5s deadline for teardown.
const DEFAULT_POLL_INTERVAL_MS = 1000;

/**
 * Pure decision: given the current signals, should the engine terminate?
 *
 * The parent is considered lost when EITHER the IPC channel has disconnected
 * (and not reconnected) OR the parent PID is no longer alive. Either condition
 * alone is sufficient — both point at the same "parent gone" conclusion, and
 * combining them with OR means a single stuck signal cannot wedge the engine
 * open.
 *
 * `null`/`undefined` liveness means "unknown / not yet checked" and must NOT by
 * itself trigger termination (we only act on a definite "not alive").
 *
 * @param {{ ipcConnected: boolean, parentAlive: (boolean|null|undefined) }} signals
 * @returns {boolean} true when the engine should self-terminate
 */
function shouldTerminate(signals) {
  const ipcConnected = Boolean(signals && signals.ipcConnected);
  const parentAlive = signals ? signals.parentAlive : undefined;

  const ipcLost = !ipcConnected;
  const pidLost = parentAlive === false;
  return ipcLost || pidLost;
}

/**
 * Resolve the parent PID to watch: the explicit `AMT_PARENT_PID` env value when
 * present and valid, otherwise `process.ppid`.
 *
 * @param {NodeJS.ProcessEnv} [env] defaults to process.env
 * @param {number} [ppid] defaults to process.ppid
 * @returns {number|null} a positive integer PID, or null when none is resolvable
 */
function resolveParentPid(env, ppid) {
  const source = env || process.env;
  const raw = source[PARENT_PID_ENV];
  if (raw !== undefined && raw !== null && String(raw).trim() !== '') {
    const n = Number(raw);
    if (Number.isInteger(n) && n > 0) {
      return n;
    }
  }
  const fallback = ppid === undefined ? process.ppid : ppid;
  if (Number.isInteger(fallback) && fallback > 0) {
    return fallback;
  }
  return null;
}

/**
 * Check whether a PID currently names a live process.
 *
 * Uses the POSIX "signal 0" probe: `process.kill(pid, 0)` throws ESRCH when no
 * such process exists and EPERM when it exists but we lack permission to signal
 * it (still alive). PID 1 (init) is treated as "parent lost" because a child
 * re-parented to init has, by definition, lost its original parent.
 *
 * @param {number|null} pid
 * @returns {boolean} true when the PID is a live, non-init process
 */
function isPidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 1) {
    // No PID, or re-parented to init: the original parent is gone.
    return false;
  }
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM: the process exists but we cannot signal it — still alive.
    if (err && err.code === 'EPERM') {
      return true;
    }
    // ESRCH (or anything else): no such process.
    return false;
  }
}

/**
 * Watches for parent loss and invokes a terminate callback exactly once when the
 * parent is confirmed gone.
 *
 * Signals are tracked independently:
 *   - `setIpcConnected(bool)` is driven by the IPC server (connection / close).
 *   - a periodic timer polls parent-PID liveness.
 *
 * The clock, PID-liveness probe, and terminate action are all injectable so the
 * watcher can be unit-tested deterministically without real timers, real
 * processes, or Electron.
 */
class ParentWatcher {
  /**
   * @param {{
   *   terminate: () => void,
   *   isAlive?: (pid: number|null) => boolean,
   *   parentPid?: number|null,
   *   pollIntervalMs?: number,
   *   setInterval?: (fn: () => void, ms: number) => any,
   *   clearInterval?: (handle: any) => void,
   * }} options
   */
  constructor(options) {
    const opts = options || {};
    if (typeof opts.terminate !== 'function') {
      throw new TypeError('ParentWatcher requires a terminate() callback');
    }
    this._terminate = opts.terminate;
    this._isAlive = typeof opts.isAlive === 'function' ? opts.isAlive : isPidAlive;
    this._parentPid =
      opts.parentPid !== undefined ? opts.parentPid : resolveParentPid();
    this._pollIntervalMs = opts.pollIntervalMs || DEFAULT_POLL_INTERVAL_MS;
    this._setInterval = opts.setInterval || setInterval;
    this._clearInterval = opts.clearInterval || clearInterval;

    // The engine begins life connected to its spawning parent: the TUI connects
    // over the socket right after launch. Starting `true` avoids a spurious
    // "disconnected" termination in the window before the first connection.
    this._ipcConnected = true;
    this._timer = null;
    this._terminated = false;
  }

  /** @returns {number} the self-termination deadline in ms (Requirement 10.7). */
  static get deadlineMs() {
    return TERMINATION_DEADLINE_MS;
  }

  /**
   * Begin polling parent-PID liveness. Also runs one immediate evaluation so a
   * parent that is already gone at start is caught without waiting a full tick.
   */
  start() {
    if (this._timer !== null) {
      return;
    }
    this._timer = this._setInterval(() => this._evaluate(), this._pollIntervalMs);
    // Do not keep the event loop alive solely for this timer.
    if (this._timer && typeof this._timer.unref === 'function') {
      this._timer.unref();
    }
    this._evaluate();
  }

  /** Stop polling. Safe to call multiple times. */
  stop() {
    if (this._timer !== null) {
      this._clearInterval(this._timer);
      this._timer = null;
    }
  }

  /**
   * Update the IPC-connection signal. Called by the IPC server when the client
   * connects (true) or the socket closes (false). A disconnect is re-evaluated
   * immediately rather than waiting for the next poll tick, to minimize latency.
   *
   * @param {boolean} connected
   */
  setIpcConnected(connected) {
    this._ipcConnected = Boolean(connected);
    this._evaluate();
  }

  /**
   * Evaluate the current signals and terminate if the parent is lost. The
   * terminate callback is invoked at most once.
   */
  _evaluate() {
    if (this._terminated) {
      return;
    }
    const parentAlive =
      this._parentPid === null ? false : this._isAlive(this._parentPid);
    if (shouldTerminate({ ipcConnected: this._ipcConnected, parentAlive })) {
      this._terminated = true;
      this.stop();
      this._terminate();
    }
  }
}

module.exports = {
  ParentWatcher,
  shouldTerminate,
  resolveParentPid,
  isPidAlive,
  PARENT_PID_ENV,
  TERMINATION_DEADLINE_MS,
  DEFAULT_POLL_INTERVAL_MS,
};
