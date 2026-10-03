# 拿不准时用 Chromatic、标题只显示 Key / Scale：实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在音集匹配器中加入“盖不住的比例”门控，拿不准和开头时自动用 Chromatic；结果连同候选与原因一起写入、缓存，并作为唯一的标题显示。

**Architecture:** 先离线定出门控参数，并在新歌上检验（Task 1）；用户看过报告后，再改 Rust 匹配器（Task 2）、缓存（Task 3）、前端写入（Task 4）和界面（Task 5）；最后同步文档（Task 6）。
- 匹配器内部仍在 12 组 7 音集合中按现有迟滞规则选择，另有一个独立的 Chromatic 门控。
- 叫法（大调名或关系小调名）在输出时按 libKeyFinder 的稳定结果决定。

**Tech Stack:** Rust（`src-tauri/src/key_detection`）、TypeScript/React（`src/`）、Playwright（`tests/`）、Python 离线工具链（`experiments/scale-match/`）。

**Spec:** `docs/superpowers/specs/2026-10-04-chromatic-uncertainty-design.md`

## Global Constraints

- 只在 Major / Minor / Chromatic 中选择；不加入 Dorian 等调式名、其他音组、新证据前端或按片段判断（spec“不在范围内”）。
- 证据只来自混音（现有 `ChromaEvidence`）；不使用用户演唱。
- 选项标签与插件 profile 一致：Key 用 `C, C#, D, D#, E, F, F#, G, G#, A, A#, B`，Scale 用 `Major`、`Minor`、`Chromatic`。不猜参数。
- 门控参数 `ENTER > EXIT`。时间一律按**有效证据秒数**（`ScaleMatcher.seconds`）计，离线与 Rust 必须用同一定义。
- 现有规则不变：每个（连接、曲目、组合）只写一次；失败不自动重试；切歌重置；`SEED_HOLD_SECONDS = 20`；`MIN_CACHE_EVIDENCE_SECONDS = 30`。
- 标题文字：`<Key> <Scale>`，例如 `F Minor`、`G# Major`、`F Chromatic`。没有候选时显示 `Chromatic`；没有曲目或尚未开始分析时显示 `Tune Love`。
- 提交身份 `YlZHE <59366419+YlZHE@users.noreply.github.com>`，提交信息末尾加 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。不推送。
- 音频、权重、`evidence/` 不入库。离线结果写入 `E:\autotune helper\work\scale-match-20261002\`。

## Review Focus

1. **开头的写入。** 新歌开始、开关打开、尚无证据时，只写 `Scale = Chromatic`，不改 Key。Task 4 测试：首次写入只包含 scale。
2. **重播一首曾拿不准的歌。** 开头就是 Chromatic，且在 `SEED_HOLD_SECONDS` 前不会退出。Task 3 测试。
3. **证据在两条线之间来回波动。** 不应来回切换。Task 2 测试：在 EXIT 与 ENTER 之间振荡时保持不变。
4. **插件 profile 没有 `Chromatic` 选项。** 降级为写候选的 Major/Minor；没有候选时不写；提示说明原因。Task 4 测试。
5. **改名但音组不变。** 例如 libKeyFinder 稳定结果从“无”变为 F 小调，`G# Major` 改为 `F Minor`。会重写一次，但不会每秒抖动。Task 2 测试：同一稳定结果重复输入时，target 不变。

---

### Task 1: 离线定门控参数，并在新歌上检验

**Files:**
- Create: `experiments/scale-match/chromatic_gate.py`（门控状态机，与 Rust 同定义）
- Create: `experiments/scale-match/run_chromatic.py`（开发集网格 + 检验集评估 + 报告 JSON）
- Test: `experiments/scale-match/tests/test_chromatic_gate.py`
- Modify: `E:\autotune helper\work\scale-match-20261002\preregistration.md`（追加 chromatic-gate-01 节，必须在看任何新歌结果之前写）
- Create: `E:\autotune helper\docs\2026-10-0X_chromatic-gate-report.md`（X 为实际日期）

