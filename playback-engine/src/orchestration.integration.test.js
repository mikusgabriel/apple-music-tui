// Integration / smoke tests for playback and sign-in orchestration (task 12.4).
//
// Run with: npm test  (Node's built-in test runner, no extra deps).
//
// Unlike the per-module unit tests (player-window.test.js, sign-in-window.test.js,
// ipc-server.test.js), these exercise the *orchestration* — the IPC server wired
// to a real PlayerWindow and a real SignInWindow, driven over a real Unix domain
// socket exactly as the Rust TUI parent drives it in production. They cover the
// scenarios the design's "Integration and Smoke Tests" section assigns to this
// task:
//
//   - Playback start latency: load -> ready -> playing reaches a Playing state
//     quickly and reports a stateChanged over the socket (Requirement 4.1).
//   - Stream / DRM failure handling: a stream (network) failure and a DRM license
//     failure each surface as a named playbackError notification; a load that
//     never readies within the timeout surfaces as playbackError{network}
//     (Requirement 4.13).
//   - Playback_Engine restart / reconnect at the last position: after the socket
//     drops, a freshly-started engine, on the TUI's restart, loads the current
//     track at the last reported position in paused status (Requirements 4.10,
//     4.14).
//   - Sign_In_Window open / cancel / complete and Music_User_Token capture:
//     routed through the IPC server's openSignIn / closeSignIn and reported as
//     signInResult / signInError notifications over the socket, mocking the
//     Apple-hosted page (Requirements 1.1-1.4, 1.14).
//
// MusicKit JS is mocked by a fake renderer transport injected into PlayerWindow
// (no real Chromium); the Apple-hosted sign-in page is mocked by a fake Electron
// injected into SignInWindow (no real BrowserWindow). The IPC transport itself
// is real.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const net = require('net');
const os = require('os');
const path = require('path');
const fs = require('fs');
const { EventEmitter } = require('events');

const { IpcServer, FrameDecoder, encodeFrame } = require('./ipc-server');
const { PlayerWindow, LOAD_TIMEOUT_MS } = require('./player-window');
const { SignInWindow, CHANNEL_READY, CHANNEL_TOKEN, CHANNEL_CONFIG } = require('./sign-in-window');

// ---------------------------------------------------------------------------
// Fake MusicKit renderer transport
//
// Stands in for the hidden MusicKit JS renderer (player.js). It records the
// transport commands the PlayerWindow sends and lets a test drive MusicKit-style
// events back in, so playback orchestration runs without a real Chromium or a
// real DRM license.
// ---------------------------------------------------------------------------

function makeFakeRenderer() {
  const commands = [];
  let eventHandler = null;
  const win = {
    send: (command) => commands.push(command),
    onEvent: (cb) => {
      eventHandler = cb;
    },
    destroy: () => commands.push({ type: '__destroyed__' }),
  };
  return {
    win,
    commands,
    emit: (event) => {
      if (!eventHandler) throw new Error('renderer event before create()');
      eventHandler(event);
    },
    // Convenience: simulate MusicKit completing the DRM license + queue setup and
    // then starting playback at a position, the happy Playback-Start Sequence.
    startPlayback: (trackId, positionMs, durationMs) => {
      if (!eventHandler) throw new Error('renderer event before create()');
      eventHandler({ type: 'ready', trackId, durationMs });
      eventHandler({ type: 'playing', trackId, positionMs, durationMs });
    },
  };
}

// ---------------------------------------------------------------------------
// Fake Electron for the Sign_In_Window (mocks the Apple-hosted page)
// ---------------------------------------------------------------------------

class FakeWebContents extends EventEmitter {
  constructor() {
    super();
    this.sent = [];
  }
  send(channel, payload) {
    this.sent.push([channel, payload]);
  }
}

class FakeSignInBrowserWindow extends EventEmitter {
  constructor(opts) {
    super();
    FakeSignInBrowserWindow.last = this;
    this.options = opts;
    this.webContents = new FakeWebContents();
    this._destroyed = false;
    this.shown = false;
  }
  loadFile(file) {
    this.loadedFile = file;
    return Promise.resolve();
  }
  show() {
    this.shown = true;
  }
  focus() {}
  isDestroyed() {
    return this._destroyed;
  }
  destroy() {
    this._destroyed = true;
    this.emit('closed');
  }
}

