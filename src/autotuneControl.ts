export type ControlRole = "retune" | "flex" | "vibrato" | "humanize";
export type DiscreteRole = "key" | "scale";
// Continuous roles carry normalized 0..1 numbers; discrete roles carry a label
// declared by the profile's option table (e.g. "F#", "Minor"), never a number.
export type ApplyValues = { [R in ControlRole]?: number } & { [R in DiscreteRole]?: string };
export type ProfileOption = { label: string; normalized: number };
export type ProfileOptions = {
  ok: true;
  source: "profile";
  profile_id: string;
  role: DiscreteRole;
  id: number;
  options: ProfileOption[];
};
export type ControlState = {
  phase: "disconnected" | "awaiting" | "ready" | "error";
  connectionId: string | null;
  target: { pid: number; processName: string; pluginName: string; profileId: string } | null;
  capabilities: string[];
  instanceCount: number;
  delivery: { stage: "cached" | "submitted"; sequence: number } | null;
  error: string | null;
  audioVerified: false;
};
export type Candidate = {
  candidateId: string; pid: number; processName: string; pluginName: string;
  profileId: string | null; compatible: boolean; reason: string | null;
};
export type BridgeRequest = { op: "status" | "scan" | "disconnect" }
  | { op: "connect"; candidateId: string }
  | { op: "options"; candidateId: string; role: DiscreteRole }
  | { op: "clear"; connectionId: string }
  | { op: "apply"; connectionId: string; sequence: number; values: ApplyValues };
export type BridgeResponse = {
  ok: boolean;
  state: ControlState;
  error?: string;
  candidates?: Candidate[];
  skippedCount?: number;
  options?: ProfileOptions;
  cleared?: boolean;
  dspReset?: false;
};
export const initialControlState: ControlState = {
  phase: "disconnected", connectionId: null, target: null, capabilities: [], instanceCount: 0,
  delivery: null, error: null, audioVerified: false,
};
// This is a debug display contract, not Kaka's strength lookup or a millisecond scale.
// Display conversion evidence: docs/2026-09-27_autotune-control-report.md, section 4.
export function normalizeControlValue(role: ControlRole, value: number, profileId: string): number {
  if (!Number.isFinite(value)) throw new Error("参数必须是有限数值");
  if (profileId !== "autotune-pro-38c42d0b-x64") throw new Error("此版本尚未定义界面数值换算");
  const min = role === "vibrato" ? -12 : 0;
  const max = role === "vibrato" ? 12 : 100;
  if (value < min || value > max) throw new Error("参数超出允许范围");
  return role === "vibrato" ? (value + 12) / 24 : value / 100;
}

// Separate input contract: the retained page's integer speed, not a percent
// or a verified plugin display unit. This independently written expression
// reproduces all 401 saved table values; no commercial table is shipped.
// Scope/evidence: docs/2026-10-01_reverse-kaka-retune-table-report.md.
export function normalizeKakaRetuneSpeed(value: number, profileId: string): number {
  if (profileId !== "autotune-pro-38c42d0b-x64") throw new Error("此版本尚未定义 Retune 速度换算");
  if (!Number.isInteger(value) || value < 0 || value > 400) throw new Error("Retune 速度输入必须是 0～400 的整数");
  return Number(Math.fround(1 - Math.log1p(0.12 * value) / Math.log(49)).toFixed(6));
}

export function controlLabel(state: ControlState): string {
  if (state.phase === "error") return "连接中断";
  if (state.phase === "awaiting") return "等待插件处理音频";
  if (state.phase !== "ready") return "未连接 Auto-Tune";
  if (state.delivery?.stage === "submitted") return "参数已提交 · 效果待验收";
  if (state.delivery?.stage === "cached") return "参数待插件接收";
  return "已连接 · 调试模式";
}

function putValue(values: ApplyValues, role: ControlRole | DiscreteRole, value: number | string) {
  if (typeof value === "number") values[role as ControlRole] = value;
  else values[role as DiscreteRole] = value;
}