**Interfaces:**
- Consumes: `replay_production.probe_rows`、`replay_production.production_combos` 的证据累积方式；`matcher.MASKS`、`matcher.Tracker`；`metrics.success_table(..., chromatic_corrects=False)`、`metrics.summarize`；`run_pilot.vocal_truth / evaluate / harmony_at`；`run_holdout.first_vocal_line`。
- Produces（写入报告，供 Task 2 原样使用）：`ENTER`、`EXIT`、`EXIT_HOLD_SECONDS`、`MIN_SECONDS` 四个冻结值。

门控定义（Rust 必须与此一致）：

```
weights = matcher 现有 (evidence + alpha/12) / (seconds + alpha)
S       = 现有迟滞规则下的当前集合（首个证据即选定）
uncovered = Σ weights[pc]，pc 不在 S 中
状态 chromatic（初始为 True）：
  chromatic 且 seconds ≥ MIN_SECONDS 且 uncovered < EXIT：hold 增加本步秒数；hold ≥ EXIT_HOLD_SECONDS 时 chromatic=False
  chromatic 且上述条件不成立：hold = 0
  非 chromatic 且 uncovered > ENTER：chromatic=True，hold=0
输出组合 = Chromatic if chromatic else S
```

- [ ] **Step 1: 写门控单元测试**（`test_chromatic_gate.py`）
  - `test_starts_chromatic_until_min_seconds`：只有 C 大调音证据，`seconds < MIN_SECONDS` 时输出 Chromatic；达到 `MIN_SECONDS + EXIT_HOLD_SECONDS` 后输出 C Major 那组。
  - `test_d_and_dflat_both_strong_enters_chromatic`：F 小调音 + D 与 D♭ 同权重，输出 Chromatic，候选集合仍报告。
  - `test_oscillating_between_lines_holds_state`：uncovered 在 EXIT 与 ENTER 之间来回，状态不变。
  - `test_exit_requires_hold`：uncovered 降到 EXIT 以下，未满 `EXIT_HOLD_SECONDS` 时仍为 Chromatic。
- [ ] **Step 2: 运行测试，确认失败**
  Run: `python -m pytest experiments/scale-match/tests/test_chromatic_gate.py -v`
  Expected: FAIL（模块不存在）
- [ ] **Step 3: 实现 `chromatic_gate.Gate(enter, exit, exit_hold, min_seconds)` 及 `.update(weights, set_index, step_seconds) -> bool`（返回是否 Chromatic）；`production_chromatic(rows, duration, gate_params) -> np.ndarray`**（逐步组合，Chromatic 用 `matcher.CHROMATIC`）
- [ ] **Step 4: 运行测试，确认通过**；同时运行 `python -m pytest experiments/scale-match/tests -q`，现有测试保持通过。
- [ ] **Step 5: 写预登记**，在 preregistration.md 追加“chromatic-gate-01”节，内容包括：
  - 开发集：以往任何一轮用过的全部歌曲；
  - 检验集：按 SHA-256 排序取前 10 首从未用过的歌；不足 10 首时用全部未用过的歌，并在报告首段说明；
  - 网格：ENTER ∈ {0.06, 0.08, 0.10, 0.12, 0.15}，EXIT ∈ {0.03, 0.05, 0.07, 0.09}（只取 EXIT < ENTER 的组合），EXIT_HOLD_SECONDS ∈ {3, 6, 10}，MIN_SECONDS ∈ {3, 6, 10}；
  - 选参规则：开发集误拉率最低；误拉率相差 ≤ 0.2 个百分点时，取修音覆盖率最高的；
  - 对照：现行 majmin（`production_combos`）；
  - 通过条件：检验集误拉率低于 majmin。
- [ ] **Step 6: 跑开发集网格**，冻结四个值，记录日志 `chromatic-gate-01-dev.log`。
- [ ] **Step 7: 跑检验集**（日志 `chromatic-gate-01.log`）。逐首统计以下各项，majmin 并列：
  - 误拉率；
  - 修音覆盖率（可评估帧中处于 Major/Minor 的比例）；
  - Chromatic 时长占比；
  - 首句综合分；
  - 切换次数；
  - 全程 Chromatic 的歌单。

  首次播放与缓存命中（`cached()` 口径，缓存值为第一次播放的最终组合，可以是 Chromatic）分别统计。
