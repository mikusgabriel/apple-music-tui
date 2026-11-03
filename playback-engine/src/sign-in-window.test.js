// Tests for the Sign_In_Window host (task 12.2).
//
// Run with: npm test  (Node's built-in test runner, no extra deps).
//
// These exercise the sign-in orchestration with a fake `electron` injected: the
// open flow and ready-to-show, Music_User_Token capture -> signInResult(ok),
// cancellation via in-page cancel and via window close, close() cancellation
// when no token was captured, keeping an obtained token when the window later
// closes, the 10-second open-timeout -> signInError, window-creation failure ->
// signInError, and that an empty token is not treated as success. The
// Apple-hosted page and MusicKit are mocked; no BrowserWindow is really created.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('events');

const {
  SignInWindow,
  normalizeToken,
  CHANNEL_READY,
  CHANNEL_TOKEN,
  CHANNEL_CANCELLED,
  CHANNEL_ERROR,
  CHANNEL_CONFIG,
} = require('./sign-in-window');

const { runAuthorize } = require('./sign-in/sign-in');

// ---------------------------------------------------------------------------
// Fake Electron harness
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

class FakeBrowserWindow extends EventEmitter {
  constructor(opts) {
    super();
    FakeBrowserWindow.lastOptions = opts;
    this.webContents = new FakeWebContents();
    this._destroyed = false;
    this.shown = false;
    this.focused = false;
    this.loadedFile = null;
  }

  loadFile(file) {
    this.loadedFile = file;
    return Promise.resolve();
  }

  show() {
    this.shown = true;
  }

  focus() {
    this.focused = true;
  }

  isDestroyed() {
    return this._destroyed;
  }

  destroy() {
    this._destroyed = true;
    // Electron emits 'closed' when a window is destroyed.
    this.emit('closed');
  }
}

// A fake ipcMain that records registered listeners and can emit to them with a
// sender identity, mirroring Electron's (event, ...args) convention.
function makeFakeIpcMain() {
  const emitter = new EventEmitter();
  emitter.setMaxListeners(0);
  return {
    on: (channel, handler) => emitter.on(channel, handler),
    removeListener: (channel, handler) => emitter.removeListener(channel, handler),
    emitFrom: (channel, sender, ...args) =>
      emitter.emit(channel, { sender }, ...args),
    listenerCount: (channel) => emitter.listenerCount(channel),
    _emitter: emitter,
  };
}

function makeFakeElectron() {
  const ipcMain = makeFakeIpcMain();
  const electron = {
    ipcMain,
    BrowserWindow: FakeBrowserWindow,
  };
  return { electron, ipcMain };
}

function makeRecorder() {
  const results = [];
  const errors = [];
  return {
    results,
    errors,
    onResult: (payload) => results.push(payload),
    onError: (payload) => errors.push(payload),
  };
}

// ---------------------------------------------------------------------------
// normalizeToken
// ---------------------------------------------------------------------------

test('normalizeToken trims and rejects empty / non-string tokens', () => {
  assert.equal(normalizeToken('  mut-123  '), 'mut-123');
  assert.equal(normalizeToken('abc'), 'abc');
  assert.equal(normalizeToken(''), null);
  assert.equal(normalizeToken('   '), null);
  assert.equal(normalizeToken(undefined), null);
  assert.equal(normalizeToken(null), null);
  assert.equal(normalizeToken(42), null);
});

// ---------------------------------------------------------------------------
// Open flow
// ---------------------------------------------------------------------------

test('open creates a hardened, initially-hidden window and loads the sign-in page', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');

  const opts = FakeBrowserWindow.lastOptions;
  assert.equal(opts.show, false);
  assert.equal(opts.webPreferences.contextIsolation, true);
  assert.equal(opts.webPreferences.nodeIntegration, false);
  assert.equal(opts.webPreferences.sandbox, true);
  assert.ok(opts.webPreferences.preload.endsWith('sign-in-preload.js'));
  assert.ok(siw.window.loadedFile.endsWith('sign-in.html'));
});

