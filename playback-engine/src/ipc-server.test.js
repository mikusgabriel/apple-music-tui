// Tests for the IPC server framing and JSON-RPC dispatch (task 11.2).
//
// Run with: npm test  (Node's built-in test runner, no extra deps).
//
// These tests exercise the length-prefixed framing (4-byte big-endian prefix),
// incremental reassembly across chunk boundaries, request dispatch to the
// player/sign-in handler stubs, value clamping, and notification emission over a
// real Unix domain socket. MusicKit playback (12.1) and sign-in (12.2) are out
// of scope; the player/sign-in handlers are replaced with recording spies.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const net = require('net');
const os = require('os');
const path = require('path');
const fs = require('fs');

const {
  IpcServer,
  FrameDecoder,
  encodeFrame,
  resolveSocketPath,
  clampVolume,
  clampNonNegative,
} = require('./ipc-server');

// ---------------------------------------------------------------------------
// Pure framing + helpers
// ---------------------------------------------------------------------------

test('encodeFrame writes a 4-byte big-endian length prefix', () => {
  const frame = encodeFrame({ jsonrpc: '2.0', method: 'play', params: {} });
  const declaredLen = frame.readUInt32BE(0);
  const payload = frame.subarray(4);
  assert.equal(declaredLen, payload.length);
  assert.deepEqual(JSON.parse(payload.toString('utf8')), {
    jsonrpc: '2.0',
    method: 'play',
    params: {},
  });
});

test('FrameDecoder decodes a single whole frame', () => {
  const d = new FrameDecoder();
  const frame = encodeFrame({ method: 'pause' });
  const frames = d.push(frame);
  assert.equal(frames.length, 1);
  assert.deepEqual(JSON.parse(frames[0].toString('utf8')), { method: 'pause' });
});

test('FrameDecoder reassembles a frame split across chunks', () => {
  const d = new FrameDecoder();
  const frame = encodeFrame({ method: 'seek', params: { positionMs: 1234 } });
  // Split at an arbitrary interior byte, including inside the length prefix.
  assert.deepEqual(d.push(frame.subarray(0, 2)), []);
  assert.deepEqual(d.push(frame.subarray(2, 7)), []);
  const out = d.push(frame.subarray(7));
  assert.equal(out.length, 1);
  assert.deepEqual(JSON.parse(out[0].toString('utf8')), {
    method: 'seek',
    params: { positionMs: 1234 },
  });
});

test('FrameDecoder yields multiple frames from one chunk', () => {
  const d = new FrameDecoder();
  const a = encodeFrame({ method: 'play' });
  const b = encodeFrame({ method: 'pause' });
  const out = d.push(Buffer.concat([a, b]));
  assert.equal(out.length, 2);
  assert.equal(JSON.parse(out[0].toString('utf8')).method, 'play');
  assert.equal(JSON.parse(out[1].toString('utf8')).method, 'pause');
});

test('clampVolume clamps to 0-100 and coerces junk to 0', () => {
  assert.equal(clampVolume(50), 50);
  assert.equal(clampVolume(150), 100);
  assert.equal(clampVolume(-10), 0);
  assert.equal(clampVolume('abc'), 0);
  assert.equal(clampVolume(33.7), 34);
});

test('clampNonNegative floors negatives and junk to 0', () => {
  assert.equal(clampNonNegative(500), 500);
  assert.equal(clampNonNegative(-1), 0);
  assert.equal(clampNonNegative('x'), 0);
});

test('resolveSocketPath honors XDG_RUNTIME_DIR then falls back to tmp', () => {
  const prev = process.env.XDG_RUNTIME_DIR;
  try {
    process.env.XDG_RUNTIME_DIR = '/run/user/1000';
    assert.equal(resolveSocketPath(), '/run/user/1000/apple-music-tui/engine.sock');
    delete process.env.XDG_RUNTIME_DIR;
    assert.equal(
      resolveSocketPath(),
      path.join(os.tmpdir(), 'apple-music-tui', 'engine.sock')
    );
  } finally {
    if (prev === undefined) delete process.env.XDG_RUNTIME_DIR;
    else process.env.XDG_RUNTIME_DIR = prev;
  }
});

// ---------------------------------------------------------------------------
// Dispatch helpers (no socket)
// ---------------------------------------------------------------------------

function makeSpies() {
  const calls = [];
  const playerWindow = {
    load: (trackId, positionMs, musicUserToken) =>
      calls.push(['load', trackId, positionMs, musicUserToken]),
    play: () => calls.push(['play']),
    pause: () => calls.push(['pause']),
    seek: (positionMs) => {
      calls.push(['seek', positionMs]);
      // The real player (task 12.1) clamps to the track bounds and returns the
      // applied position; the spy mimics the lower bound (0).
      return Math.max(0, positionMs);
    },
    setVolume: (level) => {
      calls.push(['setVolume', level]);
      return level;
    },
  };
  const signInWindow = {
    open: (developerToken) => calls.push(['open', developerToken]),
    close: () => calls.push(['close']),
  };
  return { calls, playerWindow, signInWindow };
}

