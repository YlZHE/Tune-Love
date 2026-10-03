# 去人声降延迟 P0（测量）与 P1（快速收益）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 先把延迟测清楚（诊断日志、拆分探测、设备周期查询、基线复测、校准系统段），再落地三项快速收益：余量衰减（O1）、到达即写（O2）、按电平确认接管增益（R2），最后按标准档目标做真机验收。

**Architecture:** 全部改动在 `devocal/engine` 内。
- P0：诊断日志改为带运行编号与单调时间戳的格式；新增两个开发用示例程序（`split_probe`、`device_periods`），纯分析部分单测；真机测量按新增的操作手册执行，结果作为校准 `SYSTEM_PATH_*` 的输入。
- P1：每项都先抽出纯函数或纯结构（`Headroom`、`plan_fill`、`LevelConfirm`、`Holder::attach_ramp`），用假数据和离线模拟单测，再接到音频线程。音频线程的逐块路径不分配内存、不阻塞（只有带超时的等待）。

**Tech Stack:** Rust 2021；`windows` 0.62.2；vendored `wasapi` 0.24（`src-tauri/vendor/wasapi`）；`rtrb` 0.3；`serde_json`（仅示例与测试）。

**Spec:** `docs/superpowers/specs/2026-10-03-devocal-latency-design.md`（在 `devocal-latency` 分支上；读取：`git -C "E:\autotune helper\now-playing" show devocal-latency:docs/superpowers/specs/2026-10-03-devocal-latency-design.md`）。调研与代码出处：`.superpowers/research/2026-10-03-latency.md`（不入库）。项目规则：`E:\autotune helper\AGENTS.md`。执行者必须同时读设计稿、调研和本计划。本计划只覆盖 P0、P1；P2–P4 等 P0 数据出来后另写计划。


> **控制者裁定（2026-10-03，计划审阅时）**：① 达标判定：5 次释放跳变的中位数须达到目标，且最大值不超过目标 + 5 ms；② R2 以 2.5 ms 为判定单位、抬起的块不得超过接管前参考 RMS 与峰值——采纳；③ 余量衰减退避（5→10→…≤60 s）——采纳；④ O2 第三个等待句柄（截止定时器）与 vendored wasapi `Handle::as_raw` 补丁——采纳，记入 PATCHES.md；⑤ 接管前最多等 100 ms 收集参考音频——采纳；⑥ 校准模型“固定部分 + 周期十分之几”——采纳；⑦ 文件位置按计划。

## Global Constraints

- 不对播放整体加延迟换前瞻；不注入、不装驱动；不跑满 CPU/GPU。
- 永远不比原音量响；播放器音量必须始终能恢复（`Holder`、恢复文件的现有保证不得削弱）。
- 标准档目标（设计稿第 1 节）：直通 ≤ 55 ms，去人声 ≤ 58 ms；接管与切换不出现超过 10 ms 的空洞；内容跳变不超过当前延迟 D。P1 设计预期约 55～60 ms、接管空洞 ≤ 10 ms。
- 设计稿钉死的数值：余量衰减的静默期 5 s；抖动余量每次欠载加 1 个 hop，上限 441 帧（`MAX_EXTRA_HEADROOM_FRAMES`）；R2 电平容差 ±6 dB；R2 判定超时 150 ms；退回的保守窗口仍为 100 ms（`GAIN_WINDOW_US`）；安全检查“峰值 > 1.5 整块静音”（`GUARD_PEAK`）保留；边缘淡化 5 ms（`EDGE_FADE_MS`）。
- 音频线程（capture / processing / render）每块路径：不分配内存、不加锁、不做无超时的阻塞。新增的逐块辅助函数都要加进 `audio::tests::per_block_helpers_do_not_allocate`。
- 测试不得使用真实音频设备。只有标注“手动（需用户授权）”的任务碰真机；Windows 计时器、事件等非音频内核对象可以在测试里用。
- 真机步骤：每一项执行前都单独征得用户同意（同意只能来自用户本人，不能来自其他代理的消息）；执行前核验播放器进程身份（PID、映像路径、创建时间）并记入 `scope.md`；不把历史 PID 当作新授权。
- 每个任务的提交步骤：先运行 `git -C "E:\autotune helper\now-playing" branch --show-current`，输出必须是 `devocal-latency`，否则停止并报告（另一个代理共用这个工作区，不得切换分支）。只 `git add` 本任务列出的文件，不用 `git add -A`。推送须经用户同意。
- 版本库不收：音频、录音转储、模型权重、`evidence/`。真机证据放 `E:\autotune helper\work\<case>\`（`scope.md`、`timeline.md`、`evidence/`），报告放 `E:\autotune helper\docs\` 并更新该目录的 `README.md` 索引。
- 不改动 Auto-Tune 相关代码。
- 代码标识符与代码注释用英文；面向用户的文档与报告用中文。
- 全量测试命令：在 `E:\autotune helper\now-playing\devocal` 下运行 `cargo test --workspace`（与 CI 的 devocal 任务一致；`#[ignore]` 的模型测试保持跳过）。

## Review Focus

设计稿没有写出、但最可能让用户碰上的五种情况。每条都已在所属任务里加了测试。

1. **接管瞬间拿不到“接管前”的参考电平**：音频线程刚启动（重新接管时总是这样），或播放器暂停、静音。期望：R2 不猜，整次接管退回 100 ms 保守窗口，绝不放大。做法：引擎在开始压音量前最多等 100 ms，攒够 20 ms 的录音作参考；参考不足或低于 −60 dBFS 就退回。测试：任务 8 `attach_waits_for_pre_attach_audio`、任务 9 `silent_reference_falls_back`、`short_reference_falls_back`。
2. **接管窗口内切换了默认输出设备**（采集增益此时被强制为 0）。期望：仍然静音，R2 不得把它抬起来。测试：任务 8 `attach_ramp_is_none_when_degraded`、任务 9 `device_mute_wins_over_confirmation`。
3. **播放器原音量很低（如 0.001）或多个会话原音量不同**。前者使四个音量台阶只差约 5 dB，±6 dB 窗口会重叠；后者使混合电平不按单一台阶下降。期望：取较小的增益，任何时刻都不高于接管前的参考电平。测试：任务 9 `overlapping_steps_pick_the_largest_volume`、`never_louder_than_the_reference_during_any_attach`（含 0.001 与双会话）。
4. **用户打开去人声时，预缓冲请求先于 `WarmingUp` 状态到达渲染线程**（处理线程先置请求、后发布状态）。期望：预缓冲不能因为“还没看到 WarmingUp”而被立刻撤掉，否则模型预热时断音并误退回原声。测试：任务 6 `preroll_kept_until_model_stage_seen_or_grace`。
5. **余量衰减遇到暂停或反复抖动的机器**。期望：暂停时衰减不丢任何内容；抖动反复出现时，不会每 5 s 断一次音。做法：队列里没有多余内容时取消跳过；衰减后 60 s 内再欠载，所需静默期翻倍（5 → 10 → 20 → … ≤ 60 s）。测试：任务 6 `decay_during_pause_drops_nothing`、`decay_backs_off_when_jitter_returns`。

---

## 执行顺序与依赖

- 任务 1–3：P0 代码，互不依赖，按序做。
- 任务 4：真机基线测量（手动，需授权），使用任务 3 完成时的提交构建；任务 5 依赖任务 4 的数字。
- 任务 6–9：P1 代码，不依赖 P0 数据。用户尚未授权真机时可以先做 6–9。若 P1 已提交而任务 4 还没做，基线必须用任务 3 的提交单独构建：`git worktree add ..\np-p0-baseline <任务 3 提交>`（另建目录，不动共享工作区）。
- 任务 10：最终真机验收（手动，需授权），要求任务 5 已完成。

## 文件结构