- [ ] **Step 8: 写报告**，中文，结论先行。
  - 写明冻结值、检验结果、是否通过，以及“少修换少误拉”的代价。
  - 如果曲库中有用户循环播放的那首（F 小调带多利亚 D），单列它的结果，不计入检验集。
  - 报告列入 `E:\autotune helper\docs\README.md` 索引。
- [ ] **Step 9: 提交**（只提交 `experiments/` 下的代码与测试）

```bash
git add experiments/scale-match/chromatic_gate.py experiments/scale-match/run_chromatic.py experiments/scale-match/tests/test_chromatic_gate.py
git commit -m "experiments(scale-match): Chromatic uncertainty gate calibration"
```

- [ ] **Step 10: 用户检查点。** 把报告交给用户。用户确认接受代价，并采用冻结值之后，才开始 Task 2。用户不接受时，按用户意见调整，回到 Step 5，重写预登记并换一批新歌。

### Task 2: Rust 匹配器——Chromatic 门控、候选与定名

> **2026-10-04 更新（用户选定第二轮默认点 A，Ruling 5）：** 门控统计量改用 chromatic-gate-02 的 `ratio`：集合外最大权重 ÷ 集合内最小权重（weights 同前，含先验 alpha）。冻结值：`ENTER = 3.0`、`EXIT = 1.5`、`EXIT_HOLD_SECONDS = 3`、`MIN_SECONDS = 3`。状态机与 Task 1 定义相同，只把 uncovered 换成该比值。以 `experiments/scale-match/chromatic_gate.py` 的 `gate_statistic("ratio", ...)` 为准；Rust 测试必须与其逐步输出一致。`uncovered` 字段已从 target 中删除（界面不用）。

**Files:**
- Modify: `src-tauri/src/key_detection/scale_match.rs`
- Modify: `src-tauri/src/key_detection/stability.rs`（把稳定的 libKeyFinder 结果传给匹配器）
- Modify: `src-tauri/src/key_detection/mod.rs`（重新导出新类型，修正测试构造）

**Interfaces:**
- Consumes: Task 1 报告中的四个冻结值；`MusicalKey { pitch_class: u8, mode: Mode }`（`types.rs`）。
- Produces:

```rust
pub enum Scale { Major, Minor, Chromatic }            // serde lowercase
pub struct Candidate { pub key: u8, pub scale: Scale } // scale ∈ {Major, Minor}; serde camelCase
pub struct AutoTuneTarget {
    pub key: Option<u8>,              // 完全没有证据时 None；否则为当前叫法的主音
    pub scale: Scale,
    pub candidate: Option<Candidate>, // 当前集合按定名规则的叫法；Chromatic 时也给出
    pub uncovered_notes: Vec<u8>,     // 集合外音级中权重 ≥ GATE.exit × 集合内最小权重者，最多 2 个，按权重降序
    pub evidence_seconds: f64,
    pub source: TargetSource,
}
pub struct GateParams { pub enter: f64, pub exit: f64, pub exit_hold_seconds: f64, pub min_seconds: f64 }
pub const GATE: GateParams;                            // Task 1 冻结值
impl ScaleMatcher {
    pub fn new(params: Params, gate: GateParams) -> Self;
    pub fn seed(&mut self, key: u8, scale: Scale, candidate: Option<Candidate>); // 见 Task 3
    pub fn set_name_hint(&mut self, hint: Option<MusicalKey>);
    pub fn target(&self) -> Option<AutoTuneTarget>;    // 歌曲开始后即为 Some（无证据时 scale=Chromatic, key=None）
}
pub fn transpose(target: AutoTuneTarget, semitones: i32) -> AutoTuneTarget; // 平移 key 与 candidate.key
```

`target()` 在 `reset()` 之后、尚无证据时返回 `Some`（Chromatic、key 为 None、candidate 为 None、uncovered_notes 为空）。这样开头就能写入 Chromatic。Stabilizer 只在有 identity 时调用它（现状不变）。

