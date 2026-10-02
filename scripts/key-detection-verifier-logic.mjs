const PITCH_NAMES = ["C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B"];

export function createLiveEvidence(initial) {
  return {
    initialStatus: initial.detectionStatus,
    initialUpdatedAtMs: initial.detectionUpdatedAtMs,
    initialGeneration: initial.detectionGeneration,
    initialMediaGeneration: initial.mediaGeneration,
    initialPitchClass: initial.pitchClass,
    initialMode: initial.mode,
  };
}

function validDetectedKey(observation) {
  return observation.detectionStatus === "detected"
    && Number.isInteger(observation.pitchClass) && observation.pitchClass >= 0 && observation.pitchClass < 12
    && (observation.mode === "major" || observation.mode === "minor");
}

function liveAudio(observation) {
  return observation.audioStatus === "capturing"
    && observation.audioSourceMatches === true && observation.audioTrackMatches === true
    && Number.isFinite(observation.audioAgeMs) && observation.audioAgeMs >= -50 && observation.audioAgeMs <= 400
    && Number.isFinite(observation.audioRms) && observation.audioRms > 0
    && Number.isFinite(observation.audioLevel) && observation.audioLevel > 0;
}

export function observeLiveEvidence(evidence, observation) {
  const progressed = observation.detectionGeneration === evidence.initialGeneration
    && observation.mediaGeneration === evidence.initialMediaGeneration
    && ((evidence.initialStatus !== "detected" && observation.detectionStatus === "detected")
      || (Number.isFinite(evidence.initialUpdatedAtMs) && Number.isFinite(observation.detectionUpdatedAtMs)
        && observation.detectionUpdatedAtMs > evidence.initialUpdatedAtMs));
  const expectedLabel = validDetectedKey(observation)
    ? `${PITCH_NAMES[observation.pitchClass]} ${observation.mode === "major" ? "大调" : "小调"}` : null;
  const accepted = observation.mediaStatus === "ready"
    && observation.playbackStatus === "playing"
    && Number.isFinite(observation.mediaAgeMs) && observation.mediaAgeMs >= -50 && observation.mediaAgeMs <= 8_000
    && observation.matchedIdentity === true
    && liveAudio(observation)
    && observation.presentKeyCount === 1
    && validDetectedKey(observation)
    && observation.displayedLabel === expectedLabel
    && progressed;
  return { accepted, progressed, expectedLabel };
}

export async function installKeyDetectionGuard(sourceId) {
  if (window.__keyDetectionVerification || window.__transportVerification || window.__playerControlsVerification)
    throw new Error("Another native verifier is active");
  const webview = window.chrome?.webview;
  if (!webview?.postMessage) throw new Error("Expected Windows WebView transport");
  const originalFetch = window.fetch;
  const originalPost = webview.postMessage;
  const token = `key-detection-verifier:${Date.now()}:${Math.random()}`;
  const readOnly = new Set(["get_now_playing", "get_audio_level", "get_key_detection",
    "plugin:window|is_always_on_top", "plugin:window|is_visible", "plugin:window|inner_size",
    "plugin:window|scale_factor", "plugin:window|get_all_windows"]);
  const state = { token, sourceId, originalFetch, originalPost, fetchWrapper: null, postWrapper: null,
    commands: {}, blocked: [] };
  const count = command => { state.commands[command] = (state.commands[command] ?? 0) + 1; };
  const block = command => {
    state.blocked.push(command);
    return new Response(JSON.stringify({ code: "verification_scope" }), {
      headers: { "Content-Type": "application/json", "Tauri-Response": "error" },
    });
  };
  state.fetchWrapper = async function(input, init) {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url, location.href);
    if (url.hostname !== "ipc.localhost" && url.protocol !== "ipc:") return originalFetch.call(window, input, init);
    const command = decodeURIComponent(url.pathname.slice(1)); count(command);
    if (readOnly.has(command)) return originalFetch.call(window, input, init);
    return block(command);
  };
  state.postWrapper = function(message) {
    let parsed;
    try { parsed = typeof message === "string" ? JSON.parse(message) : message; }
    catch { state.blocked.push("unreadable postMessage"); return; }
    count(parsed?.cmd ?? "unknown");
    if (readOnly.has(parsed?.cmd)) return originalPost.call(webview, message);
    state.blocked.push(parsed?.cmd ?? "unknown postMessage");
    queueMicrotask(() => window.__TAURI_INTERNALS__.runCallback(parsed?.error, { code: "verification_scope" }));
  };
  window.fetch = state.fetchWrapper;
  webview.postMessage = state.postWrapper;
  if (window.fetch !== state.fetchWrapper || webview.postMessage !== state.postWrapper) {
    if (window.fetch === state.fetchWrapper) window.fetch = originalFetch;
    if (webview.postMessage === state.postWrapper) webview.postMessage = originalPost;
    throw new Error("Could not install key-detection IPC guard");
  }
  window.__keyDetectionVerification = state;
  return token;
}

export async function restoreKeyDetectionGuard(expectedToken) {
  const state = window.__keyDetectionVerification;
  if (!state || state.token !== expectedToken) return { owned: false, restored: false };
  const webview = window.chrome?.webview;
  const intact = window.fetch === state.fetchWrapper && webview?.postMessage === state.postWrapper;
  if (window.fetch === state.fetchWrapper) window.fetch = state.originalFetch;
  if (webview?.postMessage === state.postWrapper) webview.postMessage = state.originalPost;
  const restored = window.fetch === state.originalFetch && webview?.postMessage === state.originalPost;
  delete window.__keyDetectionVerification;
  return { owned: true, intact, restored, commands: state.commands, blocked: state.blocked };
}
