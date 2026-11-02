// Renderer-side MusicKit JS driver for the hidden player window.
//
// Runs in the untrusted renderer with only `window.playerBridge` (the preload
// bridge) reachable — no Node/Electron. It translates transport commands from
// the main process into MusicKit JS calls and forwards MusicKit's player events
// back up as normalized events. The authoritative state machine and the
// JSON-RPC notification mapping live in the main process (player-window.js);
// this file is a thin adapter over MusicKit's imperative API.
//
// Normalized events emitted to main (matching PlayerWindow's handler):
//   { type: 'ready', trackId, durationMs }
//   { type: 'playing' | 'paused', trackId, positionMs, durationMs }
//   { type: 'timeupdate', trackId, positionMs, durationMs }
//   { type: 'ended', trackId }
//   { type: 'error', trackId, kind: 'network' | 'drm' }
//   { type: 'unavailable', trackId }

'use strict';

(function initPlayerRenderer() {
  const bridge = window.playerBridge;
  if (!bridge) {
    // Without the preload bridge there is no channel to the main process, so
    // there is nothing this document can usefully do.
    return;
  }

  /** @type {any} */ let music = null;
  /** Track id of the pending/active load, echoed back on every event. */
  let currentTrackId = null;

  const emit = (event) => bridge.emit(event);

  const toMs = (seconds) =>
    typeof seconds === 'number' && Number.isFinite(seconds) ? Math.round(seconds * 1000) : 0;

  const durationMs = () => {
    const d = music && music.currentPlaybackDuration;
    return toMs(d);
  };

  const positionMs = () => {
    const p = music && music.currentPlaybackTime;
    return toMs(p);
  };

  // MusicKit PlaybackStates -> our normalized events. The numeric enum values are
  // stable in MusicKit v3 (none=0, loading=1, playing=2, paused=3, stopped=4,
  // ended=5, seeking=6, ...). We key off the names when available and fall back
  // to the numbers.
  function onPlaybackStateChange(evt) {
    const state = evt && (evt.state !== undefined ? evt.state : evt.oldState);
    const names = window.MusicKit && window.MusicKit.PlaybackStates;
    const is = (name, num) => (names ? state === names[name] : state === num);

    if (is('playing', 2)) {
      emit({ type: 'playing', trackId: currentTrackId, positionMs: positionMs(), durationMs: durationMs() });
    } else if (is('paused', 3)) {
      emit({ type: 'paused', trackId: currentTrackId, positionMs: positionMs(), durationMs: durationMs() });
    } else if (is('ended', 5)) {
      emit({ type: 'ended', trackId: currentTrackId });
    }
  }

  function onPlaybackTimeChange() {
    emit({ type: 'timeupdate', trackId: currentTrackId, positionMs: positionMs(), durationMs: durationMs() });
  }

  // Classify a MusicKit error as a DRM/license failure or a network/stream
  // failure, matching Requirement 4.13's two named failure types. Anything that
  // mentions key/license/DRM/EME is treated as drm; everything else is network.
  function classifyError(err) {
    const text = String((err && (err.message || err.name)) || err || '').toLowerCase();
    if (/drm|license|widevine|eme|key|decrypt|content.?decryption/.test(text)) {
      return 'drm';
    }
    return 'network';
  }

  async function ensureConfigured(developerToken, musicUserToken) {
    if (!window.MusicKit) {
      // The DRM-capable library never loaded; playback is impossible.
      throw new Error('MusicKit JS unavailable (DRM)');
    }
    if (!music) {
      music = await window.MusicKit.configure({
        developerToken: developerToken || '',
        app: { name: 'apple-music-tui', build: '0.1.0' },
      });
      music.addEventListener('playbackStateDidChange', onPlaybackStateChange);
      music.addEventListener('playbackTimeDidChange', onPlaybackTimeChange);
      music.addEventListener('mediaPlaybackError', (e) => {
        emit({ type: 'error', trackId: currentTrackId, kind: classifyError(e && e.error ? e.error : e) });
      });
    }
    // Authorize with the captured Music_User_Token so full-length DRM streams
    // (not 30s previews) are served.
    if (musicUserToken) {
      music.musicUserToken = musicUserToken;
    }
  }

  async function handleLoad(command) {
    currentTrackId = command.trackId;
    try {
      await ensureConfigured(command.developerToken, command.musicUserToken);
      await music.setQueue({ song: command.trackId, startTime: (command.positionMs || 0) / 1000 });
      // setQueue drives the DRM license request on the first play; report ready
      // so the main process can transition out of Loading.
      emit({ type: 'ready', trackId: currentTrackId, durationMs: durationMs() });
    } catch (err) {
      // A missing/unavailable catalog item surfaces as a specific failure; Apple
      // returns a 404-ish content error for storefront-unavailable tracks.
      const text = String((err && (err.message || err.name)) || err || '').toLowerCase();
      if (/unavailable|not.?found|404|no longer available/.test(text)) {
        emit({ type: 'unavailable', trackId: currentTrackId });
      } else {
        emit({ type: 'error', trackId: currentTrackId, kind: classifyError(err) });
      }
    }
  }

  async function handlePlay() {
    if (!music) return;
    try {
      await music.play();
    } catch (err) {
      emit({ type: 'error', trackId: currentTrackId, kind: classifyError(err) });
    }
  }

  async function handlePause() {
    if (!music) return;
    try {
      await music.pause();
    } catch (_err) {
      // Pausing failing is non-fatal; the main-process timer will reconcile.
    }
  }

  async function handleSeek(command) {
    if (!music) return;
    try {
      await music.seekToTime((command.positionMs || 0) / 1000);
      emit({ type: 'timeupdate', trackId: currentTrackId, positionMs: positionMs(), durationMs: durationMs() });
    } catch (err) {
      emit({ type: 'error', trackId: currentTrackId, kind: classifyError(err) });
    }
  }

  function handleSetVolume(command) {
    if (!music) return;
    // MusicKit volume is 0..1; the main process passes an already-clamped 0..100.
    music.volume = Math.max(0, Math.min(100, Number(command.level) || 0)) / 100;
  }

  async function handleTeardown() {
    if (!music) return;
    try {
      await music.stop();
    } catch (_err) {
      // ignore — the window is going away
    }
  }

  bridge.onCommand((command) => {
    switch (command && command.type) {
      case 'load':
        void handleLoad(command);
        break;
      case 'play':
        void handlePlay();
        break;
      case 'pause':
        void handlePause();
        break;
      case 'seek':
        void handleSeek(command);
        break;
      case 'setVolume':
        handleSetVolume(command);
        break;
      case 'teardown':
        void handleTeardown();
        break;
      default:
        break;
    }
  });
})();