test('ready-to-show reveals the window and clears the open timeout', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec, openTimeoutMs: 10_000 });

  siw.open('dev-token');
  const win = siw.window;
  assert.equal(win.shown, false);
  win.emit('ready-to-show');
  assert.equal(win.shown, true);
  // The open timer is cleared on ready-to-show.
  assert.equal(siw._openTimer, null);
});

test('ready signal delivers the developer token to the renderer', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token-xyz', 'us');
  const win = siw.window;

  ipcMain.emitFrom(CHANNEL_READY, win.webContents);

  assert.deepEqual(win.webContents.sent[0], [
    CHANNEL_CONFIG,
    { developerToken: 'dev-token-xyz', storefrontId: 'us' },
  ]);
});

// ---------------------------------------------------------------------------
// Successful capture
// ---------------------------------------------------------------------------

test('a non-empty token yields signInResult(ok) and tears the window down', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;
  win.emit('ready-to-show');

  ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, '  music-user-token-123  ');

  assert.deepEqual(rec.results, [
    { status: 'ok', musicUserToken: 'music-user-token-123' },
  ]);
  assert.deepEqual(rec.errors, []);
  assert.equal(win.isDestroyed(), true);
  assert.equal(siw.window, null);
  // IPC handlers are detached after teardown.
  assert.equal(ipcMain.listenerCount(CHANNEL_TOKEN), 0);
});

test('an empty token is ignored and does not settle the flow', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;

  ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, '   ');

  assert.deepEqual(rec.results, []);
  assert.deepEqual(rec.errors, []);
  assert.equal(win.isDestroyed(), false);

  // A later real token still succeeds.
  ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, 'real-token');
  assert.deepEqual(rec.results, [{ status: 'ok', musicUserToken: 'real-token' }]);
});

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

test('closing the window with no token reports signInResult(cancelled)', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;
  win.emit('ready-to-show');

  // User closes the window via the chrome.
  win.emit('closed');

  assert.deepEqual(rec.results, [{ status: 'cancelled' }]);
  assert.deepEqual(rec.errors, []);
});

test('in-page cancel reports signInResult(cancelled)', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;

  ipcMain.emitFrom(CHANNEL_CANCELLED, win.webContents);

  assert.deepEqual(rec.results, [{ status: 'cancelled' }]);
  assert.equal(win.isDestroyed(), true);
});

test('close() with no captured token reports cancelled and tears down', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;

  siw.close();

  assert.deepEqual(rec.results, [{ status: 'cancelled' }]);
  assert.equal(win.isDestroyed(), true);
  assert.equal(siw.window, null);
});

// ---------------------------------------------------------------------------
// Keeping an obtained token (Requirement 10.8)
// ---------------------------------------------------------------------------

test('a window close AFTER a token was captured does not emit a second result', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;

  // The renderer reports the token; this destroys the window (emits 'closed').
  ipcMain.emitFrom(CHANNEL_TOKEN, win.webContents, 'kept-token');

  // Exactly one ok result, no spurious cancellation from the close.
  assert.deepEqual(rec.results, [{ status: 'ok', musicUserToken: 'kept-token' }]);
  assert.deepEqual(rec.errors, []);
});

test('close() after success keeps the result and emits nothing further', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  ipcMain.emitFrom(CHANNEL_TOKEN, siw.window ? siw.window.webContents : null, 'tok');
  // window is already torn down; a late close() is a no-op.
  siw.close();

  assert.deepEqual(rec.results, [{ status: 'ok', musicUserToken: 'tok' }]);
});

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

test('the open timeout reports signInError when the window never readies', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec, openTimeoutMs: 5 });

  siw.open('dev-token');
  const win = siw.window;

  return new Promise((resolve) => {
    setTimeout(() => {
      assert.equal(rec.results.length, 0);
      assert.equal(rec.errors.length, 1);
      assert.match(rec.errors[0].reason, /10 seconds/);
      assert.equal(win.isDestroyed(), true);
      resolve();
    }, 25);
  });
});