test('handleRequest routes transport methods to the player window', () => {
  const { calls, playerWindow, signInWindow } = makeSpies();
  const server = new IpcServer({ playerWindow, signInWindow });
  const replies = [];
  server._write = (msg) => replies.push(msg);

  server.handleRequest({ jsonrpc: '2.0', id: 1, method: 'load', params: { trackId: 't1', positionMs: 10, musicUserToken: 'mut' } });
  server.handleRequest({ jsonrpc: '2.0', id: 2, method: 'play', params: {} });
  server.handleRequest({ jsonrpc: '2.0', id: 3, method: 'pause', params: {} });

  assert.deepEqual(calls[0], ['load', 't1', 10, 'mut']);
  assert.deepEqual(calls[1], ['play']);
  assert.deepEqual(calls[2], ['pause']);
  assert.deepEqual(replies[0], { jsonrpc: '2.0', id: 1, result: { ok: true } });
  assert.deepEqual(replies[1], { jsonrpc: '2.0', id: 2, result: { ok: true } });
  assert.deepEqual(replies[2], { jsonrpc: '2.0', id: 3, result: { ok: true } });
});

test('seek and setVolume return clamped values in the result', () => {
  const { playerWindow, signInWindow } = makeSpies();
  const server = new IpcServer({ playerWindow, signInWindow });
  const replies = [];
  server._write = (msg) => replies.push(msg);

  server.handleRequest({ jsonrpc: '2.0', id: 1, method: 'seek', params: { positionMs: -5 } });
  server.handleRequest({ jsonrpc: '2.0', id: 2, method: 'setVolume', params: { level: 250 } });

  assert.deepEqual(replies[0].result, { positionMs: 0 });
  assert.deepEqual(replies[1].result, { level: 100 });
});

test('openSignIn/closeSignIn route to the sign-in window', () => {
  const { calls, playerWindow, signInWindow } = makeSpies();
  const server = new IpcServer({ playerWindow, signInWindow });
  const replies = [];
  server._write = (msg) => replies.push(msg);

  server.handleRequest({ jsonrpc: '2.0', id: 1, method: 'openSignIn', params: { developerToken: 'dev' } });
  server.handleRequest({ jsonrpc: '2.0', id: 2, method: 'closeSignIn', params: {} });

  assert.deepEqual(calls[0], ['open', 'dev']);
  assert.deepEqual(calls[1], ['close']);
  assert.deepEqual(replies[0].result, { ok: true });
  assert.deepEqual(replies[1].result, { ok: true });
});

test('shutdown replies ok and invokes the onShutdown callback', () => {
  const { playerWindow, signInWindow } = makeSpies();
  let shutdownCalled = false;
  const server = new IpcServer({ playerWindow, signInWindow, onShutdown: () => { shutdownCalled = true; } });
  const replies = [];
  server._write = (msg) => replies.push(msg);

  server.handleRequest({ jsonrpc: '2.0', id: 9, method: 'shutdown', params: {} });

  assert.deepEqual(replies[0].result, { ok: true });
  assert.equal(shutdownCalled, true);
});

test('unknown method returns a JSON-RPC method-not-found error', () => {
  const { playerWindow, signInWindow } = makeSpies();
  const server = new IpcServer({ playerWindow, signInWindow });
  const replies = [];
  server._write = (msg) => replies.push(msg);

  server.handleRequest({ jsonrpc: '2.0', id: 7, method: 'bogus', params: {} });

  assert.equal(replies[0].error.code, -32601);
});

test('notify emits a frameable JSON-RPC notification with no id', () => {
  const { playerWindow, signInWindow } = makeSpies();
  const server = new IpcServer({ playerWindow, signInWindow });
  const written = [];
  server._write = (msg) => written.push(msg);

  server.notify('stateChanged', { status: 'Playing', positionMs: 0, durationMs: 1000, trackId: 't1' });

  assert.deepEqual(written[0], {
    jsonrpc: '2.0',
    method: 'stateChanged',
    params: { status: 'Playing', positionMs: 0, durationMs: 1000, trackId: 't1' },
  });
  assert.equal('id' in written[0], false);
});

// ---------------------------------------------------------------------------
// End-to-end over a real Unix domain socket
// ---------------------------------------------------------------------------

test('round-trips a framed request and notification over a real socket', async () => {
  const { playerWindow, signInWindow } = makeSpies();
  const socketPath = path.join(
    fs.mkdtempSync(path.join(os.tmpdir(), 'amt-ipc-')),
    'engine.sock'
  );
  const server = new IpcServer({ playerWindow, signInWindow, socketPath });
  server.start();

  try {
    await new Promise((resolve, reject) => {
      const client = net.connect(socketPath);
      const decoder = new FrameDecoder();
      const received = [];

      client.on('connect', () => {
        client.write(encodeFrame({ jsonrpc: '2.0', id: 1, method: 'seek', params: { positionMs: 42 } }));
      });

      client.on('data', (chunk) => {
        for (const payload of decoder.push(chunk)) {
          received.push(JSON.parse(payload.toString('utf8')));
        }
        if (received.length === 1) {
          // Got the seek reply; now have the server push a notification.
          server.notify('trackFinished', { trackId: 't1' });
        }
        if (received.length === 2) {
          try {
            assert.deepEqual(received[0], { jsonrpc: '2.0', id: 1, result: { positionMs: 42 } });
            assert.deepEqual(received[1], {
              jsonrpc: '2.0',
              method: 'trackFinished',
              params: { trackId: 't1' },
            });
            client.end();
            resolve();
          } catch (err) {
            reject(err);
          }
        }
      });

      client.on('error', reject);
    });
  } finally {
    server.stop();
  }
});