```text
devocal/engine/
  Cargo.toml                         # [[example]] split_probe / device_periods (test = true)
  src/audio/mod.rs                   # 任务 1 诊断日志；任务 7 HiResTimer、Shared.render_wake；
                                     # 任务 8 SharedGains 的接管台阶字段
  src/audio/render.rs                # 任务 5 系统段公式；任务 6 Headroom 与衰减修剪；任务 7 plan_fill、等待
  src/audio/processing.rs            # 任务 7 每推一块就唤醒渲染线程
  src/audio/capture.rs               # 任务 9 按小块应用增益
  src/audio/confirm.rs               # 任务 9 新增：LevelConfirm（R2 电平确认）
  src/holder.rs                      # 任务 8 AttachRamp、attach_ramp()；ramp_value 改为 pub(crate)
  src/engine.rs                      # 任务 8 发布接管台阶；接管前等参考录音；AudioPort::input_frames
  examples/split_probe.rs            # 任务 2 新增
  examples/device_periods.rs         # 任务 3 新增
src-tauri/vendor/wasapi/src/api.rs   # 任务 7：Handle::as_raw（本地补丁）
src-tauri/vendor/wasapi/PATCHES.md   # 任务 7：记录该补丁
docs/devocal-latency-measurement.md  # 任务 4 新增：测量操作手册（中文，无本机数据）
```

---

### Task 1: 诊断日志防误读（运行编号、单调时间戳、partial 末行）

**Files:**
- Modify: `devocal/engine/src/audio/mod.rs`（`diag_loop`、`Shared`、`AudioHandle::start`、测试）
- Modify: `devocal/engine/src/audio/render.rs`（`after_open` 的打开日志行；测试里构造 `Shared` 处）
- Modify: `devocal/engine/src/audio/processing.rs`（测试里构造 `Shared` 处）

**Interfaces:**
- Produces（后续任务往表里加计数器名）：
  ```rust
  pub(crate) fn next_run_id() -> u32;                 // 进程内单调递增，从 1 开始
  // Shared 新增字段：pub run_id: u32
  pub(crate) fn diag_line(run: u32, t_us: u64, span_us: u64,
                          fields: &[(&str, u64)], tail: &str, partial: bool) -> String;
  pub(crate) fn diag_loop_with(stats: Arc<AudioStats>, shared: Arc<Shared>, gains: Arc<SharedGains>,
                               interval_us: u64, emit: impl FnMut(String));
  pub(crate) fn render_open_line(run: u32, describe: &str, period: usize, buffer: usize) -> String;
  ```
- 行格式（精确）：
  - 起始行：`devocal diag: run={run} start t={t_us/1e6:.6} unix_ms={unix_ms}`；
  - 周期行：`devocal diag: run={run} t={t_us/1e6:.6} span_ms={span_us/1000} {name=delta ...} {tail}`；末行在最后追加 ` partial`；
  - `tail` 为现有的 `stage=.. in_ring=.. headroom=.. cap_gain=.. out_gain=..`，再追加 `lat_ms={latency_ms_milli/1000:.1}`（供任务 4 读取引擎自估）；
  - 渲染打开行：`devocal audio: run={run} render {describe} (period {period} frames, buffer {buffer})`。
- `t` 用 `now_us()`（QPC，单调）。

- [ ] **Step 1: 写失败测试**（`audio/mod.rs` tests）
  - `diag_line_carries_run_time_and_span`：
    `diag_line(7, 12_345_678, 1_000_000, &[("cap_pkts", 100), ("underruns", 0)], "stage=0", false)`
    `== "devocal diag: run=7 t=12.345678 span_ms=1000 cap_pkts=100 underruns=0 stage=0"`；
    同参数且 `partial=true`、`span_us=420_000` 时以 `"span_ms=420 cap_pkts=100 underruns=0 stage=0 partial"` 结尾。
  - `diag_loop_emits_a_final_partial_line_after_stop`：在线程上运行 `diag_loop_with(.., interval_us = 100_000, emit = 发往 mpsc)`，250 ms 后置 `shared.stop`，线程须在 200 ms 内结束。收到的行：第 1 行含 ` start `；接着恰好 2 行不含 `partial`；最后 1 行以 ` partial` 结尾且其 `span_ms` < 100；所有行含同一个 `run=`；各行 `t=` 严格递增。
  - `run_ids_increase`：连续两次 `next_run_id()`，后者 > 前者。
  - `render_open_line_names_the_run`：`render_open_line(3, "wasapi autoconvert", 441, 1036) == "devocal audio: run=3 render wasapi autoconvert (period 441 frames, buffer 1036)"`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine audio::tests::diag` → 编译失败（函数未定义）。
- [ ] **Step 3: 实现。**
  - `diag_loop_with`：每 ≤ 50 ms 醒一次查 `shared.stop`；按 `interval_us` 输出周期行；看到 stop 后立即输出一条 partial 末行（span 为距上一行的实际时长）并返回。`diag_loop` 改为 `diag_loop_with(.., 1_000_000, |l| eprintln!("{l}"))`。
  - `AudioHandle::start` 用 `next_run_id()` 填 `Shared.run_id`；`Renderer::after_open` 改用 `render_open_line`。
  - 各测试里构造 `Shared` 的地方补上 `run_id`。
- [ ] **Step 4: 运行测试。** `cargo test --workspace`（在 `devocal` 下）→ PASS。
- [ ] **Step 5: 提交。** 确认分支为 `devocal-latency` 后：
  ```bash
  git add devocal/engine/src/audio/mod.rs devocal/engine/src/audio/render.rs devocal/engine/src/audio/processing.rs
  git commit -m "devocal engine: diag lines carry run id, QPC time and a partial last line"
  ```

### Task 2: 被动拆分探测 `split_probe`

**Files:**
- Create: `devocal/engine/examples/split_probe.rs`
- Modify: `devocal/engine/Cargo.toml`（`[[example]] name = "split_probe"`，`test = true`）

**Interfaces:**
- 命令行：
  ```text
  split_probe --pid <player root pid> --seconds <s> [--endpoint <render endpoint id>]
              [--small-period --confirm-period-change] [--out <json>] [--dump <prefix>]
  split_probe --analyze <prefix> [--out <json>]
  ```
  - 同时录进程回环（`--pid`，含子进程树）与端点回环（`--endpoint` 或默认端点），两路都以该端点混音格式的采样率、float32 立体声打开（端点回环因此不做采样率转换）。
  - 每个包记录 `PacketTime { arrival_us: u64 /* 读到包时的 now QPC µs */, device_qpc_100ns: u64, first_frame: u64, frames: u32 }`。
  - `--small-period`：录音期间在同一端点用 `IAudioClient3::InitializeSharedAudioStream` 以最小周期打开一个只写静音（`AUDCLNT_BUFFERFLAGS_SILENT`）的渲染流；必须同时给 `--confirm-period-change`，否则报错退出（它会改变该端点上所有程序的引擎周期）。报告 default/fundamental/min/max 与 `GetCurrentSharedModeEnginePeriod` 的实际周期。
  - 只录音；不连接引擎，不改任何会话音量。
  - `--dump` 写 `<prefix>-source.f32`、`<prefix>-output.f32`（交错立体声 f32 LE）、`<prefix>-packets.csv`（`stream,arrival_us,device_qpc_100ns,first_frame,frames`）、`<prefix>-meta.json`（采样率、小周期信息）；`--analyze` 读回它们并输出同样的报告。
- 纯分析函数（单测对象）：
  ```rust
  struct LagEstimate { frames: i64, corr: f64, windows: usize }
  fn global_lag(out: &[f32], src: &[f32], rate: u32, max_lag_ms: u32) -> Option<LagEstimate>;
  fn delivery_offsets_us(src: &[PacketTime], out: &[PacketTime], lag_frames: i64) -> Vec<i64>;
  struct Spread { n: usize, median: f64, p05: f64, p95: f64, min: f64, max: f64 }
  fn spread(values: &[f64]) -> Option<Spread>;
  fn packet_stats(p: &[PacketTime]) -> serde_json::Value; // 包大小直方图、到达间隔中位数与 p95
  fn parse_args(args: &[String]) -> Result<Args, String>;
  ```
  - `global_lag`：单声道；4096 帧窗口、每 0.25 s 一窗；每窗在 ±`max_lag_ms` 内找使 `out[i]` 与 `src[i − L]` 归一化相关最大的 L（先 8 倍抽取粗搜，再全采样率细化）；只取相关 ≥ 0.5 的窗，返回各窗 L 的中位数。方法与 `latency_probe` 一致（示例之间无法共享代码，按其方法重写即可，不改 `latency_probe`）。
  - `delivery_offsets_us`：对每个源包取其最后一帧 x，输出中对应帧为 y = x + L；偏移 = 源中含 x 的包的 `arrival_us` − 输出中含 y 的包的 `arrival_us`。正值表示进程回环交付更晚。y 超出输出范围的包跳过。
- 报告 JSON 键：`rate`、`lagFrames`、`lagCorr`、`deliveryOffsetUs`（`spread` 结果）、`sourcePackets`、`outputPackets`（`packet_stats` 结果）、`smallPeriod`（无则 `null`）。

- [ ] **Step 1: 写失败测试**（示例内 `#[cfg(test)]`，合成信号，不碰设备）
  - `global_lag_finds_a_known_offset`：48 kHz 确定性白噪声 6 s，输出 = 源延后 1234 帧，`frames` ∈ [1233, 1235]，`corr` > 0.99。
  - `late_source_packets_give_a_positive_offset`：输出包每 10 ms 一个、480 帧，arrival = k·10 000 µs；源包同内容、441 帧，到达比内容对应的输出包晚 13 000 µs，lag = 0。`spread(offsets)` 的中位数 ∈ [12 999, 13 001]。
  - `offsets_skip_frames_outside_the_output`：输出比源短时，越界的源包不产生偏移值，结果长度等于可对应的包数。
  - `spread_reports_percentiles`：`spread(&[1.0..=100.0])` → median 50.5，p05 ≈ 5.95（线性插值），min 1，max 100；空切片 → `None`。
  - `small_period_needs_explicit_confirmation`：`--small-period` 不带 `--confirm-period-change` → `Err`；两者都带 → `Ok`；`--analyze` 与 `--pid` 同用 → `Err`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine --example split_probe` → 编译失败。
- [ ] **Step 3: 实现** 录音、可选静音小周期流、转储与 `--analyze`、上述纯函数。录音部分沿用 `latency_probe::record` 的结构；`arrival_us` 在 `read_from_device` 返回后立即读 QPC。
- [ ] **Step 4: 运行测试。** `cargo test -p devocal-engine --example split_probe` 与 `cargo test --workspace` → PASS；`cargo build -p devocal-engine --example split_probe --release` 成功。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/examples/split_probe.rs devocal/engine/Cargo.toml
  git commit -m "devocal: split_probe measures process loopback delivery per packet"
  ```

