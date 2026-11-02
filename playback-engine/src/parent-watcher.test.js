// Tests for parent-loss detection and self-termination (task 12.3,
// Requirement 10.7).
//
// Run with: npm test  (Node's built-in test runner, no extra deps).
//
// These tests exercise the pure "should terminate" decision, parent-PID
// resolution and liveness probing, and the ParentWatcher debounce/one-shot
// termination with injected clock/liveness/terminate functions — all without
// spawning Electron. A single end-to-end case uses a real short-lived child
// process to confirm the PID-liveness probe behaves against the real OS.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');

const {
  ParentWatcher,
  shouldTerminate,
  resolveParentPid,
  isPidAlive,
  PARENT_PID_ENV,
  TERMINATION_DEADLINE_MS,
} = require('./parent-watcher');

// ---------------------------------------------------------------------------
// Pure decision: shouldTerminate
// ---------------------------------------------------------------------------

test('shouldTerminate stays false while connected and parent alive', () => {
  assert.equal(shouldTerminate({ ipcConnected: true, parentAlive: true }), false);
});

test('shouldTerminate is true when the IPC channel is disconnected', () => {
  assert.equal(shouldTerminate({ ipcConnected: false, parentAlive: true }), true);
});

test('shouldTerminate is true when the parent PID is not alive', () => {
  assert.equal(shouldTerminate({ ipcConnected: true, parentAlive: false }), true);
});

test('shouldTerminate is true when both signals indicate loss', () => {
  assert.equal(shouldTerminate({ ipcConnected: false, parentAlive: false }), true);
});

test('shouldTerminate does not fire on unknown (null/undefined) liveness alone', () => {
  // Liveness not yet checked must not by itself trigger termination while the
  // IPC channel is still connected.
  assert.equal(shouldTerminate({ ipcConnected: true, parentAlive: null }), false);
  assert.equal(shouldTerminate({ ipcConnected: true, parentAlive: undefined }), false);
});

test('shouldTerminate tolerates missing/garbage input', () => {
  assert.equal(shouldTerminate(undefined), true); // no connection known => lost
  assert.equal(shouldTerminate({}), true); // ipcConnected falsy => lost
});

// ---------------------------------------------------------------------------
// Parent-PID resolution
// ---------------------------------------------------------------------------

test('resolveParentPid prefers a valid AMT_PARENT_PID env value', () => {
  assert.equal(resolveParentPid({ [PARENT_PID_ENV]: '4321' }, 99), 4321);
});

test('resolveParentPid falls back to ppid when env is absent or invalid', () => {
  assert.equal(resolveParentPid({}, 777), 777);
  assert.equal(resolveParentPid({ [PARENT_PID_ENV]: '' }, 777), 777);
  assert.equal(resolveParentPid({ [PARENT_PID_ENV]: 'abc' }, 777), 777);
  assert.equal(resolveParentPid({ [PARENT_PID_ENV]: '0' }, 777), 777);
  assert.equal(resolveParentPid({ [PARENT_PID_ENV]: '-5' }, 777), 777);
});

test('resolveParentPid returns null when nothing is resolvable', () => {
  assert.equal(resolveParentPid({}, 0), null);
  assert.equal(resolveParentPid({}, -1), null);
});

// ---------------------------------------------------------------------------
// PID liveness probe
// ---------------------------------------------------------------------------

test('isPidAlive reports the current process as alive', () => {
  assert.equal(isPidAlive(process.pid), true);
});

test('isPidAlive treats null, init (pid 1), and non-integers as not alive', () => {
  assert.equal(isPidAlive(null), false);
  assert.equal(isPidAlive(1), false);
  assert.equal(isPidAlive(0), false);
  assert.equal(isPidAlive(-10), false);
  assert.equal(isPidAlive(1.5), false);
});

test('isPidAlive returns false for a reaped child PID', async () => {
  const child = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 50)']);
  const pid = child.pid;
  assert.equal(isPidAlive(pid), true);
  await new Promise((resolve) => child.on('exit', resolve));
  // After exit and reap, the PID no longer names a live process.
  assert.equal(isPidAlive(pid), false);
});

// ---------------------------------------------------------------------------
// ParentWatcher behavior (injected clock + liveness)
// ---------------------------------------------------------------------------

/**
 * Build a fake interval scheduler that records the registered callback so tests
 * can drive ticks deterministically.
 */
function makeFakeTimer() {
  const state = { fn: null, cleared: false };
  return {
    state,
    setInterval: (fn) => {
      state.fn = fn;
      return { unref() {} };
    },
    clearInterval: () => {
      state.cleared = true;
    },
    tick: () => {
      if (state.fn) state.fn();
    },
  };
}

test('ParentWatcher requires a terminate callback', () => {
  assert.throws(() => new ParentWatcher({}), TypeError);
});

test('ParentWatcher does not terminate while connected and parent alive', () => {
  const timer = makeFakeTimer();
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    isAlive: () => true,
    parentPid: 1234,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start(); // runs an immediate evaluation
  timer.tick();
  timer.tick();
  assert.equal(terminated, 0);
});

test('ParentWatcher terminates when the parent PID dies between polls', () => {
  const timer = makeFakeTimer();
  let alive = true;
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    isAlive: () => alive,
    parentPid: 1234,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start();
  assert.equal(terminated, 0);
  alive = false; // parent crashes
  timer.tick();
  assert.equal(terminated, 1);
  assert.equal(timer.state.cleared, true, 'polling stops after termination');
});

test('ParentWatcher terminates immediately on IPC disconnect', () => {
  const timer = makeFakeTimer();
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    isAlive: () => true, // PID still appears alive (half-open socket case)
    parentPid: 1234,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start();
  assert.equal(terminated, 0);
  w.setIpcConnected(false);
  assert.equal(terminated, 1);
});

test('ParentWatcher terminates at most once across signals and ticks', () => {
  const timer = makeFakeTimer();
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    isAlive: () => false,
    parentPid: 1234,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start(); // immediate eval: parent not alive => terminate once
  timer.tick();
  w.setIpcConnected(false);
  timer.tick();
  assert.equal(terminated, 1);
});

test('ParentWatcher terminates when no parent PID is resolvable', () => {
  const timer = makeFakeTimer();
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    parentPid: null,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start();
  assert.equal(terminated, 1);
});

test('ParentWatcher stop() halts polling without terminating', () => {
  const timer = makeFakeTimer();
  let terminated = 0;
  const w = new ParentWatcher({
    terminate: () => { terminated += 1; },
    isAlive: () => true,
    parentPid: 1234,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });
  w.start();
  w.stop();
  assert.equal(timer.state.cleared, true);
  assert.equal(terminated, 0);
});

test('the self-termination deadline is within the 5-second budget', () => {
  assert.equal(TERMINATION_DEADLINE_MS, 5000);
  assert.equal(ParentWatcher.deadlineMs, 5000);
});