定名规则：集合由大调主音 `k` 标识，其关系小调主音为 `(k + 9) % 12`。
- `set_name_hint(Some(MusicalKey))` 给出的稳定结果，如果正好是该集合的大调或关系小调，就用它的叫法；
- 否则沿用该集合上次的叫法；
- 第一次出现时用大调名。

- [ ] **Step 1: 改写和新增测试**（`scale_match.rs` 内 `mod tests`）：
  - `no_evidence_is_chromatic_without_key`：`reset()` 后 `target()` 为 `Some { scale: Chromatic, key: None, candidate: None }`。
  - `chromatic_until_min_seconds_then_commits`（替换 `undecided_until_evidence_then_c_major`）。
  - `d_and_dflat_both_strong_is_chromatic_with_candidate`：F 小调音 + D、D♭ 同权；断言 `scale == Chromatic`，`candidate` 为该集合，`uncovered_notes` 含 2 与 1 之一。
  - `exit_requires_hold` 与 `oscillation_between_lines_holds`：与 Task 1 同名测试同义。
  - `relative_pair_follows_stable_hint`（替换 `relative_minor_tie_reports_major_name`）：
    - G# 大调 / F 小调那组音，无提示时为 `G# Major`；
    - 提示 F 小调后为 `F Minor`；
    - 提示变为不相关的 C 大调后，仍为 `F Minor`。
  - `same_hint_twice_does_not_change_target`。
  - `ambiguous_evidence_still_chooses_major_or_minor`：删除，由上面两条取代。
  - `transpose_shifts_key_and_candidate`。
  - `target_serializes_with_profile_compatible_names`：断言 `"chromatic"`、`"candidate"`、`"uncoveredNotes"`。
  - Task 1 的四个离线测试场景，在 Rust 端用同样的证据序列各写一条，期望结果与 Python 一致（防止两边定义漂移）。
- [ ] **Step 2: 运行测试，确认失败**
  Run: `cargo test --manifest-path src-tauri/Cargo.toml key_detection::scale_match`
  Expected: 编译失败或断言失败。
- [ ] **Step 3: 实现。**
  - 在 `ScaleMatcher` 中加入门控状态（`chromatic: bool`、`hold: f64`）和每个集合的上次叫法。
  - `add()` 在现有选择之后更新门控；`seeded && seconds < SEED_HOLD_SECONDS` 时门控也不变。
  - Stabilizer 在 `confirmed` 变化时调用 `matcher.set_name_hint(self.confirmed)`；`reset` 时清空提示。
  - 更新文件头注释：删除“Only Major and Minor are ever chosen”和“no Chromatic state”，改为新规则并引用 spec。
- [ ] **Step 4: 运行测试，确认通过。** 再跑整个 `cargo test --manifest-path src-tauri/Cargo.toml key_detection`，`stability.rs` 中依赖“无证据时 target 为 None”的断言按新语义更新，并在报告里逐条列出。
- [ ] **Step 5: 提交** `feat(key): Chromatic when uncertain, candidate and stable naming`

### Task 3: 按歌缓存保存 Chromatic 与候选

**Files:**
- Modify: `src-tauri/src/key_detection/song_cache.rs`
- Modify: `src-tauri/src/key_detection/stability.rs`（`seed` 调用）

**Interfaces:**
- Consumes: Task 2 的 `Scale`、`Candidate`、`AutoTuneTarget`、`ScaleMatcher::seed`。
- Produces: `CachedKey { key: u8, scale: Scale, candidate: Option<Candidate>, evidence_seconds: f64, updated_ms: u64 }`；`FILE_VERSION = 2`。读取时接受版本 1 和 2，版本 1 的条目 `candidate = None`。

规则：
- `put` 只缓存 `key` 为 Some 的 target：Chromatic 时缓存候选主音与 `candidate`。
- 与已存条目相比，`key`、`scale`、`candidate` 都相同时，不重写。
- `seed(key, Chromatic, candidate)`：开头输出 Chromatic，候选为缓存值，门控初始为 Chromatic；`SEED_HOLD_SECONDS` 前不退出。
- `seed(key, Major|Minor, _)`：保持现行行为，门控初始为非 Chromatic，并在 `SEED_HOLD_SECONDS` 前保持不变。

