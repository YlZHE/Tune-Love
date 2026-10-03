import { describe, expect, it } from "vitest";
import { AutoTuneControl, initialControlState, normalizeControlValue, normalizeKakaRetuneSpeed, type ControlState, type BridgeRequest } from "./autotuneControl";

const ready = (id = "session-a"): ControlState => ({
  ...initialControlState, phase: "ready", connectionId: id,
  target: { pid: 123, processName: "fixture.exe", pluginName: "Auto-Tune Pro", profileId: "autotune-pro-38c42d0b-x64" },
  capabilities: ["retune", "flex", "vibrato", "humanize"], instanceCount: 1,
});

describe("application control session", () => {
  it("preserves edit-clear-edit order instead of merging across the clear", async () => {
    const sent: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => {
      sent.push(structuredClone(request));
      return { ok: true, state: ready() };
    });
    await control.refresh();
    sent.length = 0;
    const first = control.setValue("flex", 20);
    const clear = control.clear();
    const after = control.setValue("flex", 80);
    await Promise.all([first, clear, after]);
    expect(sent.map(r => r.op)).toEqual(["apply", "clear", "apply"]);
    expect(sent[0]).toMatchObject({ values: { flex: .2 } });
    expect(sent[2]).toMatchObject({ values: { flex: .8 } });
  });

  it("a queued options response cannot restore state during a new connect", async () => {
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    const seen: Array<string | null> = [];
    const control = new AutoTuneControl(async request => {
      if (request.op === "status") await pending;
      if (request.op === "connect") return { ok: true, state: ready("new") };
      return { ok: true, state: ready("old"), options: {
        ok: true, source: "profile", profile_id: "fixture", role: "key", id: 2,
        options: [{ label: "C", normalized: 0 }],
      } };
    });
    control.subscribe(() => { seen.push(control.getSnapshot().connectionId); });
    const refresh = control.refresh();
    const options = control.getOptions("candidate", "key");
    const connect = control.connect("replacement");
    release();
    await Promise.all([refresh, options, connect]);
    expect(seen).toEqual([null, "new"]);
  });

  it("never publishes defaults on connect, or preview changes while disconnected", async () => {
    const sent: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => { sent.push(request); return { ok: true, state: ready() }; });
    await control.setValue("flex", 25);
    expect(sent).toEqual([]);
    await control.connect("candidate");
    expect(sent).toEqual([{ op: "connect", candidateId: "candidate" }]);
    await control.setValue("vibrato", -6);
    expect(sent[1]).toMatchObject({ op: "apply", connectionId: "session-a", values: { vibrato: 0.25 } });
    expect(Object.keys((sent[1] as { values: object }).values)).toEqual(["vibrato"]);
  });

  it("invalidates queued writes after a different connection is observed", async () => {
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    const sent: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => {
      sent.push(request);
      if (request.op === "apply") { await pending; return { ok: true, state: ready("session-b") }; }
      return { ok: true, state: ready() };
    });
    await control.refresh();
    const first = control.setValue("flex", 20);
    await Promise.resolve();
    const second = control.setValue("humanize", 30);
    release();
    await Promise.all([first, second]);
    expect(sent.filter(r => r.op === "apply")).toHaveLength(1);
    expect(control.getSnapshot().connectionId).toBe("session-b");
  });

  it("does not retry an ambiguous failure or keep showing a working connection", async () => {
    let count = 0;
    const control = new AutoTuneControl(async request => {
      if (request.op === "apply") { count++; throw new Error("pipe closed"); }
      return { ok: true, state: ready() };
    });
    await control.refresh();
    await expect(control.setValue("retune", 50)).rejects.toThrow("pipe closed");
    await control.setValue("flex", 20);
    expect(count).toBe(1);
    expect(control.getSnapshot().phase).toBe("error");
    expect(control.getSnapshot().connectionId).toBeNull();
  });

  it("uses distinct increasing sequences even when values are repeated", async () => {
    const writes: number[] = [];
    const control = new AutoTuneControl(async request => {
      if (request.op === "apply") writes.push(request.sequence);
      return { ok: true, state: ready() };
    });
    await control.refresh();
    await control.setValue("flex", 25);
    await control.setValue("flex", 25);
    expect(writes).toHaveLength(2);
    expect(writes[1]).toBeGreaterThan(writes[0]);
  });

  it("batches key and scale labels with continuous values into one apply", async () => {
    const sent: BridgeRequest[] = [];
    const state = { ...ready(), capabilities: ["retune", "flex", "vibrato", "humanize", "key", "scale"] };
    const control = new AutoTuneControl(async request => { sent.push(structuredClone(request)); return { ok: true, state }; });
    await control.refresh();
    sent.length = 0;
    const a = control.setValue("flex", 20);
    const b = control.setDiscrete("key", "F#");
    const c = control.setDiscrete("scale", "Minor");
    await Promise.all([a, b, c]);
    expect(sent.map(r => r.op)).toEqual(["apply"]);
    expect(sent[0]).toMatchObject({ values: { flex: .2, key: "F#", scale: "Minor" } });
  });

  it("refuses key/scale writes when the profile does not expose them and rejects bad labels", async () => {
    let writes = 0;
    const control = new AutoTuneControl(async request => { if (request.op === "apply") writes++; return { ok: true, state: ready() }; });
    await control.refresh();
    expect(() => control.setDiscrete("key", "F#")).toThrow();
    const capable = new AutoTuneControl(async request => { if (request.op === "apply") writes++; return { ok: true, state: { ...ready(), capabilities: ["key", "scale"] } }; });
    await capable.refresh();
    expect(() => capable.setDiscrete("key", "")).toThrow();
    expect(() => capable.setDiscrete("scale", "x".repeat(33))).toThrow();
    expect(writes).toBe(0);
  });

  it("rejects unsupported capabilities before writing", async () => {
    let writes = 0;
    const state = { ...ready(), capabilities: ["retune"] };
    const control = new AutoTuneControl(async request => { if (request.op === "apply") writes++; return { ok: true, state }; });
    await control.refresh();
    await expect(control.setValue("flex", 50)).rejects.toThrow();
    expect(writes).toBe(0);
  });

  it("a late success cannot restore a connection after Stop control was requested", async () => {
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    const writes: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => {
      if (request.op === "apply") { writes.push(request); await pending; }
      return { ok: true, state: request.op === "disconnect" ? initialControlState : ready() };
    });
    await control.refresh();
    const first = control.setValue("flex", 20);
    await Promise.resolve();
    const second = control.setValue("humanize", 30);
    const stopped = control.disconnect();
    release();
    await Promise.all([first, second, stopped]);
    expect(writes).toHaveLength(1);
    expect(control.getSnapshot().phase).toBe("disconnected");
  });

  it("coalesces a fast drag to its last requested value instead of dropping the endpoint", async () => {
    const writes: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => {
      if (request.op === "apply") writes.push(request);
      return { ok: true, state: ready() };
    });
    await control.refresh();
    await Promise.all(Array.from({ length: 100 }, (_, i) => control.setValue("flex", i + 1)));
    expect(writes).toHaveLength(1);
    expect(writes[0]).toMatchObject({ values: { flex: 1 } });
  });

  it("keeps the current session returned by the backend when an old-window write is rejected", async () => {
    const control = new AutoTuneControl(async request => request.op === "apply"
      ? { ok: false, error: "Connection expired", state: ready("session-b") }
      : { ok: true, state: ready() });
    await control.refresh();
    await expect(control.setValue("flex", 25)).rejects.toThrow("Connection expired");
    expect(control.getSnapshot().connectionId).toBe("session-b");
    expect(control.getSnapshot().phase).toBe("ready");
  });
});