### Task 3: 只读设备周期查询 `device_periods`

**Files:**
- Create: `devocal/engine/examples/device_periods.rs`
- Modify: `devocal/engine/Cargo.toml`（`[[example]] name = "device_periods"`，`test = true`）

**Interfaces:**
- 用法：`device_periods [--out <json>]`。对每个活动渲染端点（`IMMDeviceEnumerator::EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)`）：`Activate` 一个 `IAudioClient3`，只调用 `GetMixFormat` 与 `GetSharedModeEnginePeriod`，**从不 Initialize、从不 Start**；名称用 wasapi `DeviceEnumerator::get_device(id)?.get_friendlyname()`。
- 纯函数：
  ```rust
  struct MixFormat { sample_rate: u32, channels: u16, float32: bool }
  unsafe fn read_mix_format(fmt: *const WAVEFORMATEX) -> MixFormat; // 照 audio::endpoint::read_mix_format 实现
  fn low_latency_capable(f: MixFormat, min_period_frames: u32) -> bool;
  // = float32 && channels >= 2 && min_period_frames * 1000 / sample_rate <= 3.0 ms（设计稿：支持 ≤ 3 ms 共享周期、混音格式为 float）
  fn row_json(id: &str, name: &str, f: MixFormat, default: u32, fundamental: u32, min: u32, max: u32) -> serde_json::Value;
  // 键：id、name、mixRate、channels、float32、defaultFrames、fundamentalFrames、minFrames、maxFrames、minMs、lowLatencyCapable
  ```
  单个端点出错时，该行写 `error` 字段，其余端点照常输出。

- [ ] **Step 1: 写失败测试**
  - `low_latency_needs_float_stereo_and_3ms`：48 kHz float 立体声：min 128 → true，144（3.0 ms）→ true，480 → false；44.1 kHz float、min 441 → false；int16 → false；单声道 → false。
  - `mix_format_parses_plain_and_extensible_float`：照 `audio::endpoint` 同名测试（WAVEFORMATEXTENSIBLE float、extensible PCM、plain float 48 kHz）。
  - `row_json_reports_min_ms`：48 kHz、min 128 → `minMs` ≈ 2.667（误差 1e-3），`lowLatencyCapable == true`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine --example device_periods`。
- [ ] **Step 3: 实现** 枚举与查询；每个 COM 对象在 `CoUninitialize` 前释放。
- [ ] **Step 4: 运行测试。** 同上与 `cargo test --workspace` → PASS；`--release` 构建成功。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/examples/device_periods.rs devocal/engine/Cargo.toml
  git commit -m "devocal: device_periods lists shared-mode engine periods read-only"
  ```

### Task 4: 测量手册与 P0 真机基线（手动，需用户授权）

**Files:**
- Create: `docs/devocal-latency-measurement.md`（入库；只写方法，不含本机数据）
- Create（不入库）：`E:\autotune helper\work\devocal-latency-p0-<日期>\{scope.md,timeline.md,evidence\}`
- Create（不入库）：`E:\autotune helper\docs\<日期>_devocal-latency-p0-report.md`；Modify：`E:\autotune helper\docs\README.md` 索引

**Interfaces:**
- Consumes：任务 1 的 `lat_ms=` 与渲染打开行；任务 2、3 的示例程序；现有 `latency_probe`。
- Produces：报告里固定的“校准输入”表（任务 5 只从这张表取值）：

  | 键 | 含义 |
  |---|---|
  | `P_frames`、`P_ms` | 渲染周期（渲染打开行），换算成毫秒 |
  | `M_a`、`M_b`、`M_c` | 状态 (a)(b)(c) 各 5 次 `releaseLatencyMs` 绝对值的中位数（ms），并列出最大值 |
  | `E_a`、`E_b`、`E_c` | 同一批运行中，释放前 5 s 内诊断行 `lat_ms` 的中位数（ms；它已含旧的 3 个周期） |
  | `S`、`S_small` | `split_probe` 的 `deliveryOffsetUs.median`（µs），分别为不开、开小周期流；附两次的源包大小直方图 |
  | `devices` | `device_periods` 输出的表 |