- [ ] **Step 1: 写测试**
  - `put_stores_chromatic_with_candidate`
  - `reads_version_1_files`：用现有 v1 JSON 字面量，读出 `candidate: None`。
  - `round_trips_version_2`
  - `seeded_chromatic_holds_until_seed_hold`：在 `scale_match` 或 `stability` 测试中，seed Chromatic 后喂 19 s 干净的 C 大调证据，仍为 Chromatic，source 为 Cache。
  - `seeded_set_is_reported_immediately`：保留现有语义。
- [ ] **Step 2: 运行，确认失败**：`cargo test --manifest-path src-tauri/Cargo.toml key_detection::song_cache`
- [ ] **Step 3: 实现**，并在 `song_cache.rs` 文件头注释写明：旧版应用读到 v2 文件，会把它当作损坏文件移到一旁，即降级会丢失缓存。
- [ ] **Step 4: 运行 `cargo test --manifest-path src-tauri/Cargo.toml`，全部通过**
- [ ] **Step 5: 提交** `feat(key): song cache remembers uncertain songs (v2)`

### Task 4: 前端类型与自动写入

**Files:**
- Modify: `src/keyDetection.ts`
- Modify: `src/autoApplyTarget.ts`
- Modify: `src/useAutoApplyTarget.ts`
- Test: `src/keyDetection.test.ts`、`src/autoApplyTarget.test.ts`（没有则新建，沿用 `keyDetection.test.ts` 的测试框架）

**Interfaces:**
- Consumes: Task 2 的 JSON 形状（camelCase：`key | null`、`scale: "major" | "minor" | "chromatic"`、`candidate`、`uncoveredNotes`、`evidenceSeconds`、`source`）。
- Produces:

```ts
export type TargetScale = "major" | "minor" | "chromatic";
export type Candidate = { key: number; scale: "major" | "minor" };
export type AutoTuneTarget = { key: number | null; scale: TargetScale; candidate: Candidate | null;
  uncoveredNotes: number[]; evidenceSeconds: number; source: "analysis" | "cache" };
export type OptionPair = { key: string | null; scale: "Major" | "Minor" | "Chromatic" };
export function autotuneTarget(...): AutoTuneTarget | null;          // 校验新字段
export function targetOptionLabels(target: AutoTuneTarget): OptionPair;
export function titleLabel(target: AutoTuneTarget): string;          // "F Minor" / "F Chromatic" / "Chromatic"
export function nextWrite(prev, target, state, trackKey, chromaticSupported: boolean): OptionPair | null;
```

- `chromaticSupported` 来自 `autoTune.getOptions(candidateId, "scale")`：每个连接查一次并缓存；结果中有 `Chromatic` 标签即为 true；查询失败按 false 处理。
- `chromaticSupported` 为 false 时：
  - Chromatic 的 target 改写为 candidate 的 Major/Minor；
  - 没有 candidate 时返回 null；
  - `useAutoApplyTarget` 通过 `onNotice` 提示一次：“插件不支持 Chromatic，已改用候选调”。
- 写入：`key` 为 null 时只调用 `setDiscrete("scale", ...)`。

- [ ] **Step 1: 写测试**
  - `autotuneTarget` 接受新形状，拒绝 `scale: "dorian"`、`uncoveredNotes` 含越界音级、`key` 越界；
  - `titleLabel` 三种文字使用 profile 拼写（`G#`，不用 `G♯`）；
  - `nextWrite` 在无证据时返回 `{ key: null, scale: "Chromatic" }`；
  - 不支持 Chromatic 时改写为候选；无候选时返回 null；
  - 同一组合不重复写。
- [ ] **Step 2: 运行，确认失败**：`npx vitest run src/keyDetection.test.ts src/autoApplyTarget.test.ts`
- [ ] **Step 3: 实现**；删除注释中所有“Chromatic is never recommended”的表述。
- [ ] **Step 4: 运行单测和 `npx tsc --noEmit`，全部通过**
- [ ] **Step 5: 提交** `feat(ui): write Chromatic and scale-only pairs`

