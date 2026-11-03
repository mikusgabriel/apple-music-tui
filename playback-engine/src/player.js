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
  /**
   * Resolves when the most recent `load` (catalog-id resolution + setQueue) has
   * finished. The TUI sends `load` and `play` as separate messages; `load` is
   * async (an API lookup + setQueue), so a `play` that arrives first would run
   * against the OLD queue and the new track would stay paused. `handlePlay`
   * awaits this barrier so play always applies to the freshly loaded track.
   */
  let pendingLoad = Promise.resolve();

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

  // Resolve a library song id (`i.*`) to its catalog song id using the Apple
  // Music API. The library item's `playParams.catalogId` is the catalog track
  // that plays reliably via `setQueue({ song })`. Returns null when the lookup
  // fails or the song has no catalog equivalent (e.g. a user upload), in which
  // case the caller falls back to the library-songs descriptor.
  async function resolveCatalogId(libraryId, developerToken, musicUserToken) {
    try {
      const res = await fetch(
        'https://api.music.apple.com/v1/me/library/songs/' + encodeURIComponent(libraryId),
        {
          headers: {
            Authorization: 'Bearer ' + (developerToken || ''),
            'Music-User-Token': musicUserToken || '',
          },
        }
      );
      if (!res.ok) {
        console.log('[player] resolveCatalogId HTTP ' + res.status);
        return null;
      }
      const body = await res.json();
      const item = body && body.data && body.data[0];
      const catalogId = item && item.attributes && item.attributes.playParams && item.attributes.playParams.catalogId;
      return typeof catalogId === 'string' && catalogId.length > 0 ? catalogId : null;
    } catch (err) {
      console.log('[player] resolveCatalogId error: ' + (err && err.message ? err.message : err));
      return null;
    }
  }

  async function handleLoad(command) {
    currentTrackId = command.trackId;
    try {
      console.log('[player] load trackId=' + command.trackId + ' hasDevToken=' + !!command.developerToken + ' hasMut=' + !!command.musicUserToken);
      await ensureConfigured(command.developerToken, command.musicUserToken);
      console.log('[player] configured; isAuthorized=' + (music && music.isAuthorized) + ' storefront=' + (music && music.storefrontCountryCode));

      const startTime = (command.positionMs || 0) / 1000;
      const isLibraryId = /^i\./.test(command.trackId);

      // Build an ordered list of queue strategies to try. Library ids can be
      // played either as a library-songs queue or, when the item has a catalog
      // equivalent available in the user's storefront, as a catalog `song`.
      // Neither works for every track (library-songs can report "No playable
      // items", a catalog id can be "NOT_FOUND" in the storefront), so we try
      // them in order and keep the first that yields a non-empty queue. This is
      // what makes playback robust across the occasional problem track.
      const strategies = [];
      if (isLibraryId) {
        strategies.push({ name: 'library-songs', descriptor: { 'library-songs': [command.trackId], startTime } });
        const catalogId = await resolveCatalogId(command.trackId, command.developerToken, command.musicUserToken);
        if (catalogId) {
          strategies.push({ name: 'song(catalog)', descriptor: { song: catalogId, startTime } });
        }
      } else {
        strategies.push({ name: 'song', descriptor: { song: command.trackId, startTime } });
      }

      let queued = false;
      let lastErr = null;
      for (const strategy of strategies) {
        try {
          console.log('[player] trying setQueue strategy=' + strategy.name);
          await music.setQueue(strategy.descriptor);
          const len = (music.queue && music.queue.length) || 0;
          if (len > 0) {
            console.log('[player] setQueue OK via ' + strategy.name + '; duration=' + durationMs() + ' queueLen=' + len);
            queued = true;
            break;
          }
          console.log('[player] setQueue via ' + strategy.name + ' produced empty queue; trying next');
        } catch (err) {
          lastErr = err;
          console.log('[player] setQueue strategy ' + strategy.name + ' failed: ' + (err && err.message ? err.message : String(err)));
        }
      }

      if (!queued) {
        // No strategy produced a playable queue: surface as unavailable so the
        // UI can skip rather than appearing to play silence.
        const text = String((lastErr && (lastErr.message || lastErr.name)) || lastErr || '').toLowerCase();
        if (/unavailable|not.?found|404|no longer available|no playable/.test(text) || lastErr === null) {
          emit({ type: 'unavailable', trackId: currentTrackId });
        } else {
          emit({ type: 'error', trackId: currentTrackId, kind: classifyError(lastErr) });
        }
        return;
      }

      // setQueue drives the DRM license request on the first play; report ready
      // so the main process can transition out of Loading.
      emit({ type: 'ready', trackId: currentTrackId, durationMs: durationMs() });
    } catch (err) {
      console.log('[player] load ERROR: name=' + (err && err.name) + ' msg=' + (err && err.message) + ' str=' + String(err));
      const text = String((err && (err.message || err.name)) || err || '').toLowerCase();
      if (/unavailable|not.?found|404|no longer available|no playable/.test(text)) {
        emit({ type: 'unavailable', trackId: currentTrackId });
      } else {
        emit({ type: 'error', trackId: currentTrackId, kind: classifyError(err) });
      }
    }
  }

  async function handlePlay() {
    // Wait for any in-flight load (catalog-id resolution + setQueue) so play
    // applies to the newly loaded track rather than racing the old queue.
    try {
      await pendingLoad;
    } catch (_err) {
      // handleLoad already surfaced its own error; proceed to attempt play.
    }
    if (!music) return;
    try {
      // MusicKit rejects play() if the player is already playing ("called
      // without a previous stop() or pause()"). Treat an already-playing state
      // as success rather than an error.
      const states = window.MusicKit && window.MusicKit.PlaybackStates;
      const playingState = states ? states.playing : 2;
      if (music.playbackState === playingState) {
        console.log('[player] play() skipped; already playing');
        emit({ type: 'playing', trackId: currentTrackId, positionMs: positionMs(), durationMs: durationMs() });
        return;
      }
      console.log('[player] play()');
      await music.play();
      console.log('[player] play() resolved; state=' + (music.playbackState) + ' pos=' + positionMs());
    } catch (err) {
      console.log('[player] play ERROR: ' + (err && err.message) + ' / ' + String(err));
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
        // Track the in-flight load so a subsequent `play` waits for it. Errors
        // are handled inside handleLoad; swallow here so the barrier never
        // rejects (a rejected barrier would block the next play).
        pendingLoad = handleLoad(command).catch(() => {});
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
