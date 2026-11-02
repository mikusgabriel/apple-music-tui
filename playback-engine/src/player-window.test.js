// Tests for the hidden MusicKit player window's main-process state machine
// (task 12.1).
//
// Run with: npm test  (Node's built-in test runner, no extra deps).
//
// These tests exercise the authoritative playback mirror, the renderer-event ->
// IPC-notification mapping (stateChanged / trackFinished / playbackError /
// trackUnavailable), the >= 1 Hz position-report timer, value clamping, and the
// 10-second load/DRM timeout (Requirement 4.13). The real MusicKit JS, the
// Electron BrowserWindow, and the wall clock are replaced with injected fakes:
// a recording transport (send/onEvent) and controllable timer primitives. A
// real Chromium is never launched.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');

const {
  PlayerWindow,
  clampVolume,
  clampPosition,
  POSITION_REPORT_INTERVAL_MS,
  LOAD_TIMEOUT_MS,
} = require('./player-window');

// ---------------------------------------------------------------------------
// Pure clamping helpers
// ---------------------------------------------------------------------------

test('clampVolume clamps to 0-100 and coerces junk to 0', () => {
  assert.equal(clampVolume(50), 50);
  assert.equal(clampVolume(150), 100);
  assert.equal(clampVolume(-10), 0);
  assert.equal(clampVolume('abc'), 0);
  assert.equal(clampVolume(33.7), 34);
});

test('clampPosition floors negatives/junk to 0 and bounds by duration', () => {
  assert.equal(clampPosition(500, null), 500);
  assert.equal(clampPosition(-1, null), 0);
  assert.equal(clampPosition('x', null), 0);
  assert.equal(clampPosition(9000, 5000), 5000); // above duration -> clamped
  assert.equal(clampPosition(3000, 5000), 3000);
});

// ---------------------------------------------------------------------------
// Test harness: fake transport + controllable timers
// ---------------------------------------------------------------------------

/**
 * A fake renderer transport that records outgoing commands and lets a test
 * drive incoming events, plus controllable interval/timeout primitives.
 */
function makeHarness() {
  const commands = [];
  const notifications = [];

  let eventHandler = null;
  const fakeWindow = {
    send: (command) => commands.push(command),
    onEvent: (cb) => {
      eventHandler = cb;
    },
    destroy: () => commands.push({ type: '__destroyed__' }),
  };

  // Controllable timers. Intervals and timeouts are tracked by handle so tests
  // can fire them deterministically.
  let nextHandle = 1;
  const intervals = new Map();
  const timeouts = new Map();

  const deps = {
    notify: (method, params) => notifications.push({ method, params }),
    createWindow: () => fakeWindow,
    setInterval: (fn, ms) => {
      const h = nextHandle++;
      intervals.set(h, { fn, ms });
      return h;
    },
    clearInterval: (h) => intervals.delete(h),
    setTimeout: (fn, ms) => {
      const h = nextHandle++;
      timeouts.set(h, { fn, ms });
      return h;
    },
    clearTimeout: (h) => timeouts.delete(h),
  };

  const player = new PlayerWindow(deps);

  return {
    player,
    commands,
    notifications,
    // Deliver a renderer event into the state machine.
    emitEvent: (event) => {
      assert.ok(eventHandler, 'player.create() must run before events are delivered');
      eventHandler(event);
    },
    // Fire every registered interval tick once.
    tickIntervals: () => {
      for (const { fn } of intervals.values()) fn();
    },
    // Fire every registered timeout and clear it (one-shot).
    fireTimeouts: () => {
      const entries = [...timeouts.entries()];
      timeouts.clear();
      for (const [, { fn }] of entries) fn();
    },
    intervals,
    timeouts,
  };
}

// ---------------------------------------------------------------------------
// Load / authorize / Playback-Start Sequence
// ---------------------------------------------------------------------------

test('load enters Loading, sends setQueue+token to renderer, and arms a 10s timeout', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 2000, 'mut-123', 'dev-abc');

  // The first command is the load with position, Music_User_Token, and
  // developer token, so the renderer can authorize and request the DRM license.
  assert.deepEqual(h.commands[0], {
    type: 'load',
    trackId: 't1',
    positionMs: 2000,
    musicUserToken: 'mut-123',
    developerToken: 'dev-abc',
  });

  // A Loading stateChanged is reported immediately.
  assert.equal(h.notifications[0].method, 'stateChanged');
  assert.equal(h.notifications[0].params.status, 'Loading');
  assert.equal(h.notifications[0].params.trackId, 't1');
  assert.equal(h.notifications[0].params.positionMs, 2000);

  // The 10-second load/DRM timeout is armed (Requirement 4.13).
  assert.equal(h.timeouts.size, 1);
  assert.equal([...h.timeouts.values()][0].ms, LOAD_TIMEOUT_MS);
});

test('a playing event clears the load timeout and reports Playing, starting the position timer', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();

  assert.deepEqual(h.commands[1], { type: 'play' });

  h.emitEvent({ type: 'ready', trackId: 't1', durationMs: 180000 });
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 0, durationMs: 180000 });

  // The load timeout is cancelled once playback starts.
  assert.equal(h.timeouts.size, 0);

  const playing = h.notifications.find((n) => n.params.status === 'Playing');
  assert.ok(playing, 'a Playing stateChanged must be emitted');
  assert.equal(playing.params.durationMs, 180000);

  // The periodic position-report timer is running at >= 1 Hz.
  assert.equal(h.intervals.size, 1);
  assert.ok([...h.intervals.values()][0].ms <= 1000, 'position timer must tick at least once per second');
  assert.equal([...h.intervals.values()][0].ms, POSITION_REPORT_INTERVAL_MS);
});

