// Sign-in renderer script.
//
// Runs the MusicKit JS authorize flow inside the Sign_In_Window: it configures
// MusicKit with the Developer_Token delivered by the main process, drives the
// Apple-hosted sign-in on a button press, and reports the resulting
// Music_User_Token (or a cancellation / error) back over the preload bridge
// exposed as `window.signIn`.
//
// The authorize core is factored into `runAuthorize` so it can be unit-tested in
// Node with a mocked MusicKit and bridge, with no DOM or real Apple page.

'use strict';

/**
 * Configure MusicKit with the Developer_Token, authorize the user, and report
 * the captured Music_User_Token over the bridge.
 *
 * Reporting rules (design "Sign-In Sequence"):
 *   - a non-empty token  -> bridge.reportToken(token)
 *   - an empty/no token  -> bridge.reportCancelled()  (authorize dismissed)
 *   - a thrown error      -> bridge.reportError(reason)
 *
 * @param {{
 *   getMusicKit: () => {
 *     configure: (opts: object) => Promise<any>|any,
 *     getInstance: () => { authorize: () => Promise<string> },
 *   },
 *   bridge: {
 *     reportToken: (t: string) => void,
 *     reportCancelled: () => void,
 *     reportError: (r: string) => void,
 *   },
 *   developerToken: string,
 *   appName?: string,
 *   appBuild?: string,
 * }} opts
 * @returns {Promise<void>}
 */
async function runAuthorize(opts) {
  const { getMusicKit, bridge, developerToken } = opts;
  const appName = opts.appName || 'apple-music-tui';
  const appBuild = opts.appBuild || '0.1.0';

  try {
    const MusicKit = getMusicKit();
    if (!MusicKit || typeof MusicKit.configure !== 'function') {
      bridge.reportError('MusicKit JS failed to load');
      return;
    }

    await MusicKit.configure({
      developerToken,
      app: { name: appName, build: appBuild },
    });

    const instance =
      typeof MusicKit.getInstance === 'function' ? MusicKit.getInstance() : null;
    if (!instance || typeof instance.authorize !== 'function') {
      bridge.reportError('MusicKit instance unavailable');
      return;
    }

    // authorize() resolves with the Music_User_Token on success. If the user
    // dismisses the Apple sign-in, it resolves with an empty value — treated as
    // a cancellation (Requirement 1.4).
    const token = await instance.authorize();
    const normalized = typeof token === 'string' ? token.trim() : '';
    if (normalized.length > 0) {
      bridge.reportToken(normalized);
    } else {
      bridge.reportCancelled();
    }
  } catch (err) {
    const reason = err && err.message ? String(err.message) : 'sign-in failed';
    bridge.reportError(reason);
  }
}

/**
 * Poll for the global MusicKit object, which the CDN script defines once it has
 * loaded. Resolves with the object, or rejects after the timeout.
 *
 * @param {{ getGlobal: () => any, timeoutMs?: number, intervalMs?: number,
 *           setTimeoutFn?: Function, clearTimeoutFn?: Function,
 *           setIntervalFn?: Function, clearIntervalFn?: Function }} opts
 * @returns {Promise<any>}
 */
function waitForMusicKit(opts) {
  const getGlobal = opts.getGlobal;
  const timeoutMs = typeof opts.timeoutMs === 'number' ? opts.timeoutMs : 10_000;
  const intervalMs = typeof opts.intervalMs === 'number' ? opts.intervalMs : 100;
  const setTimeoutFn = opts.setTimeoutFn || setTimeout;
  const clearTimeoutFn = opts.clearTimeoutFn || clearTimeout;
  const setIntervalFn = opts.setIntervalFn || setInterval;
  const clearIntervalFn = opts.clearIntervalFn || clearInterval;

  return new Promise((resolve, reject) => {
    const existing = getGlobal();
    if (existing) {
      resolve(existing);
      return;
    }
    const timer = setTimeoutFn(() => {
      clearIntervalFn(poll);
      reject(new Error('MusicKit JS did not load'));
    }, timeoutMs);
    const poll = setIntervalFn(() => {
      const mk = getGlobal();
      if (mk) {
        clearIntervalFn(poll);
        clearTimeoutFn(timer);
        resolve(mk);
      }
    }, intervalMs);
  });
}

// ---------------------------------------------------------------------------
// DOM wiring — only runs in the browser renderer, not under Node tests.
// ---------------------------------------------------------------------------
function initDom() {
  const bridge = window.signIn;
  if (!bridge) {
    return;
  }

  const statusEl = document.getElementById('status');
  const signInBtn = document.getElementById('signin');
  const cancelBtn = document.getElementById('cancel');

  const setStatus = (text) => {
    if (statusEl) statusEl.textContent = text;
  };

  let developerToken = '';

  bridge.onConfig((config) => {
    developerToken = (config && config.developerToken) || '';
    setStatus('Ready to sign in.');
    if (signInBtn) signInBtn.disabled = false;
  });

  if (signInBtn) {
    signInBtn.addEventListener('click', async () => {
      signInBtn.disabled = true;
      setStatus('Signing in\u2026');
      try {
        const MusicKit = await waitForMusicKit({
          getGlobal: () => window.MusicKit,
        });
        await runAuthorize({
          getMusicKit: () => MusicKit,
          bridge,
          developerToken,
        });
      } catch (err) {
        bridge.reportError(err && err.message ? String(err.message) : 'MusicKit JS did not load');
      }
    });
  }

  if (cancelBtn) {
    cancelBtn.addEventListener('click', () => {
      bridge.reportCancelled();
    });
  }

  // Tell the main process we are ready and want the Developer_Token.
  bridge.ready();
}

// Attach to the DOM when running in a browser environment.
if (typeof window !== 'undefined' && typeof document !== 'undefined') {
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initDom);
  } else {
    initDom();
  }
}

// Export the testable core for Node (`node --test`). In the renderer there is no
// `module`, so guard the assignment.
if (typeof module !== 'undefined' && module.exports) {
  module.exports = { runAuthorize, waitForMusicKit };
}