function makeFakeElectron() {
  const emitter = new EventEmitter();
  emitter.setMaxListeners(0);
  const ipcMain = {
    on: (channel, handler) => emitter.on(channel, handler),
    removeListener: (channel, handler) => emitter.removeListener(channel, handler),
    emitFrom: (channel, sender, ...args) => emitter.emit(channel, { sender }, ...args),
  };
  return { electron: { ipcMain, BrowserWindow: FakeSignInBrowserWindow }, ipcMain };
}

// ---------------------------------------------------------------------------
// Harness: a live engine (IPC server + real PlayerWindow + real SignInWindow)
// plus a connected TUI-style client speaking the framed JSON-RPC protocol.
// ---------------------------------------------------------------------------

/**
 * Start a Playback_Engine on a private socket with a connected client.
 *
 * @param {object} [opts]
 * @param {object} [opts.electron] fake Electron for the sign-in window
 * @returns {Promise<object>} the harness
 */
async function startEngine(opts = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amt-orch-'));
  const socketPath = path.join(dir, 'engine.sock');

  const renderer = makeFakeRenderer();
  const player = new PlayerWindow({
    createWindow: () => renderer.win,
    notify: (method, params) => server.notify(method, params),
  });
  player.create();

  const signInWindow = new SignInWindow({
    electron: opts.electron,
    onResult: (payload) => server.notify('signInResult', payload),
    onError: (payload) => server.notify('signInError', payload),
  });

  let shutdownCalled = false;
  const server = new IpcServer({
    playerWindow: player,
    signInWindow,
    socketPath,
    onShutdown: () => {
      shutdownCalled = true;
    },
  });
  server.start();

  // A TUI-style client: frames requests, decodes responses + notifications, and
  // exposes a waitFor(predicate) helper for asynchronous notifications.
  const decoder = new FrameDecoder();
  const inbox = [];
  const waiters = [];
  const client = net.connect(socketPath);

  const drainWaiters = () => {
    for (let i = waiters.length - 1; i >= 0; i -= 1) {
      const w = waiters[i];
      const match = inbox.find((m, idx) => idx >= w.from && w.predicate(m));
      if (match) {
        waiters.splice(i, 1);
        w.resolve(match);
      }
    }
  };

  client.on('data', (chunk) => {
    for (const payload of decoder.push(chunk)) {
      inbox.push(JSON.parse(payload.toString('utf8')));
    }
    drainWaiters();
  });

  await new Promise((resolve, reject) => {
    client.once('connect', resolve);
    client.once('error', reject);
  });

  let nextId = 1;
  const pendingReplies = new Map();
  // Intercept replies (messages carrying an id) for request/response matching.
  const origPush = inbox.push.bind(inbox);
  inbox.push = (msg) => {
    if (msg && msg.id !== undefined && pendingReplies.has(msg.id)) {
      const { resolve } = pendingReplies.get(msg.id);
      pendingReplies.delete(msg.id);
      resolve(msg);
    }
    return origPush(msg);
  };

  const harness = {
    server,
    player,
    signInWindow,
    renderer,
    socketPath,
    inbox,
    get shutdownCalled() {
      return shutdownCalled;
    },
    // Send a JSON-RPC request and resolve with the reply.
    request: (method, params) =>
      new Promise((resolve) => {
        const id = nextId++;
        pendingReplies.set(id, { resolve });
        client.write(encodeFrame({ jsonrpc: '2.0', id, method, params: params || {} }));
      }),
    // Resolve when a notification (or any message) matching the predicate arrives.
    waitFor: (predicate) =>
      new Promise((resolve) => {
        const existing = inbox.find((m) => predicate(m));
        if (existing) {
          resolve(existing);
          return;
        }
        waiters.push({ predicate, from: 0, resolve });
      }),
    // Simulate the TUI losing / dropping the connection (engine sees a socket close).
    dropConnection: () =>
      new Promise((resolve) => {
        client.end(() => resolve());
      }),
    stop: () => {
      try {
        client.destroy();
      } catch (_e) {
        /* ignore */
      }
      // Destroy the player too: a Playing track leaves a live position-report
      // interval that would otherwise keep the event loop alive and hang the
      // test runner's exit.
      try {
        player.destroy();
      } catch (_e) {
        /* ignore */
      }
      server.stop();
      try {
        fs.rmSync(dir, { recursive: true, force: true });
      } catch (_e) {
        /* ignore */
      }
    },
  };

  return harness;
}

