// Tests for the loopback sign-in asset server (asset-server.js).

'use strict';

const test = require('node:test');
const assert = require('node:assert');
const http = require('http');
const path = require('path');

const { AssetServer, resolveWithinRoot } = require('./asset-server');

const SIGN_IN_DIR = path.join(__dirname, 'sign-in');

function get(baseUrl, reqPath) {
  return new Promise((resolve, reject) => {
    http
      .get(`${baseUrl}${reqPath}`, (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (body += c));
        res.on('end', () =>
          resolve({ status: res.statusCode, body, contentType: res.headers['content-type'] || '' })
        );
      })
      .on('error', reject);
  });
}

test('serves sign-in.html on a loopback port and maps "/" to it', async () => {
  const server = new AssetServer({ root: SIGN_IN_DIR });
  const { host, port, baseUrl } = await server.start();
  try {
    assert.strictEqual(host, '127.0.0.1');
    assert.ok(port > 0);
    assert.strictEqual(baseUrl, `http://127.0.0.1:${port}`);
    const root = await get(baseUrl, '/');
    assert.strictEqual(root.status, 200);
    assert.match(root.contentType, /text\/html/);
    assert.match(root.body, /Apple Music/);
    const explicit = await get(baseUrl, '/sign-in.html');
    assert.strictEqual(explicit.body, root.body);
  } finally {
    server.stop();
  }
});

test('404s a missing file and rejects traversal', async () => {
  const server = new AssetServer({ root: SIGN_IN_DIR });
  const { baseUrl } = await server.start();
  try {
    assert.strictEqual((await get(baseUrl, '/nope.js')).status, 404);
    const escaped = await get(baseUrl, '/../sign-in-window.js');
    assert.ok(escaped.status === 403 || escaped.status === 404);
  } finally {
    server.stop();
  }
});

test('resolveWithinRoot maps and confines paths (pure)', () => {
  const root = path.resolve('/srv/assets');
  assert.strictEqual(resolveWithinRoot(root, '/'), path.join(root, 'sign-in.html'));
  assert.strictEqual(resolveWithinRoot(root, '/sign-in.js'), path.join(root, 'sign-in.js'));
  assert.strictEqual(resolveWithinRoot(root, '/../secrets.txt'), null);
});