test('a window creation failure reports signInError', () => {
  const { electron } = makeFakeElectron();
  // Replace BrowserWindow with one that throws on construction (no display).
  electron.BrowserWindow = class {
    constructor() {
      throw new Error('no display available');
    }
  };
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');

  assert.deepEqual(rec.results, []);
  assert.equal(rec.errors.length, 1);
  assert.equal(rec.errors[0].reason, 'no display available');
  assert.equal(siw.window, null);
});

test('did-fail-load (non-abort) reports signInError and tears down', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;
  win.webContents.emit('did-fail-load', {}, -105, 'NAME_NOT_RESOLVED');

  assert.equal(rec.errors.length, 1);
  assert.match(rec.errors[0].reason, /NAME_NOT_RESOLVED/);
  assert.equal(win.isDestroyed(), true);
});

test('a benign aborted navigation (-3) does not report an error', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const win = siw.window;
  win.webContents.emit('did-fail-load', {}, -3, 'ERR_ABORTED');

  assert.deepEqual(rec.errors, []);
  assert.equal(win.isDestroyed(), false);
});

// ---------------------------------------------------------------------------
// Re-entrancy
// ---------------------------------------------------------------------------

test('open while already open focuses the existing window', () => {
  const { electron } = makeFakeElectron();
  const rec = makeRecorder();
  const siw = new SignInWindow({ electron, ...rec });

  siw.open('dev-token');
  const first = siw.window;
  siw.open('dev-token');

  assert.equal(siw.window, first);
  assert.equal(first.focused, true);
});

test('setCallbacks wires forwarding after construction', () => {
  const { electron, ipcMain } = makeFakeElectron();
  const results = [];
  const siw = new SignInWindow({ electron });
  siw.setCallbacks({ onResult: (p) => results.push(p) });

  siw.open('dev-token');
  ipcMain.emitFrom(CHANNEL_TOKEN, siw.window.webContents, 'tok');

  assert.deepEqual(results, [{ status: 'ok', musicUserToken: 'tok' }]);
});

// ---------------------------------------------------------------------------
// runAuthorize (renderer core) — mocked MusicKit + bridge
// ---------------------------------------------------------------------------

function makeBridge() {
  const calls = [];
  return {
    calls,
    reportToken: (t) => calls.push(['token', t]),
    reportCancelled: () => calls.push(['cancelled']),
    reportError: (r) => calls.push(['error', r]),
  };
}

test('runAuthorize reports a captured token', async () => {
  const bridge = makeBridge();
  const MusicKit = {
    configure: async () => {},
    getInstance: () => ({ authorize: async () => '  mut-abc  ' }),
  };
  await runAuthorize({ getMusicKit: () => MusicKit, bridge, developerToken: 'dev' });
  assert.deepEqual(bridge.calls, [['token', 'mut-abc']]);
});

test('runAuthorize treats an empty authorize result as a cancellation', async () => {
  const bridge = makeBridge();
  const MusicKit = {
    configure: async () => {},
    getInstance: () => ({ authorize: async () => '' }),
  };
  await runAuthorize({ getMusicKit: () => MusicKit, bridge, developerToken: 'dev' });
  assert.deepEqual(bridge.calls, [['cancelled']]);
});

test('runAuthorize reports an error when authorize rejects', async () => {
  const bridge = makeBridge();
  const MusicKit = {
    configure: async () => {},
    getInstance: () => ({
      authorize: async () => {
        throw new Error('user denied');
      },
    }),
  };
  await runAuthorize({ getMusicKit: () => MusicKit, bridge, developerToken: 'dev' });
  assert.deepEqual(bridge.calls, [['error', 'user denied']]);
});

test('runAuthorize reports an error when MusicKit failed to load', async () => {
  const bridge = makeBridge();
  await runAuthorize({ getMusicKit: () => null, bridge, developerToken: 'dev' });
  assert.deepEqual(bridge.calls, [['error', 'MusicKit JS failed to load']]);
});