test('position timer emits stateChanged at least once per second while playing', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 0, durationMs: 180000 });

  const before = h.notifications.length;
  // Renderer reports progress, then the periodic timer fires.
  h.emitEvent({ type: 'timeupdate', trackId: 't1', positionMs: 1000, durationMs: 180000 });
  h.tickIntervals();

  const after = h.notifications.slice(before);
  const tick = after.find((n) => n.method === 'stateChanged' && n.params.positionMs === 1000);
  assert.ok(tick, 'the position timer should report the updated position');
  assert.equal(tick.params.status, 'Playing');
});

// ---------------------------------------------------------------------------
// Transport commands
// ---------------------------------------------------------------------------

test('pause stops the position timer and reports Paused keeping the position', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 5000, durationMs: 180000 });
  assert.equal(h.intervals.size, 1);

  h.player.pause();
  h.emitEvent({ type: 'paused', trackId: 't1', positionMs: 5000, durationMs: 180000 });

  const paused = h.notifications.find((n) => n.params.status === 'Paused');
  assert.ok(paused);
  assert.equal(paused.params.positionMs, 5000); // position unchanged (R4.2)
  assert.equal(h.intervals.size, 0); // timer stopped while paused
});

test('seek clamps to [0, duration] and echoes the applied position', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.emitEvent({ type: 'ready', trackId: 't1', durationMs: 10000 });

  assert.equal(h.player.seek(-500), 0); // below start
  assert.equal(h.player.seek(99999), 10000); // beyond duration
  assert.equal(h.player.seek(4000), 4000);

  const seekCmd = h.commands.filter((c) => c.type === 'seek').pop();
  assert.deepEqual(seekCmd, { type: 'seek', positionMs: 4000 });
});

test('setVolume clamps 0-100 and echoes the applied level', () => {
  const h = makeHarness();
  h.player.create();
  assert.equal(h.player.setVolume(250), 100);
  assert.equal(h.player.setVolume(-5), 0);
  assert.equal(h.player.setVolume(60), 60);
  const volCmd = h.commands.filter((c) => c.type === 'setVolume').pop();
  assert.deepEqual(volCmd, { type: 'setVolume', level: 60 });
});

// ---------------------------------------------------------------------------
// Event -> notification mapping
// ---------------------------------------------------------------------------

test('ended emits trackFinished and stops the position timer', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 1000, durationMs: 2000 });
  assert.equal(h.intervals.size, 1);

  h.emitEvent({ type: 'ended', trackId: 't1' });

  const finished = h.notifications.find((n) => n.method === 'trackFinished');
  assert.ok(finished);
  assert.deepEqual(finished.params, { trackId: 't1' });
  assert.equal(h.intervals.size, 0);
});

test('a network error pauses and emits playbackError{network}', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 1000, durationMs: 2000 });

  h.emitEvent({ type: 'error', trackId: 't1', kind: 'network' });

  const err = h.notifications.find((n) => n.method === 'playbackError');
  assert.deepEqual(err.params, { trackId: 't1', kind: 'network' });
  assert.equal(h.intervals.size, 0); // paused -> timer stopped
});

test('a drm error emits playbackError{drm}', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.emitEvent({ type: 'error', trackId: 't1', kind: 'drm' });

  const err = h.notifications.find((n) => n.method === 'playbackError');
  assert.deepEqual(err.params, { trackId: 't1', kind: 'drm' });
});

test('an unavailable event emits trackUnavailable (R4.11/4.12)', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.emitEvent({ type: 'unavailable', trackId: 't1' });

  const unavailable = h.notifications.find((n) => n.method === 'trackUnavailable');
  assert.deepEqual(unavailable.params, { trackId: 't1' });
});

// ---------------------------------------------------------------------------
// Load timeout (Requirement 4.13)
// ---------------------------------------------------------------------------

test('a load that never becomes ready within 10s reports playbackError{network}', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');

  // No ready/playing event arrives; the 10s timeout fires.
  h.fireTimeouts();

  const err = h.notifications.find((n) => n.method === 'playbackError');
  assert.deepEqual(err.params, { trackId: 't1', kind: 'network' });
});

test('the load timeout does not fire once playback has started', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 0, durationMs: 1000 });

  // Timeout was cleared, so firing any remaining timers must not raise an error.
  h.fireTimeouts();
  assert.equal(h.notifications.some((n) => n.method === 'playbackError'), false);
});

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

test('destroy stops timers, tears down the renderer, and releases the window', () => {
  const h = makeHarness();
  h.player.create();
  h.player.load('t1', 0, 'mut');
  h.player.play();
  h.emitEvent({ type: 'playing', trackId: 't1', positionMs: 0, durationMs: 1000 });
  assert.equal(h.intervals.size, 1);

  h.player.destroy();

  assert.equal(h.intervals.size, 0);
  assert.equal(h.timeouts.size, 0);
  assert.ok(h.commands.some((c) => c.type === 'teardown'));
  assert.ok(h.commands.some((c) => c.type === '__destroyed__'));
  assert.equal(h.player.window, null);
});