- [ ] **Step 1: 写手册 `docs/devocal-latency-measurement.md`**（无需授权）。内容：
  - 构建：`node scripts/build-devocal-engine.mjs --release`；在 `devocal` 下 `cargo build -p devocal-engine --release --examples`；应用按 2026-10-03 验收的方式用 `src-tauri/tauri.verify.conf.json` 构建。以 `DEVOCAL_DIAG=1` 启动应用，使引擎继承该环境变量。
  - 身份核验：播放器 PID、映像路径、创建时间，写入 `scope.md`。
  - 状态定义：(a) 接管后从未开过去人声的直通；(b) 开、关一次去人声后的直通；(c) 去人声稳定 ≥ 10 s。
  - 单次运行：`latency_probe --pid <pid> --seconds 20 --dump <evidence>\<state>-<n>`；接管状态保持 ≥ 5 s 后由用户在界面释放，之后再录 ≥ 5 s；读 `releaseLatencyMs`，再用 `latency_probe --analyze <prefix>` 复核。每次运行前后记录诊断日志里对应 `run=` 的 `lat_ms`、`headroom`、渲染打开行。
  - 每个状态 5 次，报中位数与最大值；中间释放、再接管均由用户在界面操作。
  - 拆分探测：不接管时，`split_probe --pid <pid> --seconds 20 --dump ...` 一次；经**单独授权**后加 `--small-period --confirm-period-change` 再一次。
  - 设备查询：`device_periods --out <evidence>\devices.json`。
  - 收尾：释放；确认播放器音量回到原值、恢复文件不存在；记录到 `timeline.md`。
  - 证据的 SHA-256 写进 `evidence-hashes.txt`。
- [ ] **Step 2: 提交手册。** 确认分支后：`git add docs/devocal-latency-measurement.md`，`git commit -m "docs: devocal latency measurement procedure"`。
- [ ] **Step 3: 征得同意（停在这里等用户）。** 向用户说明：用哪个播放器；应用会接管它；需要用户在界面开关去人声并释放 15 次以上；会录两路回环音频；`--small-period` 那一次会把该端点的引擎周期临时改小（单独征得同意）。没有得到用户本人的同意就把本任务标为 BLOCKED，继续做任务 6–9。
- [ ] **Step 4: 按手册执行**，使用任务 3 的提交构建（见“执行顺序与依赖”）。在 `timeline.md` 记录每一步与时间。
- [ ] **Step 5: 写报告**，填好“校准输入”表；如实记录异常与未完成项。同时回答设计稿 P0 的问题：进程回环交付延迟是否随周期变小（比较 `S` 与 `S_small`、两次的包大小）。
- [ ] **Step 6: 收尾核对**：播放器音量已回到原值；恢复文件不存在；关闭测试应用前确认未处于接管状态。

### Task 5: 用实测校准系统段

**Files:**
- Modify: `devocal/engine/src/audio/render.rs`（`SYSTEM_PATH_PERIODS` → 新常量与 `latency_frames`；测试）
- Modify: `devocal/engine/src/audio/mod.rs`（`AudioStats::latency_ms_milli`、`LATENCY_BUDGET_FRAMES` 的文档注释）

**Interfaces:**
- Consumes：任务 4 报告的“校准输入”表。**表里缺任何一个值，本任务就是 BLOCKED，不得自行编造或估算。**
- Produces：
  ```rust
  pub(crate) const SYSTEM_PATH_FIXED_US: u64;      // 不随渲染周期缩放的部分
  pub(crate) const SYSTEM_PATH_PERIOD_TENTHS: u64; // 随周期缩放的部分，单位 0.1 个周期
  fn system_path_frames(period: u64) -> u64;       // FIXED_US*44_100/1_000_000 + TENTHS*period/10
  fn latency_frames(capture_packet: u64, ring_a: u64, ring_b: u64, padding: u64,
                    proc_latency: u64, period: u64) -> u64; // 管线各项 + system_path_frames(period)；period 为 0（无输出）时系统段为 0
  ```
- 换算规则（固定，执行时只代入数字）：
  1. 我们自己的部分：`our_x = E_x − 3·P_ms`（x ∈ {a, c}）。
  2. 系统段：`sys_x = M_x − our_x`；`sys = (sys_a + sys_c) / 2`。若 `|sys_a − sys_c| > 3 ms`，在报告与提交说明里写明，仍按平均值。
  3. 固定部分：若 `|S − S_small| < 2000 µs`（交付延迟不随周期变化），`fixed_us = S`；否则 `fixed_us = 0`。
  4. `tenths = max(0, round(10 · (sys − fixed_us/1000) / P_ms))`。

- [ ] **Step 1: 写失败测试**（把表里的实际数值写进测试）
  - `system_path_matches_the_p0_measurement`：`system_path_frames(P_frames)` 换算成毫秒，与 `sys` 的差 ≤ 1 ms。
  - `latency_estimate_reproduces_state_a`：`our_a`（帧）+ `system_path_frames(P_frames)` 换算成毫秒，与 `M_a` 的差 ≤ 1 ms。
  - 改写 `latency_frames_adds_three_render_periods` → `latency_frames_adds_the_system_path`：`latency_frames(441, 128, 1_000, 200, 128, 441) == 1_897 + system_path_frames(441)`；period 为 0 时 `== 1_897`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine render::tests::` → FAIL（常量不存在）。
- [ ] **Step 3: 实现。** 删除 `SYSTEM_PATH_PERIODS`，改用上述常量与函数；更新注释，写明数据来源（报告文件名、日期、`M_a`/`E_a`/`S` 等数值）。
- [ ] **Step 4: 运行。** `cargo test --workspace` → PASS。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/src/audio/render.rs devocal/engine/src/audio/mod.rs
  git commit -m "devocal engine: calibrate the system path from the P0 measurements"
  ```

### Task 6: O1 余量衰减

**Files:**
- Modify: `devocal/engine/src/audio/render.rs`（新增 `Headroom` 与衰减修剪；`Renderer` 用它代替 `extra`；测试）
- Modify: `devocal/engine/src/audio/mod.rs`（`DiagCounters::render_decay_frames`，诊断字段名 `r_decay`；不分配测试）

**Interfaces:**
- Consumes：`stage_from_code`、`Stage`、`MAX_EXTRA_HEADROOM_FRAMES`、`PREROLL_FRAMES`、`preroll_frames()`（现有）。
- Produces：
  ```rust
  pub const HEADROOM_DECAY_US: u64 = 5_000_000;      // 设计稿：连续 5 s 无欠载
  pub const HEADROOM_DECAY_MAX_US: u64 = 60_000_000; // 退避上限，也是“衰减后多久算干净”
  pub const PREROLL_GRACE_US: u64 = 200_000;
  pub const LOW_ENERGY_WAIT_US: u64 = 1_000_000;
  pub const LOW_ENERGY_RATIO: f32 = 0.5;             // 窗口 RMS ≤ 近期输出 RMS 的一半（−6 dB）
  pub struct Headroom { /* jitter, preroll, preroll_at_us, preroll_model_seen, last_underrun_us, quiet_needed_us, last_decay_us */ }
  impl Headroom {
      pub fn new() -> Self;
      pub fn reset(&mut self);
      pub fn total(&self) -> usize;                                  // jitter + preroll ≤ cap
      pub fn on_underrun(&mut self, jitter: bool, hop: usize, cap: usize, now_us: u64);
      pub fn add_preroll(&mut self, frames: usize, now_us: u64);
      pub fn release(&mut self, stage: Stage, now_us: u64) -> usize; // 本次释放的帧数
  }
  pub fn decay_now(window_rms: f32, recent_rms: f32, waited_us: u64) -> bool;
  ```
- 规则：
  - `on_underrun`：每次计数的欠载都把静默计时归零（`last_underrun_us = now`）；`jitter == true` 时 `jitter = min(jitter + hop, cap − preroll)`。若距上次衰减 < `HEADROOM_DECAY_MAX_US`，`quiet_needed_us` 翻倍（上限 `HEADROOM_DECAY_MAX_US`）；否则重置为 `HEADROOM_DECAY_US`。
  - `release`：
    - 预缓冲部分：stage 为 `WarmingUp` 或 `FadingIn` 时记 `preroll_model_seen = true`，不释放；其他 stage 下，若已见过模型阶段，或距 `add_preroll` 已过 `PREROLL_GRACE_US`，就全部释放。
    - 抖动部分：距 `last_underrun_us`（无欠载则从最近一次增长或衰减算起）≥ `quiet_needed_us` 时全部释放，并记 `last_decay_us = now`。
  - `decay_now`：`window_rms <= LOW_ENERGY_RATIO * recent_rms || waited_us >= LOW_ENERGY_WAIT_US`。