### Task 5: 标题合并为唯一的 Key / Scale

**Files:**
- Modify: `src/components/KeyTitle.tsx`
- Delete: `src/components/AutoTuneTargetStatus.tsx`、`src/components/AutoTuneTargetStatus.css`（样式需要保留的部分并入 KeyTitle 所在样式）
- Modify: `src/App.tsx:139-140`、`src/useAutoTuneTarget.ts`（不再取 `keyLabel` 用于显示）
- Modify: `src/components/AutoApplySettings.tsx:23`（说明文字）
- Test: `tests/key-detection.spec.ts`、`tests/auto-apply-key-scale.spec.ts`

**Interfaces:**
- Consumes: Task 4 的 `titleLabel`、`targetStatusLabel` 的状态判断（迁入 KeyTitle 的提示）、`WrittenPair`。
- Produces: `KeyTitle({ target, written, enabled })`。target 为 null 时显示品牌 `Tune Love`。

提示文字（逐字使用）：
- 证据：沿用现有“已分析 N 秒。”和“来自上次播放的分析结果（本次已分析 N 秒，足够后会重新确认）。”
- 拿不准：`拿不准：<音名1> 与 <音名2> 都常用，暂用 Chromatic。排第一的候选是 <候选标题>。`
  - 只有一个音名时写 `拿不准：<音名1> 常用但不在候选调内，暂用 Chromatic。…`；
  - 无候选时写 `刚开始分析，暂用 Chromatic。`
  - 音名用 profile 拼写。
- 写入状态：沿用现有四种句子，即自动写入已关闭 / 写入失败，不会自动重试 / 已写入当前连接的插件 / 连接 Auto-Tune 后自动写入。
- 写入失败时，标题旁加 `aria-label="写入失败"` 的小标记。
- 设置页说明改为：`未连接时不写入。拿不准或刚开始时写入 Chromatic，确定后写入 Major/Minor，之后只在明显更合适时更换。写入失败不会自动重试。`

- [ ] **Step 1: 更新 Playwright 用例**（先改断言，使其失败）
  - 标题显示 `A Minor`（替换原“A minor 标签”断言）；
  - 无证据时显示 `Chromatic`；
  - `F Chromatic` 的提示含“拿不准”和候选；
  - 写入失败标记可见；
  - `.autotune-target-status` 不存在；
  - 原有动画与布局用例保持语义。
- [ ] **Step 2: 运行，确认失败**：`npx playwright test tests/key-detection.spec.ts tests/auto-apply-key-scale.spec.ts`
- [ ] **Step 3: 实现**
- [ ] **Step 4: 运行全部 Playwright 用例与单测，全部通过**
- [ ] **Step 5: 提交** `feat(ui): one Key/Scale title with write status in its tooltip`

### Task 6: 文档与项目约束

**Files（均在 `E:\autotune helper\`，不在 now-playing 仓库内，直接编辑，不提交）:**
- Modify: `AGENTS.md`：在“路线更新”节加入 2026-10-04 用户决定的条目，内容为 spec“背景与用户决定”第 1–3 点。在两条旧条目（“自动选择的 Scale 只允许 Major / Minor，不使用 Chromatic…”和“不得以‘退回/切换到 Chromatic’作为防误拉手段…”）开头加“（已被 2026-10-04 用户决定取代）”，正文不改。
- Modify: `docs/2026-09-30_vocal-effect-acceptance.md`：补一段说明，Chromatic 时段单独统计（误拉率、修音覆盖率、Chromatic 占比），不计为误拉，也不计为成功。
- Modify: `docs/README.md`：列入 Task 1 报告（如果 Task 1 尚未列入）。

- [ ] **Step 1: 编辑上述三个文件**
- [ ] **Step 2: 用 `grep -rn -i "chromatic" src src-tauri/src` 检查仓库内残留的“Chromatic 从不推荐”表述，并逐条修正**（已在 Task 2/4/5 修改的除外）
- [ ] **Step 3: 仓库内有修正时提交** `docs: Chromatic rule wording`