// ---------------------------------------------------------------------------
// Playback start latency (Requirement 4.1)
// ---------------------------------------------------------------------------

test('smoke: a load+play reaches Playing and reports stateChanged over the socket within the start budget', async () => {
  const h = await startEngine();
  try {
    const started = Date.now();

    const loadReply = await h.request('load', {
      trackId: 'song-1',
      positionMs: 0,
      musicUserToken: 'mut-xyz',
    });
    assert.deepEqual(loadReply.result, { ok: true });

    await h.request('play');

    // The mocked MusicKit renderer completes the DRM license + queue setup and
    // begins playback (the Playback-Start Sequence).
    h.renderer.startPlayback('song-1', 0, 180000);

    const playing = await h.waitFor(
      (m) => m.method === 'stateChanged' && m.params.status === 'Playing'
    );
    const elapsed = Date.now() - started;

    assert.equal(playing.params.trackId, 'song-1');
    assert.equal(playing.params.durationMs, 180000);
    // Requirement 4.1 budgets 3 seconds for real audio output; with MusicKit
    // mocked the orchestration overhead must be a tiny fraction of that. This is
    // a smoke bound on the engine's own latency, not the network stream.
    assert.ok(elapsed < 3000, `engine orchestration latency ${elapsed}ms should be well under the 3s budget`);

    // The renderer received the load with the Music_User_Token so it could
    // authorize a full-length DRM stream, then a play.
    const load = h.renderer.commands.find((c) => c.type === 'load');
    assert.equal(load.musicUserToken, 'mut-xyz');
    assert.ok(h.renderer.commands.some((c) => c.type === 'play'));
  } finally {
    h.stop();
  }
});

// ---------------------------------------------------------------------------
// Stream / DRM failure handling (Requirement 4.13)
// ---------------------------------------------------------------------------

test('a stream (network) load failure surfaces as playbackError{network} over the socket', async () => {
  const h = await startEngine();
  try {
    await h.request('load', { trackId: 'song-2', positionMs: 0, musicUserToken: 'mut' });
    await h.request('play');

    // MusicKit reports a stream/network failure instead of readying.
    h.renderer.emit({ type: 'error', trackId: 'song-2', kind: 'network' });

    const err = await h.waitFor((m) => m.method === 'playbackError');
    assert.deepEqual(err.params, { trackId: 'song-2', kind: 'network' });

    // The engine never reported a Playing state: the only stateChanged is the
    // initial Loading, and the failure left it internally Paused at the current
    // position (Requirement 4.13) without a phantom Playing notification.
    assert.equal(
      h.inbox.some((m) => m.method === 'stateChanged' && m.params.status === 'Playing'),
      false
    );
    assert.equal(h.player._status, 'Paused');
  } finally {
    h.stop();
  }
});

test('a DRM license failure surfaces as playbackError{drm} over the socket', async () => {
  const h = await startEngine();
  try {
    await h.request('load', { trackId: 'song-3', positionMs: 0, musicUserToken: 'mut' });

    h.renderer.emit({ type: 'error', trackId: 'song-3', kind: 'drm' });

    const err = await h.waitFor((m) => m.method === 'playbackError');
    assert.deepEqual(err.params, { trackId: 'song-3', kind: 'drm' });
  } finally {
    h.stop();
  }
});