- `Renderer` 的改动：
  - 目标 = `period + hop + headroom.total()`。
  - `release` 释放出的帧累加到 `decay_drop`，记 `decay_since_us`。
  - 每次写入前，若 `decay_drop > 0`：
    - `spare = (padding + avail).saturating_sub(target)`；`spare == 0` 时取消（`decay_drop = 0`，暂停时就是这种情况）。
    - 否则，用 `read_chunk` 只读不提交地窥看环 B 前 `fade_frames + min(decay_drop, spare)` 帧，算出 `window_rms`。`recent_rms` 是已写出真实帧的均方指数平均（时间常数 4410 帧）的平方根，开方前保留均方。
    - `decay_now` 为真时，令 `drop_pending = min(decay_drop, spare, avail − fade_frames)`，走现有修剪路径（淡出、跳过、淡入），并计入 `render_decay_frames`（不计入 `render_trim_frames`）；然后 `decay_drop = 0`。
  - `after_open` 调 `headroom.reset()` 并清零 `decay_drop`。
  - `headroom_frames` 统计改为发布 `headroom.total()`。
  - 为便于测试，`fill` 拆为 `fill_at(&mut self, now_us: u64)`，`fill()` 用 `now_us()` 调它。

- [ ] **Step 1: 写失败测试**（`render.rs` tests，时间为合成值）
  - `preroll_headroom_lasts_only_while_the_model_warms`：`add_preroll(441, t)`；`release(WarmingUp, t+1 ms) == 0`；`release(FadingIn, t+100 ms) == 0`；`release(Devocal, t+300 ms) == 441`；`total() == 0`。
  - `preroll_kept_until_model_stage_seen_or_grace`（Review Focus 4）：`add_preroll(441, t)`；`release(Passthrough, t+1 ms) == 0`；`release(Passthrough, t+199 ms) == 0`；`release(Passthrough, t+200 ms) == 441`。
  - `jitter_headroom_decays_after_5s_without_underrun`：t 与 t+1 ms 各一次 `on_underrun(true, 128, 441, ..)` → `total() == 256`；`release(Passthrough, t+1 ms+4.999 s) == 0`；`release(.., t+1 ms+5 s) == 256`。
  - `any_underrun_restarts_the_quiet_timer`：t 时增长；t+3 s 有一次 `on_underrun(false, ..)`；`release` 在 t+5 s 为 0，在 t+8 s 为 128。
  - `jitter_regrows_after_decay_up_to_the_cap`：衰减后连续 4 次抖动欠载，`total()` 依次为 128、256、384、441。
  - `decay_backs_off_when_jitter_returns`（Review Focus 5）：T 时衰减；T+2 s 欠载 → 需静默 10 s：`release` 在 T+2 s+9.999 s 为 0，在 T+12 s 为 128；T+13 s 再欠载 → 需 20 s；若距上次衰减已 ≥ 60 s 才欠载，则恢复为 5 s。
  - `decay_waits_for_a_quiet_spot_at_most_1s`：`decay_now(0.4, 1.0, 0)`；`!decay_now(0.6, 1.0, 999_999)`；`decay_now(0.6, 1.0, 1_000_000)`。
  - `peeking_ring_b_does_not_consume`：`read_chunk(n)` 后不提交直接丢弃，`slots()` 不变。
  - `decay_trims_the_released_headroom_with_fades`：FakeSink（周期 441、缓冲 4410）；抖动余量 441；环 B 持续喂 0.5 的常值信号（不是低能量）；以 1 ms 步长调用 `fill_at`，同时模拟设备每 10 ms 消耗 441 帧。断言：5 s 前 `render_decay_frames == 0`；在 6 s ± 20 ms 内（等满 1 s 后强制执行）`render_decay_frames` 变为 > 0 且 ≤ 441；`headroom_frames` 统计为 0；写出的数据中，跳过点前一帧为 0（淡出）。
  - `decay_during_pause_drops_nothing`（Review Focus 5）：抖动余量 441 后环 B 不再有输入；6 s 后 `render_decay_frames == 0`、`headroom.total() == 0`。
  - 改写 `after_open_keeps_a_pending_preroll_request` 与 `a_request_kept_through_after_open_is_consumed_by_the_next_fill`，改为通过 `headroom.total()` 断言；`standing_latency_with_preroll_stays_within_budget` 保持通过。
  - 把 `Headroom::{on_underrun, add_preroll, release, total}` 与 `decay_now` 加进 `per_block_helpers_do_not_allocate`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine render::tests::` → 编译失败。
- [ ] **Step 3: 实现** `Headroom`、`decay_now`、`Renderer` 的上述改动，以及诊断字段 `r_decay`。
- [ ] **Step 4: 运行。** `cargo test --workspace` → PASS。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/src/audio/render.rs devocal/engine/src/audio/mod.rs
  git commit -m "devocal engine: render headroom decays after 5 s without underrun (O1)"
  ```

### Task 7: O2 到达即写

**Files:**
- Modify: `devocal/engine/src/audio/render.rs`（`Wake`、`plan_fill`、`deadline_mode`、等待循环、`Sink::event`、模拟器测试）
- Modify: `devocal/engine/src/audio/mod.rs`（`HiResTimer`；`Shared.render_wake`；诊断计数器 `r_data`、`r_deadline`）
- Modify: `devocal/engine/src/audio/processing.rs`（推入环 B 后 `render_wake.set()`；测试）
- Modify: `src-tauri/vendor/wasapi/src/api.rs`（`impl Handle { pub fn as_raw(&self) -> HANDLE }`）、`src-tauri/vendor/wasapi/PATCHES.md`

**Interfaces:**
- Consumes：任务 6 的 `Headroom::total()`（目标）与 `fill_at`。
- Produces：
  ```rust
  pub const DEADLINE_GUARD_US: u64 = 1_500;
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) enum Wake { Device, Data, Deadline, Timeout }
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub(crate) struct FillPlan { pub real: usize, pub silence: usize }
  pub(crate) fn plan_fill(wake: Wake, deadline_mode: bool, padding: usize, avail: usize,
                          room: usize, period: usize, target: usize) -> FillPlan;
  pub(crate) fn deadline_mode(has_timer: bool, period_frames: usize) -> bool; // has_timer && period_us > 2*DEADLINE_GUARD_US
  pub(crate) fn wake_from_wait(result: u32, has_timer: bool) -> Wake;         // WAIT_OBJECT_0+0/1/2 → Device/Data/Deadline，其余 → Timeout
  pub(crate) struct HiResTimer;   // CreateWaitableTimerExW(.., CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
  impl HiResTimer { pub fn new() -> Option<Self>; pub fn arm_in_us(&self, us: u64); pub fn raw(&self) -> HANDLE; }
  // Sink：fn wait(&self, timeout_ms) 改为 fn event(&self) -> HANDLE
  // Shared 新增：pub render_wake: OwnedEvent   // 处理线程每推入环 B 一块就 set
  ```
- `plan_fill` 规则（这就是重新推导后的目标；队列 = 设备 padding + 环 B）：
  - `real = min(target − padding, room, avail)`（饱和减法）。
  - 是否检查补静音：`(deadline_mode && wake == Deadline) || wake == Timeout || (!deadline_mode && wake == Device)`。
  - 检查时若 `padding + real < period`，则 `silence = min(target, padding + room) − (padding + real)`，否则为 0。
  - 含义：只在设备下一次读取前 `DEADLINE_GUARD_US` 的截止时刻核对一次，保证两次写入之间、设备读取那一刻队列 ≥ 一个周期；其余时刻有数据就写，不补静音。目标公式不变（`period + hop + headroom`），作为写入上限；修剪仍由 `TrimPolicy` 处理。
