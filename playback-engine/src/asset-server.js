// Loopback static asset server for the Sign_In_Window.
//
// The sign-in page must not run on a `file://` origin (MusicKit rejects it), so
// it is served over a loopback HTTP origin and loaded with `loadURL`. We do NOT
// intercept the global https scheme (that breaks Apple's authorize popup and its
// cross-window postMessage handshake). Instead, the engine rewrites the `Origin`
// header on requests to `api.music.apple.com` to the origin the Developer_Token
// is bound to (`https://music.apple.com`); see origin-headers.js.
//
// The server binds 127.0.0.1 on an OS-assigned ephemeral port, serves only the
// whitelisted sign-in directory (path traversal rejected), and is torn down when
// the engine exits.

'use strict';

const http = require('http');
const fs = require('fs');
const path = require('path');

const CONTENT_TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.map': 'application/json; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.ico': 'image/x-icon',
  '.png': 'image/png',
  '.svg': 'image/svg+xml',
};

/**
 * Resolve a request URL path to a real file inside `root`, or `null` when the
 * path escapes `root` (traversal) or cannot be decoded.
 *
 * @param {string} root absolute directory the server is allowed to serve
 * @param {string} urlPath the request pathname (query already stripped)
 * @returns {string|null}
 */
function resolveWithinRoot(root, urlPath) {
  const rel = urlPath === '/' || urlPath === '' ? '/sign-in.html' : urlPath;
  let decoded;
  try {
    decoded = decodeURIComponent(rel);
  } catch (_err) {
    return null;
  }
  const candidate = path.normalize(path.join(root, decoded));
  if (candidate !== root && !candidate.startsWith(root + path.sep)) {
    return null;
  }
  return candidate;
}

class AssetServer {
  /**
   * @param {{ root: string, host?: string }} opts
   */
  constructor(opts) {
    if (!opts || typeof opts.root !== 'string') {
      throw new TypeError('AssetServer requires a { root } directory');
    }
    this._root = path.resolve(opts.root);
    this._host = opts.host || '127.0.0.1';
    this._server = null;
    this._port = null;
  }

  /** @returns {Promise<{host: string, port: number, baseUrl: string}>} */
  start() {
    return new Promise((resolve, reject) => {
      const server = http.createServer((req, res) => this._handle(req, res));
      server.on('error', reject);
      server.listen(0, this._host, () => {
        const addr = server.address();
        this._server = server;
        this._port = addr.port;
        resolve({ host: this._host, port: this._port, baseUrl: this.baseUrl() });
      });
    });
  }

  baseUrl() {
    if (this._port === null) {
      throw new Error('AssetServer.baseUrl() called before start()');
    }
    return `http://${this._host}:${this._port}`;
  }

  signInUrl() {
    return `${this.baseUrl()}/sign-in.html`;
  }

  stop() {
    if (this._server) {
      this._server.close();
      this._server = null;
      this._port = null;
    }
  }

  _handle(req, res) {
    if (req.method !== 'GET' && req.method !== 'HEAD') {
      res.writeHead(405, { Allow: 'GET, HEAD' });
      res.end();
      return;
    }
    const urlPath = (req.url || '/').split('?')[0];
    const filePath = resolveWithinRoot(this._root, urlPath);
    if (!filePath) {
      res.writeHead(403);
      res.end('Forbidden');
      return;
    }
    fs.readFile(filePath, (err, data) => {
      if (err) {
        res.writeHead(err.code === 'ENOENT' ? 404 : 500);
        res.end(err.code === 'ENOENT' ? 'Not Found' : 'Server Error');
        return;
      }
      const ext = path.extname(filePath).toLowerCase();
      const type = CONTENT_TYPES[ext] || 'application/octet-stream';
      res.writeHead(200, { 'Content-Type': type });
      if (req.method === 'HEAD') {
        res.end();
      } else {
        res.end(data);
      }
    });
  }
}

module.exports = { AssetServer, resolveWithinRoot, CONTENT_TYPES };