describe("display values are not plugin milliseconds", () => {
  it.each([["retune", 75, .75], ["flex", 25, .25], ["humanize", 75, .75], ["vibrato", -6, .25], ["vibrato", 0, .5], ["vibrato", 12, 1]] as const)("converts %s %s using the identified profile", (role, value, expected) => {
    expect(normalizeControlValue(role, value, "autotune-pro-38c42d0b-x64")).toBe(expected);
  });
  it.each([NaN, Infinity, -1, 101])("rejects invalid percent %s", value => {
    expect(() => normalizeControlValue("flex", value, "autotune-pro-38c42d0b-x64")).toThrow();
  });
  it("does not infer another version's display conversion", () => {
    expect(() => normalizeControlValue("vibrato", -6, "another-version")).toThrow();
  });
});

describe("Kaka Retune speed conversion", () => {
  it.each([[0, 1], [1, 0.97088], [20, 0.685552], [50, 0.5], [59, 0.463133], [70, 0.424251], [100, 0.340938], [146, 0.250004], [353, 0.03142], [400, 0]] as const)(
    "reproduces the verified page table at %s",
    (input, expected) => expect(normalizeKakaRetuneSpeed(input, "autotune-pro-38c42d0b-x64")).toBe(expected),
  );
  it.each([NaN, Infinity, -1, 401, 1.5])("rejects a non-page speed input %s", input => {
    expect(() => normalizeKakaRetuneSpeed(input, "autotune-pro-38c42d0b-x64")).toThrow("0～400");
  });
  it("does not apply the page curve to an unverified profile", () => {
    expect(() => normalizeKakaRetuneSpeed(50, "another-version")).toThrow("此版本");
  });
  it("sends the explicit speed domain through the existing edit queue without publishing other defaults", async () => {
    const requests: BridgeRequest[] = [];
    const control = new AutoTuneControl(async request => {
      requests.push(structuredClone(request));
      return { ok: true, state: ready() };
    });
    await control.setRetuneSpeed(20);
    expect(requests).toEqual([]);
    await control.connect("candidate");
    expect(requests.map(r => r.op)).toEqual(["connect"]);
    await Promise.all([control.setRetuneSpeed(146), control.setRetuneSpeed(20), control.setValue("flex", 25)]);
    expect(requests).toHaveLength(2);
    expect(requests[1]).toMatchObject({ op: "apply", connectionId: "session-a", values: { retune: 0.685552, flex: 0.25 } });
    await control.clear();
    await control.setRetuneSpeed(400);
    expect(requests.map(r => r.op)).toEqual(["connect", "apply", "clear", "apply"]);
    expect(requests[3]).toMatchObject({ values: { retune: 0 } });
    // The old percentage contract remains a separate, explicit input domain.
    await control.setValue("retune", 75);
    expect(requests[4]).toMatchObject({ values: { retune: 0.75 } });
  });
  it("rejects invalid speeds and unsupported roles before any write", async () => {
    const requests: BridgeRequest[] = [];
    let state = ready();
    const control = new AutoTuneControl(async request => { requests.push(request); return { ok: true, state }; });
    await control.refresh();
    await expect(control.setRetuneSpeed(401)).rejects.toThrow("0～400");
    state = { ...ready(), capabilities: ["flex"] };
    await control.refresh();
    await expect(control.setRetuneSpeed(20)).rejects.toThrow("不支持此参数");
    state = { ...ready(), target: { ...ready().target!, profileId: "another-version" } };
    await control.refresh();
    await expect(control.setRetuneSpeed(20)).rejects.toThrow("此版本");
    expect(requests.every(r => r.op === "status")).toBe(true);
  });
});
