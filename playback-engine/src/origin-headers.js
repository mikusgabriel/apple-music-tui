// Origin header rewriter for Apple Music API requests.
//
// Why this exists: the web-player Developer_Token is bound to the `apple.com`
// origin (its JWT carries `root_https_origin: ["apple.com"]`). Apple's API
// rejects any request whose `Origin` is not under that domain with 401/403 —
// which surfaces inside MusicKit as "Storefront Country Code error." (on
// `/me/storefront`) or "AUTHORIZATION_ERROR: Unauthorized".
//
// The sign-in page runs on a loopback `http://127.0.0.1:<port>` origin (it only
// needs to avoid `file://`). That means its XHRs to `api.music.apple.com` carry
// the wrong `Origin`. Rather than hijack the whole https scheme (which breaks
// Apple's authorize popup and its cross-window postMessage), we rewrite just the
// `Origin` (and `Referer`) headers on requests to Apple's API hosts so they look
// like they came from `https://music.apple.com`. A curl test confirmed the API
// accepts the token purely based on this header.
//
// We scope the rewrite narrowly to the Apple Music API hosts so nothing else —
// the authorize popup, the CDN, appleid.apple.com — is touched.

'use strict';

// The origin the Developer_Token authorizes.
const SPOOF_ORIGIN = 'https://music.apple.com';

// Request URLs whose Origin/Referer we rewrite so the web-player Developer_Token
// (bound to the `apple.com` origin) is accepted. These are the XHR/fetch
// endpoints that validate the origin:
//   - api.music.apple.com / amp-api.music.apple.com: catalog, /me/*, library.
//   - play.itunes.apple.com / play-edge.itunes.apple.com: the DRM playback /
//     Widevine license requests MusicKit issues to start a full-length stream.
// The interactive sign-in/authorize pages (appleid.apple.com, *-buy.itunes…)
// are deliberately NOT matched: they use a cross-window postMessage handshake
// that must not be disturbed.
const MATCH_URLS = [
  'https://api.music.apple.com/*',
  'https://amp-api.music.apple.com/*',
  'https://play.itunes.apple.com/*',
  'https://play-edge.itunes.apple.com/*',
];

/**
 * Decide the rewritten header set for a request, given its current headers.
 * Pure so it can be unit-tested without Electron.
 *
 * @param {Record<string,string>} headers the outgoing request headers
 * @returns {Record<string,string>} the headers to send (a new object)
 */
function rewriteHeaders(headers) {
  const next = { ...(headers || {}) };
  next.Origin = SPOOF_ORIGIN;
  next.Referer = `${SPOOF_ORIGIN}/`;
  return next;
}

/**
 * Install the header rewriter on the given Electron session.
 *
 * @param {{ session: import('electron').Session }} deps
 */
function installOriginHeaders(deps) {
  const { session } = deps;
  session.webRequest.onBeforeSendHeaders({ urls: MATCH_URLS }, (details, callback) => {
    callback({ requestHeaders: rewriteHeaders(details.requestHeaders) });
  });
}

module.exports = {
  installOriginHeaders,
  rewriteHeaders,
  SPOOF_ORIGIN,
  MATCH_URLS,
};
