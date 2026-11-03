// Tests for the Apple Music API origin-header rewriter (origin-headers.js).

'use strict';

const test = require('node:test');
const assert = require('node:assert');

const { rewriteHeaders, SPOOF_ORIGIN, MATCH_URLS } = require('./origin-headers');

test('spoof origin is https://music.apple.com (matches token root_https_origin)', () => {
  assert.strictEqual(SPOOF_ORIGIN, 'https://music.apple.com');
});

test('rewrites Origin and Referer, preserving other headers', () => {
  const out = rewriteHeaders({
    Authorization: 'Bearer dev',
    'Music-User-Token': 'mut',
    Origin: 'http://127.0.0.1:5000',
    Referer: 'http://127.0.0.1:5000/sign-in.html',
  });
  assert.strictEqual(out.Origin, 'https://music.apple.com');
  assert.strictEqual(out.Referer, 'https://music.apple.com/');
  // Unrelated headers are preserved.
  assert.strictEqual(out.Authorization, 'Bearer dev');
  assert.strictEqual(out['Music-User-Token'], 'mut');
});

test('sets Origin/Referer even when absent from the request', () => {
  const out = rewriteHeaders({ Accept: 'application/json' });
  assert.strictEqual(out.Origin, 'https://music.apple.com');
  assert.strictEqual(out.Referer, 'https://music.apple.com/');
  assert.strictEqual(out.Accept, 'application/json');
});

test('only the Apple Music API hosts are matched', () => {
  assert.ok(MATCH_URLS.includes('https://api.music.apple.com/*'));
  // The DRM playback/license hosts ARE matched (streams need the origin).
  assert.ok(MATCH_URLS.includes('https://play.itunes.apple.com/*'));
  assert.ok(MATCH_URLS.includes('https://play-edge.itunes.apple.com/*'));
  // The authorize popup / appleid hosts are NOT in the match list, so their
  // cross-window postMessage handshake is left untouched.
  assert.ok(!MATCH_URLS.some((u) => u.includes('appleid')));
  assert.ok(!MATCH_URLS.some((u) => u.includes('buy.itunes.apple.com')));
  assert.ok(!MATCH_URLS.some((u) => u.includes('authorize.music.apple.com')));
});