export class AutoTuneControl {
  private state = initialControlState;
  private listeners = new Set<() => void>();
  private tail: Promise<unknown> = Promise.resolve();
  private sequence = Date.now() * 1000;
  private epoch = 0;
  private candidateId: string | null = null;
  private chromatic: { connectionId: string; supported: Promise<boolean> } | null = null;
  private batch: { connectionId: string; epoch: number; values: ApplyValues; done: Promise<void> } | null = null;
  constructor(private invoke: (request: BridgeRequest) => Promise<BridgeResponse>) {}
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private publish(state: ControlState) {
    this.state = state;
    this.listeners.forEach(listener => listener());
  }
  private enqueue<T>(action: () => Promise<T>): Promise<T> {
    const result = this.tail.then(action);
    this.tail = result.catch(() => {});
    return result;
  }
  private async call(request: BridgeRequest, epoch = this.epoch) {
    let response: BridgeResponse;
    try {
      response = await this.invoke(request);
    } catch (error) {
      if (epoch === this.epoch) this.publish({ ...initialControlState, phase: "error", error: error instanceof Error ? error.message : "Auto-Tune 连接失败" });
      throw error;
    }
    // A rejected old-window request can carry the valid replacement session.
    if (epoch === this.epoch) this.publish(response.state);
    if (!response.ok) throw new Error(response.error || response.state.error || "Auto-Tune 控制失败");
    return response;
  }
  refresh = () => {
    const epoch = this.epoch;
    return this.enqueue(() => this.call({ op: "status" }, epoch));
  };
  scan = () => {
    const epoch = this.epoch;
    return this.enqueue(() => this.call({ op: "scan" }, epoch));
  };
  connect = (candidateId: string) => {
    const epoch = ++this.epoch;
    this.candidateId = candidateId;
    this.publish(initialControlState);
    return this.enqueue(() => this.call({ op: "connect", candidateId }, epoch));
  };
  disconnect = () => {
    const epoch = ++this.epoch;
    this.candidateId = null;
    this.publish(initialControlState);
    return this.enqueue(() => this.call({ op: "disconnect" }, epoch));
  };
  clear = () => {
    const current = this.state;
    if (current.phase !== "ready" || !current.connectionId) return Promise.resolve();
    const epoch = this.epoch;
    const connectionId = current.connectionId;
    // Finish the pending edit batch before clear. Later edits must form a new
    // batch behind it, so they cannot be published early and then erased.
    this.batch = null;
    return this.enqueue(async () => {
      if (epoch !== this.epoch || this.state.phase !== "ready" || this.state.connectionId !== connectionId) return;
      await this.call({ op: "clear", connectionId }, epoch);
    });
  };
  getOptions = async (candidateId: string, role: DiscreteRole) => {
    const epoch = this.epoch;
    const response = await this.enqueue(() => this.call({ op: "options", candidateId, role }, epoch));
    if (!response.options) throw new Error("未返回配置选项");
    return response.options;
  };
  /** Whether the connected plugin's Scale list has Chromatic; asked once per connection,
   * and any failure (or an unknown candidate) counts as not supported. */
  supportsChromatic(): Promise<boolean> {
    const { connectionId } = this.state;
    if (!connectionId || !this.candidateId) return Promise.resolve(false);
    if (this.chromatic?.connectionId !== connectionId) {
      this.chromatic = { connectionId, supported: this.getOptions(this.candidateId, "scale")
        .then(options => options.options.some(option => option.label === "Chromatic"), () => false) };
    }
    return this.chromatic.supported;
  }
  setValue(role: ControlRole, value: number) {
    return this.setConvertedValue(role, value, normalizeControlValue);
  }
  // Explicit opt-in API for the verified page-input curve. The product slider
  // still uses setValue's percentage contract until its input UI is migrated.
  setRetuneSpeed(value: number) {
    return this.setConvertedValue("retune", value, (_role, input, profileId) => normalizeKakaRetuneSpeed(input, profileId));
  }
  private async setConvertedValue(role: ControlRole, value: number,
    convert: (role: ControlRole, value: number, profileId: string) => number) {
    const current = this.state;
    if (current.phase !== "ready" || !current.connectionId || !current.target) return;
    if (!current.capabilities.includes(role)) throw new Error("当前版本不支持此参数");
    return this.queue(role, convert(role, value, current.target.profileId));
  }
  // Key / Scale take a label from the profile option table (see getOptions).
  // The bridge resolves the label; an unknown label fails closed there.
  setDiscrete(role: DiscreteRole, label: string) {
    const current = this.state;
    if (current.phase !== "ready" || !current.connectionId || !current.target) return Promise.resolve();
    if (!current.capabilities.includes(role)) throw new Error("当前版本不支持自动写入 Key/Scale");
    if (typeof label !== "string" || label.length === 0 || label.length > 32) throw new Error("无效的选项标签");
    return this.queue(role, label);
  }
  private queue(role: ControlRole | DiscreteRole, value: number | string): Promise<void> {
    const current = this.state;
    if (current.phase !== "ready" || !current.connectionId) return Promise.resolve();
    // Coalesce only unsent UI edits. The native cache/serial contract is unchanged.
    // There is at most one in-flight write and one batch waiting behind it.
    const epoch = this.epoch;
    if (this.batch?.connectionId === current.connectionId && this.batch.epoch === epoch) {
      putValue(this.batch.values, role, value);
      return this.batch.done;
    }
    const values: ApplyValues = {};
    putValue(values, role, value);
    const batch = { connectionId: current.connectionId, epoch, values, done: Promise.resolve() };
    this.batch = batch;
    batch.done = this.enqueue(async () => {
      if (this.batch === batch) this.batch = null;
      if (epoch !== this.epoch || this.state.phase !== "ready" || this.state.connectionId !== current.connectionId) return;
      await this.call({ op: "apply", connectionId: current.connectionId!, sequence: ++this.sequence, values: batch.values }, epoch);
    });
    return batch.done;
  }
}