test('a load that never readies within the 10s timeout surfaces as playbackError{network}', async () => {
  // Drive the engine with controllable timers so the 10-second load/DRM timeout
  // is exercised deterministically rather than by waiting in real time.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amt-orch-to-'));
  const socketPath = path.join(dir, 'engine.sock');
  const renderer = makeFakeRenderer();

  let fireLoadTimeout = null;
  const player = new PlayerWindow({
    createWindow: () => renderer.win,
    notify: (method, params) => server.notify(method, params),
    // Capture the load-timeout callback; never auto-fire.
    setTimeout: (fn, ms) => {
      if (ms === LOAD_TIMEOUT_MS) fireLoadTimeout = fn;
      return { ms };
    },
    clearTimeout: () => {
      fireLoadTimeout = null;
    },
    setInterval: () => 0,
    clearInterval: () => {},
  });
  player.create();

  const server = new IpcServer({
    playerWindow: player,
    signInWindow: { open() {}, close() {} },
    socketPath,
  });
  server.start();

  const decoder = new FrameDecoder();
  const inbox = [];
  const client = net.connect(socketPath);
  client.on('data', (chunk) => {
    for (const payload of decoder.push(chunk)) inbox.push(JSON.parse(payload.toString('utf8')));
  });
  await new Promise((resolve, reject) => {
    client.once('connect', resolve);
    client.once('error', reject);
  });

  try {
    client.write(
      encodeFrame({ jsonrpc: '2.0', id: 1, method: 'load', params: { trackId: 'song-4', positionMs: 0, musicUserToken: 'mut' } })
    );
    // Let the request be dispatched, then fire the (mocked) 10-second deadline.
    await new Promise((r) => setTimeout(r, 20));
    assert.ok(fireLoadTimeout, 'the 10-second load timeout must have been armed');
    fireLoadTimeout();
    await new Promise((r) => setTimeout(r, 20));

    const err = inbox.find((m) => m.method === 'playbackError');
    assert.ok(err, 'a playbackError notification must be emitted on load timeout');
    assert.deepEqual(err.params, { trackId: 'song-4', kind: 'network' });
  } finally {
    client.destroy();
    server.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// ---------------------------------------------------------------------------
// Playback_Engine restart / reconnect at the last position (R4.10, R4.14)
// ---------------------------------------------------------------------------

test('after a lost connection, a restarted engine loads the current track at the last reported position, paused', async () => {
  // First engine: play a track and let it report progress, then the TUI loses
  // the connection (simulating a crash / 5-second no-response restart path).
  const first = await startEngine();
  let lastState;
  try {
    await first.request('load', { trackId: 'song-5', positionMs: 0, musicUserToken: 'mut' });
    await first.request('play');
    first.renderer.startPlayback('song-5', 0, 240000);

    // The renderer reports progress; the engine's position timer would normally
    // emit these — here we drive an explicit timeupdate + a state report.
    first.renderer.emit({ type: 'timeupdate', trackId: 'song-5', positionMs: 42000, durationMs: 240000 });
    first.player.seek(42000); // force the mirror to the reported position
    lastState = { trackId: 'song-5', positionMs: 42000 };

    await first.dropConnection();
  } finally {
    first.stop();
  }

  // The TUI owns Queue + current track + last reported position across the
  // restart. It starts a fresh engine and issues the reconnect load at the last
  // position; per Requirement 4.14 the track loads paused (no play follows).
  const second = await startEngine();
  try {
    const reply = await second.request('load', {
      trackId: lastState.trackId,
      positionMs: lastState.positionMs,
      musicUserToken: 'mut',
    });
    assert.deepEqual(reply.result, { ok: true });

    // The reconnect load carried the preserved track + last position.
    const load = second.renderer.commands.find((c) => c.type === 'load');
    assert.equal(load.trackId, 'song-5');
    assert.equal(load.positionMs, 42000);

    // No play command is sent on reconnect: the track is loaded paused.
    assert.equal(second.renderer.commands.some((c) => c.type === 'play'), false);

    // The renderer finishes the license/queue setup; the engine stays out of a
    // Playing state because no play was requested.
    second.renderer.emit({ type: 'ready', trackId: 'song-5', durationMs: 240000 });
    const everPlaying = second.inbox.some(
      (m) => m.method === 'stateChanged' && m.params.status === 'Playing'
    );
    assert.equal(everPlaying, false, 'a reconnect load must not auto-start playback');
  } finally {
    second.stop();
  }
});

// ---------------------------------------------------------------------------
// Sign_In_Window open / cancel / complete and token capture (R1.1-1.4, 1.14)
// ---------------------------------------------------------------------------

test('openSignIn routed through IPC completes with a captured token -> signInResult{ok} over the socket', async () => {
  const { electron, ipcMain } = makeFakeElectron();
  const h = await startEngine({ electron });
  try {
    const reply = await h.request('openSignIn', { developerToken: 'dev-abc' });
    assert.deepEqual(reply.result, { ok: true });

    // The fake Apple-hosted page became ready and asked for the developer token.
    const win = FakeSignInBrowserWindow.last;
    win.emit('ready-to-show');
    ipcMain.emitFrom(CHANNEL_READY, win.webContents);
    assert.deepEqual(win.webContents.sent[0], [CHANNEL_CONFIG, { developerToken: 'dev-abc' }]);

    // The user signs in; the mocked page returns a non-empty Music_User_Token.
    ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, '  captured-mut-123  ');

    const result = await h.waitFor((m) => m.method === 'signInResult');
    assert.deepEqual(result.params, { status: 'ok', musicUserToken: 'captured-mut-123' });
    assert.equal(win.isDestroyed(), true);
  } finally {
    h.stop();
  }
});

test('cancelling via closeSignIn before a token is captured -> signInResult{cancelled} over the socket', async () => {
  const { electron } = makeFakeElectron();
  const h = await startEngine({ electron });
  try {
    await h.request('openSignIn', { developerToken: 'dev-abc' });
    FakeSignInBrowserWindow.last.emit('ready-to-show');

    await h.request('closeSignIn');

    const result = await h.waitFor((m) => m.method === 'signInResult');
    assert.deepEqual(result.params, { status: 'cancelled' });
  } finally {
    h.stop();
  }
});

test('the user closing the sign-in window with no token -> signInResult{cancelled} over the socket', async () => {
  const { electron } = makeFakeElectron();
  const h = await startEngine({ electron });
  try {
    await h.request('openSignIn', { developerToken: 'dev-abc' });
    const win = FakeSignInBrowserWindow.last;
    win.emit('ready-to-show');

    // User closes the window via the chrome.
    win.emit('closed');

    const result = await h.waitFor((m) => m.method === 'signInResult');
    assert.deepEqual(result.params, { status: 'cancelled' });
  } finally {
    h.stop();
  }
});

test('a sign-in window that fails to open within the timeout -> signInError over the socket (R1.14)', async () => {
  const { electron } = makeFakeElectron();
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'amt-orch-si-'));
  const socketPath = path.join(dir, 'engine.sock');

  // A short open timeout so the test exercises R1.14 without a 10s wait; the
  // reported reason still names the 10-second budget to the user.
  const signInWindow = new SignInWindow({
    electron,
    openTimeoutMs: 5,
    onResult: (payload) => server.notify('signInResult', payload),
    onError: (payload) => server.notify('signInError', payload),
  });
  const server = new IpcServer({
    playerWindow: { load() {}, play() {}, pause() {}, seek: () => 0, setVolume: () => 0 },
    signInWindow,
    socketPath,
  });
  server.start();

  const decoder = new FrameDecoder();
  const inbox = [];
  const client = net.connect(socketPath);
  client.on('data', (chunk) => {
    for (const payload of decoder.push(chunk)) inbox.push(JSON.parse(payload.toString('utf8')));
  });
  await new Promise((resolve, reject) => {
    client.once('connect', resolve);
    client.once('error', reject);
  });

  try {
    client.write(encodeFrame({ jsonrpc: '2.0', id: 1, method: 'openSignIn', params: { developerToken: 'dev' } }));
    // The fake window is created but never emits ready-to-show, so the open
    // timeout fires.
    await new Promise((r) => setTimeout(r, 40));

    const err = inbox.find((m) => m.method === 'signInError');
    assert.ok(err, 'a signInError notification must be emitted on open timeout');
    assert.match(err.params.reason, /10 seconds/);
  } finally {
    client.destroy();
    server.stop();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('an empty token is not treated as sign-in completion (no premature signInResult)', async () => {
  const { electron, ipcMain } = makeFakeElectron();
  const h = await startEngine({ electron });
  try {
    await h.request('openSignIn', { developerToken: 'dev-abc' });
    const win = FakeSignInBrowserWindow.last;
    win.emit('ready-to-show');

    // The mocked page reports a blank token; this is not a completed sign-in.
    ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, '   ');

    // Give any erroneous notification a chance to arrive.
    await new Promise((r) => setTimeout(r, 20));
    assert.equal(h.inbox.some((m) => m.method === 'signInResult'), false);
    assert.equal(win.isDestroyed(), false);

    // A later real token still completes the flow.
    ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, 'real-mut');
    const result = await h.waitFor((m) => m.method === 'signInResult');
    assert.deepEqual(result.params, { status: 'ok', musicUserToken: 'real-mut' });
  } finally {
    h.stop();
  }
});