- 渲染循环：
  - `WaitForMultipleObjects([设备事件, render_wake, 定时器?], false, WAIT_MS)` → `Wake`。
  - 设备唤醒后，若处于截止模式，`timer.arm_in_us(period_us − DEADLINE_GUARD_US)`。
  - 然后 `fill_at(wake, now)`。
  - 只有 `Device` 与 `Timeout` 唤醒时才调用 `TrimPolicy::observe` 与 `publish`；预缓冲、淡化、欠载判定在各种唤醒里沿用现有逻辑，补静音改由 `FillPlan.silence` 决定。
  - 没有 sink 时行为不变（丢弃、睡 `FAILED_POLL`），不因数据事件空转。
  - `fill_at` 签名改为 `fill_at(&mut self, wake: Wake, now_us: u64)`。
- `WasapiSink::event` 用 vendored wasapi 的 `Handle::as_raw()`；`LowLatencySink::event` 用 `OwnedEvent::raw()`。

- [ ] **Step 1: 写失败测试**
  - `plan_fill` 单测（period 441、target 569、room 4000）：
    - `data_wake_writes_real_frames_only`：`(Data, true, 100, 300)` → `{real: 300, silence: 0}`；
    - `deadline_wake_pads_up_to_target_when_short`：`(Deadline, true, 100, 200)` → `{real: 200, silence: 269}`；
    - `device_wake_in_deadline_mode_never_pads`：`(Device, true, 0, 0)` → `{0, 0}`；
    - `legacy_mode_pads_on_device_wake`：`(Device, false, 100, 200)` → `{200, 269}`；
    - `timeout_wake_pads_like_legacy`：`(Timeout, true, 0, 0)` → `{0, 569}`；
    - `target_caps_real_frames`：`(Data, true, 500, 1000)` → `{69, 0}`。
  - `deadline_mode_needs_a_timer_and_a_long_enough_period`：`(true, 441)` → true；`(true, 128)`（2.9 ms ≤ 3 ms）→ false；`(false, 441)` → false。
  - `wake_from_wait_maps_handles`：0 → Device，1 → Data，2 → Deadline（有定时器时），`WAIT_TIMEOUT` → Timeout；无定时器时 2 → Timeout。
  - 模拟器（`render.rs` 测试内，纯整数时间）。模型：
    - 设备每 10 000 µs 读 441 帧；读取前 padding < 441 记一次 `starved_read`。读完立即产生设备唤醒；截止唤醒在其后 8 500 µs。
    - 采集每 10 000 µs 在相位 φ 送来 441 帧。处理每凑满 128 帧出一个 hop，每个耗时 300 µs，出一个就产生数据唤醒。
    - 每次唤醒按 `plan_fill` 写入，headroom 为 0。
    - 每 100 µs 采样一次 padding + 环 B，取平均作为平均排队帧数。每个相位模拟 2 s。

    断言：
    - `write_on_arrival_never_starves_regular_input`：φ 取 0..10 000 µs、步长 250 µs，`deadline_mode = true` 时 `starved_read == 0`，且启动 20 ms 后补静音不超过 1 次。另加 ±500 µs 的确定性抖动（LCG 种子固定）重跑，`starved_read == 0`。
    - `write_on_arrival_lowers_the_mean_queue`：每个相位 O2 平均排队 ≤ 旧策略（`deadline_mode = false`、只有设备唤醒）+ 1 帧；所有相位平均下降 ≥ 132 帧（3 ms）。**若这条不成立，停下并报告模拟数字，不得放宽断言。**
  - `processing_wakes_render_after_each_pushed_block`：仿照 `run_and_stop`，向环 A 推 2 个 hop，`shared.render_wake.wait(100)` 为 true。
  - `hires_timer_fires_after_its_delay`（内核计时器，不是音频设备）：`arm_in_us(2_000)` 后 50 ms 内收到信号，耗时 ≥ 1 500 µs；`HiResTimer::new()` 失败的系统上跳过断言并打印原因。
  - `plan_fill` 加进 `per_block_helpers_do_not_allocate`。
  - 现有渲染测试改用 `fill_at(Wake::Device, now)`，`FakeSink::event` 返回一个测试用 `OwnedEvent` 的句柄。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine render::tests::` → 编译失败。
- [ ] **Step 3: 实现** vendored wasapi 的 `Handle::as_raw`（在 `PATCHES.md` 补一节：用途为让 devocal-engine 同时等待多个句柄，只增不改）、`HiResTimer`、`Shared.render_wake`、处理线程的 `set()`、渲染循环与 `plan_fill`，以及诊断字段 `r_data`（数据唤醒次数）、`r_deadline`（截止唤醒次数）。在模块文档里写明新的唤醒与补静音规则。
- [ ] **Step 4: 运行。** `cargo test --workspace` → PASS；另外在 `src-tauri` 下 `cargo check` 通过（vendored wasapi 也被应用使用）。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/src/audio/render.rs devocal/engine/src/audio/mod.rs devocal/engine/src/audio/processing.rs src-tauri/vendor/wasapi/src/api.rs src-tauri/vendor/wasapi/PATCHES.md
  git commit -m "devocal engine: render writes on arrival, pads only at the read deadline (O2)"
  ```

### Task 8: R2 接管台阶的发布与参考录音等待

**Files:**
- Modify: `devocal/engine/src/holder.rs`（`AttachRamp`、`attach_ramp`、`attach_epoch`、`ramp_disturbed`；`ramp_value` 改为 `pub(crate)`；测试）
- Modify: `devocal/engine/src/audio/mod.rs`（`SharedGains::{set_attach, attach}`；测试）
- Modify: `devocal/engine/src/engine.rs`（每个 tick 发布；`pending_attach` 等参考录音；`AudioPort::input_frames`；`FakeAudio`；测试）

**Interfaces:**
- Produces（任务 9 使用）：
  ```rust
  // holder.rs
  pub const CONFIRM_WINDOW_US: u64 = 150_000;   // 设计稿：判定超时 150 ms
  #[derive(Debug, Clone, Copy, PartialEq)]
  pub struct AttachRamp { pub epoch: u32, pub steps: u32 /* 已下发的台阶 1..=4 */, pub original: f32 }
  impl<S: SessionVolumes> Holder<S> { pub fn attach_ramp(&self, now_us: u64) -> Option<AttachRamp>; }
  pub(crate) fn ramp_value(from: f32, to: f32, step: u32) -> f32;   // 现有函数，仅改可见性
  // audio/mod.rs
  impl SharedGains { pub fn set_attach(&self, r: Option<AttachRamp>); pub fn attach(&self) -> Option<AttachRamp>; }
  // engine.rs
  pub const ATTACH_REF_FRAMES: u64 = 882;        // 20 ms 接管前录音作参考
  pub const ATTACH_REF_WAIT_US: u64 = 100_000;
  // AudioPort 新增：fn input_frames(&self) -> u64;  // RealAudio = diag.capture_frames
  ```
- `attach_ramp` 返回 `Some` 必须同时满足：
  - stage 为 `Attaching` 或 `Held`，`ramp_step ≥ 1`，`now_us < ramp_start_us + CONFIRM_WINDOW_US`；
  - `owned` 非空且每个都参与台阶（`ramp == true`，没有直接接管的会话）；
  - `attach_failure` 为空，`!attach_unread`，`device_mute` 为空；
  - 窗口内没有调用过 `follow`（`follow` 一进入就置 `ramp_disturbed`）。

  返回时 `steps = ramp_step`，`original = self.original`，`epoch` 在每次 `begin_attach` 时加 1。
