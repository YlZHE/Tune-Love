// Evaluated inside the debug WebView; never bundled into the application.
export function installTransportGuard(sourceId) {
  if (window.__transportVerification || window.__playerControlsVerification) throw new Error("Another native verifier is active");
  const webview = window.chrome?.webview;
  if (!webview?.postMessage) throw new Error("Expected Windows WebView transport");
  const originalFetch = window.fetch;
  const originalPost = webview.postMessage;
  const state = { sourceId, expected: null, records: [], blocked: [] };
  const readOnly = new Set(["get_now_playing", "get_audio_level", "get_key_detection", "plugin:window|is_always_on_top",
    "plugin:window|is_visible", "plugin:window|inner_size", "plugin:window|scale_factor", "plugin:window|get_all_windows"]);
  const blockedResponse = command => {
    state.blocked.push(command);
    return new Response(JSON.stringify({ code: "verification_scope" }), {
      headers: { "Content-Type": "application/json", "Tauri-Response": "error" },
    });
  };
  const fetchWrapper = async function(input, init) {
    const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url, location.href);
    if (url.hostname !== "ipc.localhost" && url.protocol !== "ipc:") return originalFetch.call(window, input, init);
    const command = decodeURIComponent(url.pathname.slice(1));
    if (readOnly.has(command)) return originalFetch.call(window, input, init);
    if (command !== "control_media") return blockedResponse(command);
    let request;
    try { request = JSON.parse(init?.body).request; } catch { return blockedResponse("unreadable control request"); }
    const expected = state.expected;
    if (!expected || request?.sourceId !== sourceId || !["action", "sessionId", "sourceId", "trackKey", "targetGeneration"]
      .every(field => request[field] === expected[field])) return blockedResponse("out-of-scope control request");
    state.expected = null; // One click authorizes exactly one native operation.
    const record = { request, outcome: "pending" };
    state.records.push(record);
    try {
      const response = await originalFetch.call(window, input, init);
      record.response = await response.clone().json();
      record.outcome = response.headers.get("Tauri-Response") === "ok" && response.ok ? "ok" : "error";
      return response; // Record, do not replace, the actual native result.
    } catch (error) { record.outcome = "network_error"; throw error; }
  };
  const postWrapper = function(message) {
    const parsed = typeof message === "string" ? JSON.parse(message) : message;
    if (readOnly.has(parsed?.cmd)) return originalPost.call(webview, message);
    // This fallback has no observable response here. Refuse writes rather than
    // claim verification without observing the real success/error result.
    state.blocked.push(`unobservable or unexpected postMessage: ${parsed?.cmd}`);
    queueMicrotask(() => window.__TAURI_INTERNALS__.runCallback(parsed.error, { code: "verification_scope" }));
  };
  state.restore = () => {
    const intact = window.fetch === fetchWrapper && webview.postMessage === postWrapper;
    if (window.fetch === fetchWrapper) window.fetch = originalFetch;
    if (webview.postMessage === postWrapper) webview.postMessage = originalPost;
    delete window.__transportVerification;
    return { records: state.records, blocked: state.blocked, intact,
      restored: window.fetch === originalFetch && webview.postMessage === originalPost };
  };
  window.fetch = fetchWrapper; webview.postMessage = postWrapper;
  if (window.fetch !== fetchWrapper || webview.postMessage !== postWrapper) {
    state.restore(); throw new Error("Could not install native transport guard");
  }
  window.__transportVerification = state;
}