- `SharedGains` 发布顺序：先写 original，再写打包字（有效位 + epoch + steps，Release）。读取顺序：读字 → 读 original → 再读字，两次字不同就返回 `None`。original 非有限或 ≤ 0 时返回 `None`。
- 引擎：
  - `tick` 在现有 `gains.set(..)` 之后调用 `self.gains.set_attach(self.holder.attach_ramp(now_us))`。
  - `pending_attach` 记录音频启动时刻 `(pid, created_at, started_us)`。`start_pending_attach` 只有在 `audio.input_frames() ≥ ATTACH_REF_FRAMES` 或 `now_us ≥ started_us + ATTACH_REF_WAIT_US` 时才调用 `begin_attach`。这段等待期间播放器仍是原音量，我们的输出增益为 0，听不出来。
  - 更新 `engine.rs` 模块文档中“Attach order”一段。
  - `FakeAudio` 的 `input_frames` 默认为 `u64::MAX`，现有测试行为不变。

- [ ] **Step 1: 写失败测试**
  - holder：
    - `attach_ramp_reports_issued_steps_during_the_window`：会话原音量 0.5，`begin_attach(T0)`。`tick(T0)` 后 `attach_ramp(T0) == Some{steps: 1, original: 0.5, epoch: e}`；T0+10 ms → steps 2；T0+30 ms（Held）→ steps 4；T0+149.999 ms → `Some`；T0+150 ms → `None`。
    - `attach_ramp_is_none_when_degraded`：分别构造以下情形，每种都断言为 `None`：读不到音量的会话（`attach_unread`）；直接接管的会话（复用 `attach_adopts_a_still_held_session_from_a_kept_entry` 的场景）；台阶中 `set_volume` 失败；窗口内 `default_device_changed`；窗口内调用 `follow`。
    - `attach_ramp_epoch_increments_per_attach`：接管、释放、再接管，epoch 加 1。
  - mod.rs：`attach_ramp_round_trips_through_shared_gains`：写 `Some` 读回相同；写 `None` 读回 `None`；original 为 NaN 时读回 `None`。
  - engine：
    - `attach_waits_for_pre_attach_audio`（Review Focus 1）：`input_frames = 0`，发 attach 后 tick 5 次（每次 1 ms），会话音量仍为原值；设 `input_frames = 882` 后下一个 tick 开始压低。
    - `attach_proceeds_after_100ms_without_audio`：`input_frames` 一直为 0，启动后 99 ms 未压低、100 ms 时开始压低。
    - `published_attach_ramp_follows_the_holder`：接管后发布的 `gains.attach()` 的 steps 依次为 1..4，150 ms 后为 `None`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine holder::tests::attach_ramp engine::tests::attach_` → 编译失败。
- [ ] **Step 3: 实现** 上述接口与引擎改动。
- [ ] **Step 4: 运行。** `cargo test --workspace` → PASS（现有 holder 与 engine 测试全部保持通过）。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/src/holder.rs devocal/engine/src/audio/mod.rs devocal/engine/src/engine.rs
  git commit -m "devocal engine: publish the attach volume steps; collect reference audio before the ramp (R2)"
  ```

### Task 9: R2 采集侧电平确认（`LevelConfirm`）

**Files:**
- Create: `devocal/engine/src/audio/confirm.rs`（并在 `audio/mod.rs` 中 `mod confirm;`）
- Modify: `devocal/engine/src/audio/capture.rs`（`Conditioner::condition_with`；每包调用 `LevelConfirm`）
- Modify: `devocal/engine/src/audio/mod.rs`（`DiagCounters::{capture_confirm_chunks, capture_fallback_chunks}`，诊断字段 `r2_ok`、`r2_fb`；不分配测试）

**Interfaces:**
- Consumes：`AttachRamp`、`SharedGains::attach()`、`holder::ramp_value`、`HELD_VOLUME`、`GainHistory`、`GAIN_WINDOW_US`（测试中复现保守增益）。
- Produces：
  ```rust
  pub const CONFIRM_FRAMES: usize = 110;          // 判定单位约 2.5 ms
  pub const CONFIRM_TOLERANCE_DB: f32 = 6.0;      // 设计稿
  pub const CONFIRM_TIMEOUT_US: u64 = 150_000;    // 设计稿
  pub const CONFIRM_REF_CHUNKS: usize = 20;       // 参考 = 接管前最近 50 ms
  pub const CONFIRM_MIN_REF_CHUNKS: usize = 8;    // 至少 20 ms
  pub const CONFIRM_SILENCE_RMS: f32 = 1.0e-3;    // −60 dBFS 以下视为静音
  pub(crate) fn match_step(rel_db: f32, original: f32, issued: u32, tol_db: f32) -> Option<u32>;
  pub(crate) struct LevelConfirm { /* 定长数组，无堆分配 */ }
  impl LevelConfirm {
      pub fn new() -> Self;
      pub fn observe(&mut self, ramp: Option<AttachRamp>, now_us: u64);              // 每包一次
      pub fn chunk_gain(&mut self, raw: &[f32], conservative: f32) -> f32;           // raw：未乘增益的交错立体声，≤ CONFIRM_FRAMES 帧
      pub fn take_counts(&mut self) -> (u64 /*confirmed*/, u64 /*fallback*/);
  }
  // capture.rs
  impl Conditioner {
      pub fn condition_with<F: FnMut(&[f32]) -> f32>(&mut self, samples: &mut [f32], gain_for: F,
          stats: &AudioStats, follow_now: &AtomicBool, gap_at: impl FnMut(usize));
      // condition(samples, gain, ..) 保留，内部调用 condition_with(samples, |_| gain, ..)
  }
  ```
- 算法：
  - `match_step`：设 `exp_j = 20·log10(ramp_value(original, HELD_VOLUME, j) / original)`，j ∈ 0..=issued（j = 0 即 0 dB，未衰减）。返回满足 `|rel_db − exp_j| ≤ tol_db` 的**最小** j（音量最大，增益最小），都不满足则返回 `None`。
  - `observe`：
    - `ramp == None`：窗口不活动（若正在窗口中则结束窗口），继续往参考环里记各小块的均方。
    - 出现新 epoch 且 `steps ≥ 1`：冻结参考，取参考环的均方平均 `ref_e` 与样本绝对值最大值 `ref_peak`（参考环每块记均方与峰值）；开始窗口，记 `t0 = now`。参考块数 < `CONFIRM_MIN_REF_CHUNKS`，或参考 RMS < `CONFIRM_SILENCE_RMS` 时，本 epoch 只走保守增益。
    - `now − t0 ≥ CONFIRM_TIMEOUT_US`：窗口结束。
  - `chunk_gain`：
    - 窗口外：记参考（仅在 `ramp == None` 时），返回 `conservative`。
    - 窗口内：
      - `conservative == 0` 时返回 0（设备切换静音优先）；
      - 小块均方 e == 0 时返回 `conservative`；
      - 否则 `rel_db = 10·log10(e / ref_e)`，`j = match_step(..)`；
      - 匹配到 j：`g = original / ramp_value(original, HELD, j)`，`rms_cap = sqrt(ref_e / e)`，`peak_cap = ref_peak / 小块峰值`，返回 `max(conservative, min(g, rms_cap, peak_cap))`，计一次 confirmed。`peak_cap` 防止跨台阶小块里少数响样本被放大后触发 1.5 的安全检查（那会整块静音，造成 10 ms 空洞）；
      - 没匹配到：返回 `conservative`，计一次 fallback。
  - 采集线程每包：`confirm.observe(gains.attach(), now)`；`conservative = gains.capture_gain()`。`conditioner.condition_with(pkt, |raw| confirm.chunk_gain(raw, conservative), ..)`：按 `CONFIRM_FRAMES` 逐块乘增益，然后照旧按 441 帧做安全检查。之后把 `take_counts()` 累加进诊断计数器。
- “永远不比原音量响”在 R2 中的含义：被 R2 抬高（增益 > 保守值）的每个小块，输出 RMS 不超过接管前的参考 RMS，输出峰值不超过参考峰值；其余小块等于原保守增益的输出，其上界由现有 `GainHistory` 测试保证。

- [ ] **Step 1: 写失败测试**（`confirm.rs`、`capture.rs`）
  - `match_step_picks_the_issued_step_within_6db`：original 0.5（每台阶约 −18.49 dB）：`match_step(−18.0, 0.5, 2, 6.0) == Some(1)`；`(−37.0, 0.5, 2) == Some(2)`；`(−37.0, 0.5, 1) == None`（台阶 2 未下发）；`(−28.0, 0.5, 2) == None`；`(0.0, 0.5, 2) == Some(0)`。
  - `overlapping_steps_pick_the_largest_volume`（Review Focus 3）：original 0.001（每台阶约 −5 dB）：`(−5.0, 0.001, 4) == Some(0)`；`(−9.0, 0.001, 4) == Some(1)`。
  - `confirmed_chunk_gets_the_step_gain_capped_at_reference`：参考与测试块都用幅度恒定的 ±a 交替信号（峰值等于 RMS），参考 a = 0.1；在 steps 2 下，喂一块正好低 −36.99 dB 的块 → 增益 = 0.5 / v_2；再喂一块 −31 dB（仍在容差内）的块 → 增益 = `sqrt(ref_e / e)`（小于 0.5 / v_2）。
  - `a_few_loud_samples_are_capped_by_the_reference_peak`：参考同上；块整体 −36.99 dB，但其中 3 个样本高 18 dB → 返回的增益使这 3 个样本 ≤ 0.1。
  - `silent_reference_falls_back`、`short_reference_falls_back`（Review Focus 1）：参考 RMS 5e-4，或参考只有 7 块 → 整个窗口都返回 `conservative`。
  - `timeout_after_150ms_falls_back`：`t0 + 150 ms` 后的块返回 `conservative`。
  - `mismatch_falls_back_per_chunk`：下一块不匹配时返回 `conservative`；再下一块匹配时又能确认。
  - `device_mute_wins_over_confirmation`（Review Focus 2）：`conservative == 0` 时，匹配的块也返回 0。
  - 不变量模拟。场景：
    - 真实音量按 `ramp_value` 在 t0、+10、+20、+30 ms 下发，经交付延迟 d 后体现在录音里；
    - 台阶在下发后 1 ms 发布给 `observe`；
    - 每 10 ms 一包、441 帧，相位 φ；
    - 保守增益用 `GainHistory::new(GAIN_WINDOW_US)` 按 holder 的方式记录。

    断言：
    - `never_louder_than_the_reference_during_any_attach`：
      - 参数网格：original ∈ {1.0, 0.5, 0.3, 0.001}；d ∈ {0, 5, 12, 16, 26} ms；φ ∈ 0..9 ms，步长 1 ms；
      - 源：平稳白噪声（RMS 0.1）、每 2.5 ms 随机 ±10 dB 起伏的“动态”噪声，以及双会话混合（原音量 1.0 与 0.3，各自按自己的台阶下降，`original` 取 0.3）；
      - 结论：每个增益 > 保守值的小块，输出 RMS ≤ 参考 RMS × 1.0001，且输出峰值 ≤ 参考峰值 × 1.0001。
    - `stationary_attach_leaves_no_hole_longer_than_10ms`：
      - 平稳噪声，original ∈ {1.0, 0.3}，d ∈ {12, 16, 26} ms，φ 同上；
      - 在 [t0, t0 + 200 ms] 内，低于参考 30 dB 以上的输出小块，最长连续不超过 4 块（10 ms）；
      - 对照组：同样输入只用保守增益时，最长连续 ≥ 32 块（80 ms），证明改进确实来自 R2。
  - `conditioner_applies_a_gain_per_confirm_chunk`：441 帧包，`gain_for` 依次返回 1、2、3、4、5，样本依次被乘以对应值（最后一块 1 帧）；安全检查仍按 441 帧判定。现有 `Conditioner` 测试保持通过。
  - `LevelConfirm::{observe, chunk_gain}` 与 `condition_with` 加进 `per_block_helpers_do_not_allocate`。
- [ ] **Step 2: 运行，确认失败。** `cargo test -p devocal-engine audio::confirm` → 编译失败。
- [ ] **Step 3: 实现** `confirm.rs`、`condition_with`、采集线程接线与诊断字段 `r2_ok`、`r2_fb`；在 `capture.rs` 模块文档中写明 R2 规则与退回条件。
- [ ] **Step 4: 运行。** `cargo test --workspace` → PASS。
- [ ] **Step 5: 提交。** 确认分支后：
  ```bash
  git add devocal/engine/src/audio/confirm.rs devocal/engine/src/audio/capture.rs devocal/engine/src/audio/mod.rs
  git commit -m "devocal engine: confirm the attach capture gain by level, per 2.5 ms chunk (R2)"
  ```

### Task 10: P1 真机验收（手动，每项需用户授权）

**Files:**
- Create（不入库）：`E:\autotune helper\work\devocal-latency-p1-<日期>\{scope.md,timeline.md,evidence\,evidence-hashes.txt}`
- Create（不入库）：`E:\autotune helper\docs\<日期>_devocal-latency-p1-acceptance-report.md`；Modify：`E:\autotune helper\docs\README.md`
- 可能 Modify：`devocal/engine/src/audio/render.rs`（仅当 Step 3 触发重新校准时，按任务 5 的规则与新数字执行）

**Interfaces:**
- Consumes：`docs/devocal-latency-measurement.md`（任务 4），任务 5–9 的构建；诊断字段 `r_decay`、`r_data`、`r_deadline`、`r2_ok`、`r2_fb`、`guard`。

- [ ] **Step 1: 征得同意（停在这里等用户）。** 列出将要做的每一项（下面 Step 2–6），每项单独确认。核验播放器身份，记入 `scope.md`。未获同意的项记为“未授权”，不做。
- [ ] **Step 2: 稳态延迟。** 按手册，(a)(b)(c) 各 5 次释放跳变，报中位数与最大值。

  对照标准档：直通 (a)(b) ≤ 55 ms，去人声 (c) ≤ 58 ms。判定先按中位数，同时列出最大值。(b) 与 (a) 的差应 ≤ 2 ms（验证 O1）。另外记录 `headroom` 在开关去人声 5 s 后是否回到 0（O1）。
- [ ] **Step 3: 自估核对。** 若任一状态的引擎自估中位数与实测中位数相差 > 3 ms，用本次数字按任务 5 的规则重新校准（单独提交，提交前确认分支），并在报告里写明。
- [ ] **Step 4: 重新接管。** 录覆盖一次接管的音频（`latency_probe --dump`，接管前后各 ≥ 5 s）。期望：
  - `attachLatencyMs` 测出的重复长度 ≤ D（本次实测延迟）；
  - 输出断音计数（10 ms 块比邻域低 30 dB 以上）显示空洞 ≤ 10 ms（即不超过 1 块）；
  - 该次运行 `guard == 0`，并记录 `r2_ok`、`r2_fb`。

  覆盖两种情况：从播放中接管；从暂停后接管。
- [ ] **Step 5: 回归。**
  - 直通、去人声各连续 60 s 0 断音；`loadRatio < 0.7`；记录引擎 CPU 占用。
  - 复测 2026-10-03 验收报告中的缺陷 A、B、D、E：切换设备、暂停与继续、播放器退出与重开、强杀引擎。每项单独授权。
- [ ] **Step 6: 用户试听。** 请用户评价接管、释放、切换的听感，如实记录原话，不用代理指标代替。
- [ ] **Step 7: 报告与收尾。**
  - 报告：每项结果、证据哈希、未通过或未授权项。未达标的项写明差距，并说明 P2 / P3 预期能补多少（引用设计稿）。
  - 收尾：释放播放器；确认播放器音量回到原值、恢复文件不存在；确认未处于接管状态后再关闭测试应用。
