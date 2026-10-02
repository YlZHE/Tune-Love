# 实时去人声子项目 1（接管与回放主干）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增独立进程 `devocal-engine.exe`，接管 Windows 媒体会话识别到的播放器的声音，以原样直通或 StemgenRT 去人声的方式由本程序重新输出；应用通过命名管道控制它，并保证任何异常下播放器音量都能恢复。

**Architecture:** 三个 Rust 包：
- `devocal/core`：协议、恢复文件、会话音量控制，应用与引擎共用；
- `devocal/engine`：音频引擎，可执行程序；
- `src-tauri` 的 `devocal` 模块：引擎管理与命令。

引擎内部分为纯逻辑层和薄的 Windows I/O 层。纯逻辑层包括 DSP、处理器、会话保持、状态机，都可用假对象单测。I/O 层包括录音、输出、管道，只做真机验证。前端把“去人声”预览按钮改成真实开关，并在设置页加“释放播放器”。

**Tech Stack:**
- Rust 2021；`wasapi` 0.24（`src-tauri/vendor/wasapi`，按进程录音）；`windows` 0.62.2；
- `ort` 2.x（ONNX Runtime，CPU）；`rtrb`（单生产者单消费者无锁环形队列）；`serde`/`serde_json`；
- Tauri 2；React 19 + TypeScript；vitest；Playwright。

**Spec:** [2026-10-02-devocal-engine-design.md](../specs/2026-10-02-devocal-engine-design.md)。执行者必须同时读设计稿和本计划。

## Global Constraints

- 不得为了换取前瞻而整体延迟播放；处理延迟目标为：直通 ≤ 40 ms，去人声（StemgenRT）≤ 50 ms。
- 不用驱动、注入、loopMIDI、播放器插件或针对某个宿主的脚本；任何通过 Windows 媒体会话识别到的播放器都走同一条路径。
- 被接管会话的音量固定为 `HELD_VOLUME = 1.0e-4`（−80 dB）；判断“仍为 1e-4”时允许 `|v − 1e-4| ≤ 1e-6`。
- 只在 CPU 上推理，`set_model.device` 第一版只接受 `"cpu"`，`intra_op_threads` 默认为 1；不得跑满 CPU。
- 内部采样率等于模型采样率（StemgenRT 44 100 Hz）；录音与输出都以共享模式、`autoconvert` 打开，引擎内部不自行重采样。唯一的例外见任务 8。
- 所有音频线程都申请 MMCSS “Pro Audio”优先级。
- 交叉淡化 20 ms，等功率；接管与释放的音量分 4 步，约 30 ms；断音时输出静音，前后各做 5 ms 淡化。
- 过载判据：最近 1 秒推理耗时的平均值超过块时长的 70%，或者输入在流动时出现断音。一旦过载，退回直通，原因报告为 `overload`，不自动重试。
- 会话与设备每 0.5 秒检查一次。
- 恢复文件：`app_local_data_dir/devocal-restore.json`。
  - 必须原子写入：先写临时文件，再改名。
  - 恢复时必须同时满足两道保护：进程号与创建时间一致；当前音量仍为 1e-4。
- 引擎重启上限：60 秒内最多 3 次；超过后停在原声并提示。
- 管道：
  - 名称为 `\\.\pipe\autotune-helper-devocal-<应用进程号>`；ACL 只允许当前用户；
  - 一行一条 JSON，每条都带 `"protocol": 1`。
- StemgenRT 权重不打包、不分发。开发阶段的路径为 `artifacts/separation-bench/models/hop128.onnx`，SHA-256 为 `77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9`。
- 界面文案一律中文，见任务 13 的固定文案表。
- 真机测试（任务 8 的验证步骤和任务 14）**每次执行前都要征得用户同意**，只用 Folia；执行前核验进程身份；不保存、也不改动用户的播放器设置以外的任何东西。
- 本项目目录**不是 git 仓库**。
  - 每个任务的最后一步是“检查点”：运行该任务列出的全部测试并记录结果。
  - 如果用户在执行前同意 `git init`，检查点改为提交。

## Review Focus

以下输入或失败方式设计稿没有写出，但最容易让用户碰上。每条都已在对应任务里加了测试。

1. **接管前播放器音量不是 100%**（例如用户把 Folia 调在 30%）。
   - 期望：接管前后响度不变，识调电平也连续。
   - 做法：
     - 还原倍数取 `original_volume / HELD_VOLUME`，不是固定的 1e4；
     - 上报的 `attenuation` 取 `HELD_VOLUME / original_volume`；
     - 原音量 ≤ 1e-4 的会话不去压低。
   - 测试归属：任务 7。
2. **接管或释放的音量阶梯期间，录到的数据衰减量未知。** 按调好之后的倍数放大会出现最高 +80 dB 的爆音。
   - 期望：绝不比原声更响。
   - 做法：还原倍数取保守值 `original / max(最近 100 ms 内设过的音量)`。
   - 测试归属：任务 4、任务 7。
3. **播放器暂停或长时间静音时，录音不再送数据。**
   - 期望：输出静音，但不算断音，也不触发过载退回。
   - 做法：50 ms 没有收到录音数据就算“输入空闲”，空闲期间缺数据不计为断音。
   - 测试归属：任务 6。
4. **接管时播放器还没创建音频会话**（刚打开、尚未播放）。
   - 期望：接管照常成功；会话出现后，0.5 秒检查时把它压低，并先写入恢复文件。
   - 测试归属：任务 7。
5. **快速连点去人声开关**：在预热或交叉淡化期间反复切换。
   - 期望：不出现跳变爆音，最终模式等于最后一次请求。
   - 测试归属：任务 6。

---

## 文件结构

```text
now-playing/
  devocal/
    Cargo.toml                    # workspace: core, engine
    core/
      Cargo.toml
      src/lib.rs                  # pub mod protocol, restore, sessions; pub const HELD_VOLUME
      src/protocol.rs             # 命令/事件类型，编码/解码，版本检查
      src/restore.rs              # 恢复文件：原子写、读、带双保护的恢复
      src/sessions.rs             # trait SessionVolumes + 测试用 FakeSessions
      src/sessions_win.rs         # WinSessions：枚举进程树会话、读写音量（cfg(windows)）
      examples/sessions_probe.rs  # 只读列出某进程树的会话（真机只读核对）
    engine/
      Cargo.toml
      src/main.rs                 # 参数解析，启动 engine::run
      src/dsp.rs                  # DelayLine、等功率曲线、Crossfade、GainHistory、边缘淡化
      src/load.rs                 # LoadMonitor（推理负载）
      src/separator.rs            # trait Separator、DelayOnly
      src/stemgen.rs              # StemgenRt（ort）
      src/processor.rs            # Processor：直通/预热/淡入/去人声/淡出/退回
      src/holder.rs               # Holder：压低/跟随/释放会话，恢复文件，还原倍数
      src/state.rs                # 引擎相位状态机（纯函数）
      src/audio/mod.rs            # 线程编排，环形队列，MMCSS
      src/audio/capture.rs        # 按进程录音线程
      src/audio/render.rs         # 输出线程（IAudioClient3 或 autoconvert）
      src/audio/endpoint.rs       # 找播放器会话所在端点
      src/pipe.rs                 # 命名管道服务端（ACL、对端进程校验、按行读写）
      src/engine.rs               # 命令分发、每秒指标、看门狗
      tests/fixtures/README.md    # 参考数据的生成方式
      tools/stemgen_reference.py  # 用 Python 生成 StemgenRT 参考输出
      examples/latency_probe.rs   # 真机延迟与断音测量
  src-tauri/
    Cargo.toml                    # + devocal-core 路径依赖
    tauri.conf.json               # + bundle.externalBin
    binaries/                     # 构建脚本放引擎 exe（不入库）
    src/lib.rs                    # 注册 DevocalState、启动时恢复、退出时关闭引擎
    src/audio/mod.rs              # AudioState 读取 AttenuationGate
    src/devocal/mod.rs            # DevocalState、DevocalStatus、两个命令、模型路径
    src/devocal/gate.rs           # AttenuationGate
    src/devocal/link.rs           # trait EngineLink + ProcessLink（启动子进程、管道客户端）
    src/devocal/supervisor.rs     # Supervisor：期望状态、重启预算、崩溃恢复、换播放器
  scripts/build-devocal-engine.mjs
  src/devocal.ts                  # 状态类型 + devocalLabel()
  src/devocal.test.ts
  src/useDevocal.ts
  src/components/PlayerControls.tsx   # 改为真实开关
  src/components/DevocalSettings.tsx  # 释放播放器 + 说明
  src/SettingsPage.tsx                # 挂上 DevocalSettings
  tests/devocal.spec.ts
  tests/*.spec.ts                     # 现有模拟 IPC 中补上 get_devocal_status
```

---

### Task 1: 共用协议（`devocal-core::protocol`）

**Files:**
- Create: `devocal/Cargo.toml`, `devocal/core/Cargo.toml`, `devocal/core/src/lib.rs`, `devocal/core/src/protocol.rs`
- Test: `devocal/core/src/protocol.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Produces（字段在线上用 camelCase，`cmd` 和 `event` 的取值用 snake_case）:
  ```rust
  pub const PROTOCOL: u32 = 1;
  pub enum Command { Hello { version: u32 }, Attach { pid: u32, created_at: u64 },
      SetMode { devocal: bool }, SetModel { id: String, path: PathBuf, device: String, threads: u16 },
      Release, Shutdown }                                   // #[serde(tag = "cmd")]
  pub enum Phase { Idle, Attaching, Active, Releasing }
  pub enum Mode { Passthrough, Devocal, Fallback }
  pub enum FallbackReason { Overload, ModelError }
  pub enum ErrorCode { Protocol, AttachFailed, CaptureFailed, RenderFailed, ModelLoadFailed, NoModel, BadDevice }
  pub struct Metrics { mode: Option<Mode>, latency_ms: f32, load_ratio: f32, underruns: u64,
      fallback_reason: Option<FallbackReason>, attenuation: f32, attenuation_epoch: u64,
      session_overridden: u64, input_silent_ms: u64 }
  pub enum Event { State { phase: Phase, mode: Option<Mode>, fallback_reason: Option<FallbackReason>,
      attached_pid: Option<u32> }, Metrics(Metrics), Error { code: ErrorCode, message: String } } // #[serde(tag = "event")]
  pub fn encode<T: Serialize>(body: &T) -> String;            // {"protocol":1, ...}\n
  pub fn decode_command(line: &str) -> Result<Command, ProtocolError>;
  pub fn decode_event(line: &str) -> Result<Event, ProtocolError>;
  pub enum ProtocolError { Version(u32), Malformed(String) }
  ```
- 线上格式：`{"protocol":1,"cmd":"attach","pid":1234,"createdAt":133...}`。

- [ ] **Step 1: 建 workspace 和 core 包。** `devocal/Cargo.toml` 是 `[workspace] members = ["core", "engine"]`, `resolver = "2"`（engine 目录在任务 4 创建；在那之前先只写 `core`）。core 依赖 `serde`（derive）、`serde_json`。
- [ ] **Step 2: 写失败测试**
  - `attach_round_trips_with_camel_case_fields`：
    - `encode(&Command::Attach{pid:1234, created_at:5})` 恰好等于 `{"protocol":1,"cmd":"attach","pid":1234,"createdAt":5}\n`；
    - 再 `decode_command` 回来，与原值相等。
  - `wrong_protocol_is_rejected`：`decode_command(r#"{"protocol":2,"cmd":"release"}"#)` 得到 `Err(ProtocolError::Version(2))`。
  - `missing_protocol_is_malformed`：没有 `protocol` 字段时得到 `Malformed`。
  - `unknown_command_is_malformed`。
  - `metrics_event_round_trips`：所有字段取非默认值。
- [ ] **Step 3: 运行，确认失败。** `cargo test -p devocal-core`（在 `devocal/` 下）。期望：编译错误或断言失败。
- [ ] **Step 4: 实现。** 先解析成 `serde_json::Value`，检查 `protocol` 字段，然后反序列化成枚举。
- [ ] **Step 5: 运行，确认通过。** `cargo test -p devocal-core`，期望全部 PASS。
- [ ] **Step 6: 检查点。**

### Task 2: 恢复文件（`devocal-core::restore` + `sessions` trait）

**Files:**
- Create: `devocal/core/src/restore.rs`, `devocal/core/src/sessions.rs`
- Modify: `devocal/core/src/lib.rs`
- Test: 两个文件里的 `mod tests`

**Interfaces:**
- Produces:
  ```rust
  pub const HELD_VOLUME: f32 = 1.0e-4;
  pub fn is_held(v: f32) -> bool;                            // |v - 1e-4| <= 1e-6
  pub struct SessionInfo { pub instance_id: String, pub pid: u32, pub endpoint_id: String, pub active: bool }
  pub trait SessionVolumes {
      fn sessions_for_tree(&self, root_pid: u32) -> Result<Vec<SessionInfo>, String>;
      fn session(&self, instance_id: &str) -> Result<Option<SessionInfo>, String>;
      fn volume(&self, instance_id: &str) -> Result<f32, String>;
      fn set_volume(&self, instance_id: &str, volume: f32) -> Result<(), String>;
      fn muted(&self, instance_id: &str) -> Result<bool, String>;
      fn process_created(&self, pid: u32) -> Option<u64>;    // FILETIME；进程不存在则为 None
  }
  pub struct FakeSessions { .. }   // #[cfg(any(test, feature = "fake"))]，内部 RefCell；可增删会话、读写音量、设定进程创建时间
  pub struct RestoreEntry { pub pid: u32, pub created_at: u64, pub instance_id: String,
      pub original_volume: f32, pub original_mute: bool, pub saved_at_ms: u64 }
  pub struct RestoreRecord { pub version: u32 /* =1 */, pub entries: Vec<RestoreEntry> }
  pub fn write_atomic(path: &Path, record: &RestoreRecord) -> io::Result<()>;
  pub fn read(path: &Path) -> io::Result<Option<RestoreRecord>>;   // 文件不存在 → Ok(None)
  pub struct RestoreOutcome { pub restored: usize, pub left_changed: usize, pub gone: usize, pub corrupt: bool }
  pub fn restore(path: &Path, sessions: &dyn SessionVolumes) -> RestoreOutcome;
  ```
- 恢复规则：
  - 逐条处理。进程创建时间不等于 `created_at`，或者会话已不存在时，计入 `gone`。
  - 当前音量 `is_held` 时，恢复为 `original_volume`，计入 `restored`。
  - 否则保持现状，计入 `left_changed`。
  - 全部处理完后删除文件。
  - 文件损坏时改名为 `devocal-restore.json.corrupt`，置 `corrupt = true`，不动任何音量。

- [ ] **Step 1: 写失败测试**（用 `FakeSessions` 和临时目录）
  - `write_is_atomic_and_round_trips`：
    - 写入后目录里只剩目标文件，没有残留的 `.tmp`；
    - 读回来与原值相等。
  - `restores_only_when_still_held`：两条记录，A 当前为 1e-4，B 当前为 0.5。结果：A 恢复为原值 0.8，B 仍为 0.5，`restored = 1`，`left_changed = 1`，文件已删除。
  - `pid_reuse_is_ignored`：进程号相同但创建时间不同时，计入 `gone`，音量不变。
  - `missing_file_is_noop`：`restore` 返回全 0，不报错。
  - `corrupt_file_is_quarantined`：写入 `{oops` 后调用 `restore`：`corrupt = true`，出现 `.corrupt` 文件，没有任何音量被写过（`FakeSessions` 记录的写入次数为 0）。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。**
  - 临时文件名为 `<path>.tmp-<pid>`，写完后 `sync_all`，再 `std::fs::rename` 覆盖目标（Windows 上用 `MoveFileExW` 的 `REPLACE_EXISTING` 语义，`std::fs::rename` 已提供）。
  - `FakeSessions` 放在 `cfg(any(test, feature = "fake"))` 之下，在 core 的 `Cargo.toml` 里声明 `fake` feature，供 engine 和 app 的测试使用。
- [ ] **Step 4: 运行，确认通过。** 运行 `cargo test -p devocal-core`。
- [ ] **Step 5: 检查点。**

### Task 3: Windows 会话控制（`WinSessions`）

**Files:**
- Create: `devocal/core/src/sessions_win.rs`, `devocal/core/examples/sessions_probe.rs`
- Modify: `devocal/core/Cargo.toml`
  - 依赖：`windows = "0.62.2"`，开启 `Win32_Foundation`、`Win32_Media_Audio`、`Win32_System_Com`、`Win32_System_Diagnostics_ToolHelp`、`Win32_System_Threading`。

**Interfaces:**
- Produces: `pub struct WinSessions;`，`impl SessionVolumes for WinSessions`，以及 `pub fn process_tree(root: u32) -> Vec<u32>`。
- 要求调用线程已初始化 COM（MTA）。

- [ ] **Step 1: 写测试**
  - `process_tree_contains_root_and_children`：纯函数部分。把 `process_tree` 拆成 `tree_from_pairs(root, &[(pid, parent)])`，测试其中的循环保护与进程号复用。
  - 复用保护：子进程的创建时间早于父进程时不算子进程。所以输入用 `(pid, parent, created)` 三元组。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。**
  - 遍历所有处于 `DEVICE_STATE_ACTIVE` 的 render 端点。对每个端点：
    - 取 `IAudioSessionManager2::GetSessionEnumerator`；
    - 用 `IAudioSessionControl2::GetProcessId` 过滤出属于进程树的会话；
    - 记录 `GetSessionInstanceIdentifier`、端点 ID，以及 `GetState() == AudioSessionStateActive`。
  - 读写音量用 `ISimpleAudioVolume`，即 `GetMasterVolume`、`SetMasterVolume(v, null)`、`GetMute`。
  - `process_created` 用 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` 加 `GetProcessTimes`，与 `src-tauri/src/audio/process.rs` 的做法相同。
  - 示例 `sessions_probe <pid>` 只读打印会话列表，不改音量。
- [ ] **Step 4: 运行。** `cargo test -p devocal-core`，期望 PASS。再运行 `cargo run -p devocal-core --example sessions_probe -- <Folia 的 pid>`，这是只读操作。期望至少列出 1 个会话，带端点 ID 与当前音量。
- [ ] **Step 5: 检查点。**

### Task 4: 引擎 DSP 基础件（`dsp.rs`、`load.rs`）

**Files:**
- Create: `devocal/engine/Cargo.toml`, `devocal/engine/src/main.rs`（暂时只有 `fn main() {}` 和 `mod` 声明）, `devocal/engine/src/dsp.rs`, `devocal/engine/src/load.rs`
- Modify: `devocal/Cargo.toml`（members 加上 `engine`）

**Interfaces:**
- Produces（采样一律为交织立体声 `f32`，时间一律为微秒 `u64`）:
  ```rust
  pub const SAMPLE_RATE: u32 = 44_100;
  pub fn frames_for_ms(ms: f32) -> usize;                       // 按 SAMPLE_RATE 四舍五入；20 ms → 882
  pub struct DelayLine; impl DelayLine { pub fn new(frames: usize) -> Self;
      pub fn process(&mut self, inout: &mut [f32]); pub fn reset(&mut self); }
  pub fn equal_power(t: f32) -> (f32, f32);                     // (out, in) = (cos(πt/2), sin(πt/2))，t 截到 [0,1]
  pub struct Crossfade; impl Crossfade { pub fn new(frames: usize) -> Self; pub fn start(&mut self);
      pub fn mix(&mut self, from: &[f32], to: &[f32], out: &mut [f32]) -> bool /* 完成 */; }
  pub struct GainHistory; impl GainHistory { pub fn new(window_us: u64 /* 100_000 */) -> Self;
      pub fn set(&mut self, at_us: u64, volume: f32);
      pub fn conservative_gain(&self, original: f32, now_us: u64) -> f32; } // original / max(窗口内设过的音量)
  pub fn fade_edges(block: &mut [f32], fade_in: bool, fade_out: bool, fade_frames: usize); // 5 ms 断音淡化
  pub struct LoadMonitor; impl LoadMonitor { pub fn new(block_us: f64, window_blocks: usize /* 344 */, threshold: f64 /* 0.70 */) -> Self;
      pub fn record(&mut self, elapsed_us: f64); pub fn ratio(&self) -> f64; pub fn overloaded(&self) -> bool; pub fn reset(&mut self); }
  ```

- [ ] **Step 1: 写失败测试**
  - `delay_line_shifts_by_exact_frames`：延迟 128 帧时，输入第 0 帧的冲激出现在输出第 128 帧；左右声道不串。
  - `equal_power_sums_to_unit_power`：`t ∈ {0, .25, .5, 1}` 时 `out² + in² = 1 ± 1e-6`；`t = 0` 为 `(1, 0)`；`t = 1` 为 `(0, 1)`。
  - `crossfade_takes_882_frames_and_reports_done`：两路常数输入 1.0 与 0.0，第 881 帧之前 `mix` 返回 false，跨过 882 帧后返回 true；之后输出全等于 `to`。
  - `gain_history_never_exceeds_true_gain_while_ramping_down`：
    - 分别在 t = 0、10、20、30 ms 设 0.5、0.05、0.005、1e-4，原音量取 0.5。
    - 在 t = 15 ms 时，增益等于 `0.5/0.5 = 1.0`（窗口内最大值是 0.5）。
    - 在 t = 131 ms 时，增益等于 `0.5/1e-4 = 5000`。
  - `gain_history_is_conservative_when_ramping_up`：在 t = 0 设 1e-4，在 t = 10 ms 设 0.5。t = 12 ms 时增益为 1.0，不是 5000。
  - `fade_edges_ramps_over_220_frames`：5 ms 等于 220.5 帧，四舍五入为 221 帧。第一帧为 0，第 221 帧为 1。
  - `load_monitor_trips_above_70_percent`：块时长 2902 µs。连续记 344 次 2100 µs 时 `overloaded()` 为 true；连续记 344 次 1900 µs 时为 false。
- [ ] **Step 2: 运行，确认失败。** 在 `devocal/` 下运行 `cargo test -p devocal-engine`。
- [ ] **Step 3: 实现。** `LoadMonitor` 用固定长度的环形缓冲求滑动平均，不足 344 个样本时按已有的样本平均。
- [ ] **Step 4: 运行，确认通过。**
- [ ] **Step 5: 检查点。**

### Task 5: 模型接口与 StemgenRT（`separator.rs`、`stemgen.rs`）

**Files:**
- Create: `devocal/engine/src/separator.rs`, `devocal/engine/src/stemgen.rs`, `devocal/engine/tools/stemgen_reference.py`, `devocal/engine/tests/fixtures/README.md`
- Modify: `devocal/engine/Cargo.toml`
  - 依赖 `ort` 2.x 当前最新正式版，CPU，用 `download-binaries` 静态链接；把所用版本记进 `Cargo.toml` 注释。

**Interfaces:**
- Produces:
  ```rust
  pub trait Separator: Send {
      fn sample_rate(&self) -> u32; fn hop(&self) -> usize; fn latency_frames(&self) -> usize;
      fn process(&mut self, input: &[f32], out_accompaniment: &mut [f32]) -> Result<(), String>; // 长度 = hop*2
      fn reset(&mut self);
  }
  pub struct DelayOnly { .. }  // 测试与直通对齐用：输出 = 输入延迟 latency_frames
  pub struct StemgenRt { .. }
  impl StemgenRt { pub fn load(path: &Path, threads: u16) -> Result<Self, String>; }
  // StemgenRt: sample_rate 44100, hop 128, latency_frames 128
  ```
- 与设计稿的差异：`process` 返回 `Result`，这样推理出错时，处理器可以按 `model_error` 退回。
- 已核实的模型事实，来源为 `artifacts/separation-bench/src/quality.py::stemgenrt` 与 `models/stemgenrt_streaming_models.json`：
  - 输入：
    - `audio_chunk`，形状 `[1, 2, 128]`，为声道优先、非交织；
    - 8 个状态张量，名称和形状见 json 里的 `states`。
  - 输出：
    - `outputs[0]` 是形状为 `[1, 4, 2, 128]` 的 stems；
    - `outputs[1..]` 是新状态，**顺序与状态输入相同**。
  - 第 k 次的输出对应第 k−1 个输入块，固有延迟为 1 个 hop，即 128 帧。
  - 人声为 `stems[2]`。伴奏 = 上一个输入块 − 人声。实现里要保存上一块输入。

- [ ] **Step 1: 写参考数据生成脚本。**
  - `tools/stemgen_reference.py`：用 `artifacts/separation-bench/venv` 运行。
  - 用种子 1234 生成 2 秒的确定性立体声噪声加正弦信号（不用歌曲）。
  - 按 `quality.stemgenrt` 的方式逐块跑，输出 `fixtures/stemgen-input.f32` 和 `fixtures/stemgen-accompaniment.f32`，都是小端 float32、交织。
  - 伴奏取未裁剪的逐块输出，与 Rust 的流式输出逐样本对齐。
  - README 写明命令，以及生成时 onnxruntime 的版本（venv 里是 1.24.4）。
  - 夹具文件不入库：把 `fixtures/*.f32` 写进 `.gitignore`。
- [ ] **Step 2: 写失败测试**
  - `delay_only_matches_delay_line`：`DelayOnly::new(128)` 与 `DelayLine::new(128)` 的输出逐样本相等。
  - `stemgen_matches_python_reference`：标为 `#[ignore]`，从环境变量 `STEMGENRT_ONNX` 读取模型路径。逐块喂入夹具，`max |rust − python| ≤ 1e-4`。
  - `stemgen_reset_zeroes_state`：标为 `#[ignore]`。`reset()` 后重新喂同一段输入，输出与第一次运行逐样本相等。
- [ ] **Step 3: 运行，确认失败。**
  - 先在 `devocal/engine` 下运行 `artifacts/separation-bench/venv/Scripts/python tools/stemgen_reference.py` 生成夹具。
  - 再运行 `STEMGENRT_ONNX=<path> cargo test -p devocal-engine stemgen -- --ignored`，期望 FAIL。
- [ ] **Step 4: 实现。**
  - 状态张量用预分配的 `Vec<f32>`，每块交换新旧状态，不在热路径上分配内存。
  - 交织与非交织的转换在这里完成。
  - `load` 时设 `intra_op_threads = threads`、`inter_op_threads = 1`，然后先空跑 20 次预热。
- [ ] **Step 5: 运行，确认通过。** 运行 `cargo test -p devocal-engine` 和带 `--ignored` 的那组。期望全部 PASS，并在检查点记录 `max |diff|`。
- [ ] **Step 6: 检查点。**

### Task 6: 处理器（`processor.rs`）

**Files:**
- Create: `devocal/engine/src/processor.rs`

**Interfaces:**
- Consumes: 任务 4 的 `DelayLine`、`Crossfade`、`fade_edges`、`LoadMonitor`；任务 5 的 `Separator`。
- Produces:
  ```rust
  pub enum Stage { Passthrough, WarmingUp, FadingIn, Devocal, FadingOut, Fallback }
  pub struct BlockReport { pub ran_model: bool, pub stage: Stage }
  pub struct Processor;
  impl Processor {
      pub fn new(separator: Option<Box<dyn Separator>>) -> Self;  // 无模型时 latency = 128 帧（与 StemgenRT 对齐）
      pub fn set_separator(&mut self, s: Box<dyn Separator>);      // 回到 Passthrough；直通延迟改为该模型的 latency_frames
      pub fn request_devocal(&mut self, on: bool);
      pub fn force_fallback(&mut self, reason: FallbackReason);
      pub fn fallback_reason(&self) -> Option<FallbackReason>;
      pub fn on_discontinuity(&mut self);                          // reset 模型与延迟线
      pub fn hop(&self) -> usize;
      pub fn process_block(&mut self, input: &[f32], output: &mut [f32]) -> BlockReport;
  }
  ```
- 阶段规则：
  - **开**：
    - `Passthrough` 或 `Fallback` 时请求打开：先 `reset()` 模型，进入 `WarmingUp`；
    - 模型与直通并行跑 200 ms（`frames_for_ms(200)` 向上取整到 hop 的整数倍），期间仍输出直通；
    - 然后进入 `FadingIn`，用 20 ms 等功率淡化从直通过渡到伴奏，最后到 `Devocal`。
  - **关**：`Devocal` 时请求关闭，进入 `FadingOut`，20 ms 后到 `Passthrough`，并停止调用模型。
  - **过载退回**：`force_fallback` 走 `FadingOut`，结束后进入 `Fallback`。
  - **推理出错**：`process` 返回 `Err` 时，当前块输出直通，并直接 `force_fallback(ModelError)`。
  - **淡化途中改向**：在 `WarmingUp`、`FadingIn` 或 `FadingOut` 期间收到相反请求时，从当前增益位置反向，不跳变。
  - `Fallback` 时只有新的 `request_devocal(true)` 或 `set_separator` 才会离开。

- [ ] **Step 1: 写失败测试**
  - 测试用的分离器：
    - `ConstSep`：伴奏恒为 0.25；
    - `FailSep`：第 n 块返回 `Err`。
  - `passthrough_is_delayed_by_model_latency`：直通输出等于输入延迟 128 帧。
  - `enable_warms_200ms_then_fades_20ms`：
    - 开启后的前 `ceil(8820/128) = 69` 块全为直通（输出 = 延迟后的输入），且 `ran_model = true`；
    - 接下来 882 帧单调过渡；
    - 再之后的输出恒为 0.25。
  - `disable_fades_then_stops_running_model`：淡出完成后，`ran_model = false`。
  - `model_error_falls_back`：`FailSep` 出错后进入 `Fallback`，原因是 `ModelError`；出错那块的输出等于直通，没有 NaN。
  - `rapid_toggle_never_jumps`（对应 Review Focus 第 5 条）：
    - 交替发送 开、关、开、关、开 五次请求，彼此间隔 3 块；
    - 整个过程中相邻样本差的最大值 ≤ 单块淡化步长 × 2 + 输入本身的相邻差；
    - 最终阶段为 `Devocal`。
  - `discontinuity_resets_model`：`on_discontinuity()` 后，分离器的 `reset` 被调用过一次（用计数器验证）。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。** 直通路径始终经过 `DelayLine`，即使正在跑模型也一样，保证两路时间对齐。
- [ ] **Step 4: 运行，确认通过。**
- [ ] **Step 5: 检查点。**

### Task 7: 会话保持（`holder.rs`）与相位状态机（`state.rs`）

**Files:**
- Create: `devocal/engine/src/holder.rs`, `devocal/engine/src/state.rs`
- Modify: `devocal/engine/Cargo.toml`（dev-dependency `devocal-core`，开启 `features = ["fake"]`）

**Interfaces:**
- Consumes: 任务 2 的 `SessionVolumes`、`restore::*`、`HELD_VOLUME`；任务 4 的 `GainHistory`。
- Produces:
  ```rust
  pub struct Holder<S: SessionVolumes> { .. }
  impl<S: SessionVolumes> Holder<S> {
      pub fn new(sessions: S, restore_path: PathBuf) -> Self;
      pub fn begin_attach(&mut self, pid: u32, created_at: u64, now_us: u64) -> Result<(), String>;
      pub fn begin_release(&mut self, now_us: u64);
      pub fn tick(&mut self, now_us: u64) -> HolderPhase;   // 推进 4 步音量阶梯：0/10/20/30 ms
      pub fn follow(&mut self, now_us: u64) -> FollowReport; // 每 0.5 s：新会话、被调大的会话、进程退出
      pub fn capture_gain(&self, now_us: u64) -> f32;       // 录音线程用：conservative_gain(original, now)
      pub fn attenuation(&self) -> f32;                    // HELD_VOLUME / original；未接管为 1.0
      pub fn attenuation_epoch(&self) -> u64;              // attenuation 的值每变一次加 1
      pub fn output_gain(&self) -> f32;                    // 我们自己输出的淡入/淡出增益 0..1，与阶梯同步
  }
  pub enum HolderPhase { Idle, Attaching, Held, Releasing }
  pub struct FollowReport { pub newly_lowered: usize, pub overridden: usize, pub player_exited: bool }
  // state.rs
  pub enum Input { Attach, AttachDone, Release, ReleaseDone, Failure, PlayerExited }
  pub fn next(phase: Phase, input: Input) -> Result<Phase, Phase /* 拒绝时原样返回 */>;
  ```
- 还原倍数（对应 Review Focus 第 1 条）：
  - `original` = 接管时进程树中所有原音量 > `HELD_VOLUME` 的会话里，原音量的最大值。
  - 原音量 ≤ 1e-4 的会话不压低，也不写入恢复文件。
  - 一个会话都没有时 `original = 1.0`。
- 写入顺序：每次压低新会话之前，先 `write_atomic` 更新恢复文件，把新条目追加进去。
- 释放：
  - 4 步阶梯回到原音量。
  - 某会话已被用户改动（不是 1e-4）时，不覆盖用户的设定。
  - 完成后删除恢复文件。
- 跟随（`follow`）：
  - 进程树中新出现的会话：先写文件，再压低。
  - 已压低的会话音量变得不是 1e-4 时，重新压回，`overridden += 1`。
  - 进程已退出（`process_created` 不再等于 `created_at`）时，`player_exited = true`，并删除恢复文件。
- `next` 的合法转移：
  - `Idle + Attach → Attaching`
  - `Attaching + AttachDone → Active`
  - `Attaching | Active + Release → Releasing`
  - `Releasing + ReleaseDone → Idle`
  - 任意相位 `+ Failure → Releasing`
  - `Active + PlayerExited → Idle`
  - 其余组合一律拒绝。

- [ ] **Step 1: 写失败测试**（用 `FakeSessions` 和临时目录）
  - `attach_writes_file_before_lowering`：`FakeSessions` 记录每次写音量时恢复文件是否已存在，每次都必须为 true。
  - `ramp_reaches_held_in_four_steps_over_30ms`：
    - 在 t = 0、10、20、30 ms 分别 `tick`，音量依次为原值的几何插值，最后一步恰好是 1e-4；
    - `tick(30ms)` 后返回 `Held`。
  - `gain_matches_original_volume`（对应 Review Focus 第 1 条）：
    - 原音量 0.3，`Held` 之后 `attenuation()` 为 `1e-4/0.3`；
    - 超过 100 ms 窗口后，`capture_gain` 为 `0.3/1e-4`。
  - `gain_never_exceeds_true_during_attach`（对应 Review Focus 第 2 条）：在 0～130 ms 内每 1 ms 采样，`capture_gain(t) ≤ original / 当时实际生效的音量`，对窗口内每个音量都成立。
  - `near_zero_session_is_left_alone`：原音量 0 或 5e-5 的会话，接管后音量不变，也不在文件中。
  - `session_appearing_later_is_lowered`（对应 Review Focus 第 4 条）：
    - 接管时没有会话，`attach` 成功，返回 `Held`；
    - 随后添加一个 0.8 的会话，`follow` 返回 `newly_lowered = 1`；
    - 文件中有该条目，且写文件发生在压低之前。
  - `overridden_session_is_lowered_again`：
    - 把已压低的会话改为 0.6，`follow` 返回 `overridden = 1`；
    - 音量回到 1e-4，`attenuation_epoch` 不变。
  - `release_respects_user_change_and_deletes_file`。
  - `player_exit_reports_and_clears_file`。
  - `state_transitions_table`：用表驱动覆盖上面列出的所有合法转移，再至少测 6 个非法组合被拒绝。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。** `Holder` 不持有线程，时间全部由参数传入。
- [ ] **Step 4: 运行，确认通过。**
- [ ] **Step 5: 检查点。**

### Task 8: 引擎音频 I/O（`audio/*`）与延迟测量工具

**Files:**
- Create: `devocal/engine/src/audio/{mod,capture,render,endpoint}.rs`, `devocal/engine/examples/latency_probe.rs`
- Modify: `devocal/engine/Cargo.toml`
  - 依赖：`wasapi = { path = "../../src-tauri/vendor/wasapi" }`、`rtrb`、`windows`（与 core 相同的 features，再加 `Win32_System_Performance`）、`devocal-core`。

**Interfaces:**
- Consumes: `Processor`（任务 6）、`Holder::capture_gain` 与 `output_gain`（任务 7）、`LoadMonitor`（任务 4）。
- Produces:
  ```rust
  pub struct AudioConfig { pub pid: u32, pub sample_rate: u32, pub hop: usize }
  pub struct AudioStats { pub underruns: AtomicU64, pub input_silent_ms: AtomicU64, pub load_ratio_milli: AtomicU32,
      pub latency_ms_milli: AtomicU32, pub discontinuities: AtomicU64 }
  pub struct AudioHandle { pub stats: Arc<AudioStats>, .. }
  impl AudioHandle {
      pub fn start(cfg: AudioConfig, gains: Arc<SharedGains>, processor: Processor) -> Result<Self, String>;
      pub fn set_devocal(&self, on: bool);              // 经 rtrb 控制队列送到处理线程
      pub fn set_separator(&self, s: Box<dyn Separator>);
      pub fn rebind_output(&self, endpoint_id: Option<String>);
      pub fn output_failed(&self) -> bool;               // 输出线程遇到 AUDCLNT_E_DEVICE_INVALIDATED 等错误后为 true，重新绑定后清零
      pub fn stop(self);                                 // join 三个线程
  }
  pub struct SharedGains { pub capture_gain_bits: AtomicU32, pub output_gain_bits: AtomicU32 } // f32 位模式；由引擎主循环每 1 ms 根据 Holder 更新
  pub fn session_endpoint(sessions: &[SessionInfo]) -> Option<String>; // 优先选 active 会话的端点
  ```
- 线程：
  - **录音线程**：
    - `AudioClient::new_application_loopback_client(pid, true)`，`EventsShared{autoconvert: true}`，44.1 kHz float32 立体声；
    - 每个包乘以 `capture_gain`；遇到 `discontinuity` 或 `timestamp_error` 时发送重置标记。
  - **处理线程**：
    - 按 hop 取块，调用 `process_block`，计时后写入 `LoadMonitor`；
    - `overloaded()` 时调用 `force_fallback(Overload)`；
    - 输入在流动时（最近 50 ms 内收到过录音数据）出现断音，也同样退回（对应 Review Focus 第 3 条）。
  - **输出线程**：
    - 播放器所在端点的混音格式恰好为 44 100 Hz 时，用 `windows` 直接 `Activate` `IAudioClient3`，以 `GetSharedModeEnginePeriod` 的最小周期调用 `InitializeSharedAudioStream`；
    - 否则走 wasapi 的 `EventsShared{autoconvert: true, buffer_duration_hns: 0}`；
    - 目标排队量为 1 个周期加 1 个 hop；
    - 排队量持续 1 秒超过目标加 20 ms 时，丢弃到目标值；
    - 缺数据时输出静音，并用 `fade_edges` 淡化边缘；仅当最近 50 ms 内有录音数据时，才计一次 `underruns`。
  - 三个线程开头都调用 `AvSetMmThreadCharacteristicsW("Pro Audio")`。
- 延迟估算：`latency_ms = (录音周期 + 两个环形队列当前帧数 + 输出 padding + separator.latency_frames) / 44.1`。

- [ ] **Step 1: 写失败测试**（纯函数部分）
  - `session_endpoint_prefers_active`：两个会话位于不同端点，只有一个 active，返回 active 的那个；全都不 active 时返回第一个；为空时返回 `None`。
  - `idle_input_is_not_an_underrun`（对应 Review Focus 第 3 条）：
    - 把计数逻辑拆成纯函数 `fn count_underrun(last_input_us: u64, now_us: u64) -> bool`，阈值 50 ms；
    - 输入 49 ms 前到达时返回 true；51 ms 前到达时返回 false。
  - `trim_policy_drops_only_after_one_second_over`：拆成纯函数 `TrimPolicy::observe(queued_frames, now_us) -> Option<usize /* 丢弃量 */>`。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。** 三个线程与 `AudioHandle`；`latency_probe` 示例重写自 `work/devocal-spike-20261002/src/main.rs` 的测量方法：
  - 同时录系统输出端点 loopback 与源进程；
  - 用互相关求延迟；
  - 以 10 ms 块计断音：块能量比前后中位数低 30 dB 以上；
  - 输出 JSON。
  - 示例必须用 `--engine-pipe` 连接一个已运行的引擎，自己不压低任何会话。
- [ ] **Step 4: 运行单元测试。** `cargo test -p devocal-engine`，期望 PASS。
- [ ] **Step 5: 检查点。** 真机测量放在任务 14，这里只要求编译通过、单元测试通过。

### Task 9: 管道与引擎主循环（`pipe.rs`、`engine.rs`、`main.rs`）

**Files:**
- Create: `devocal/engine/src/pipe.rs`, `devocal/engine/src/engine.rs`
- Modify: `devocal/engine/src/main.rs`

**Interfaces:**
- Consumes: `protocol::*`；`Holder`、`state::next`（任务 7）；`AudioHandle`（任务 8）；`StemgenRt::load`（任务 5）。
- Produces:
  - 命令行：`devocal-engine.exe --app-pid <u32> --restore-file <path>`。
  - `pub fn pipe_name(app_pid: u32) -> String`：返回 `\\.\pipe\autotune-helper-devocal-<pid>`。
  - `pub struct PipeServer`：
    - `create(name)` 带三项设置：`FILE_FLAG_FIRST_PIPE_INSTANCE`、`PIPE_REJECT_REMOTE_CLIENTS`，以及 SDDL `D:P(A;;GA;;;<当前用户 SID>)`（SID 来自 `GetTokenInformation(TokenUser)` 和 `ConvertSidToStringSidW`）；
    - `accept(expected_client_pid)`：用 `GetNamedPipeClientProcessId` 校验，不符则断开并继续等待；
    - `read_line()`、`write_line()`。
  - `pub struct EngineCore<S: SessionVolumes, A: AudioPort>`，其中 `trait AudioPort { start, set_devocal, set_separator, rebind_output, output_failed, stop, stats }` 让测试能用假音频。
  - 端点跟随：`tick` 每 0.5 s 用 `session_endpoint(sessions_for_tree(pid))` 求出播放器当前所在端点。端点变了，或者 `output_failed()` 为 true（端点失效）时，调用 `rebind_output`；`None` 表示使用默认端点。
  - `pub fn handle(&mut self, cmd: Command, now_us: u64) -> Vec<Event>`。
  - `pub fn tick(&mut self, now_us: u64) -> Vec<Event>`：推进阶梯，每 0.5 s 调一次 `follow`，每 1 s 产出 `Metrics`。
- 命令处理：
  - `hello`：版本不符时回复 `Error{Protocol}`，然后执行释放流程并退出。
  - `set_model`：
    - `device != "cpu"` 时回复 `Error{BadDevice}`；
    - 加载失败时回复 `Error{ModelLoadFailed}`；
    - 加载成功后，`AudioPort::set_separator`。
  - `set_mode{devocal: true}`：没加载模型时回复 `Error{NoModel}`。
  - `attach` 失败时回复 `Error{AttachFailed}`，相位回到 `Idle`。
  - 任何 I/O 线程报告失败时，按 `Failure` 走释放流程。
- 看门狗：
  - 主循环每 1 ms 调用 `tick`；
  - 管道读到 EOF，或者 `WaitForSingleObject(app_handle, 0)` 发现应用进程已结束时，先释放（完成后恢复文件被删除），然后退出码 0；
  - 释放失败时保留恢复文件，留给应用下次启动时处理。

- [ ] **Step 1: 写失败测试**（`EngineCore` 搭配 `FakeSessions` 和 `FakeAudio`）
  - `full_lifecycle_emits_states_in_order`：依次发送 `hello`、`set_model`（假模型）、`attach`、多次 tick、`set_mode(true)`、`release`、多次 tick。`State` 事件的相位依次为 Attaching → Active(Passthrough) → Active(Devocal) → Releasing → Idle。
  - `bad_device_rejected`、`devocal_without_model_rejected`、`protocol_mismatch_releases`。
  - `metrics_once_per_second`：tick 2.5 s，恰好产出 2 条 `Metrics`。
  - `overload_reported_as_fallback`：`FakeAudio` 报告过载后，下一条 `State` 为 `Active` + `Fallback` + `Overload`，之后不会自动回到 Devocal。
  - `endpoint_change_rebinds`：`FakeSessions` 把会话移到另一个端点后，下一次 0.5 s 检查调用 `rebind_output(Some(新端点))` 恰好 1 次；`output_failed` 为 true 时也会重新绑定。
  - `pipe_name_format`。
  - `pipe_rejects_wrong_client_pid`：进程内建服务端，`expected_client_pid` 设为当前进程号加 1；本进程连上后应被断开。只用本机管道，无外部影响。
- [ ] **Step 2: 运行，确认失败。**
- [ ] **Step 3: 实现。** `main.rs` 解析参数，初始化 COM（MTA），然后运行 `engine::run(args)`。真实的 `AudioPort` 实现包装 `AudioHandle`。
- [ ] **Step 4: 运行，确认通过。**
- [ ] **Step 5: 检查点。**

### Task 10: 应用侧引擎管理（`src-tauri/src/devocal/*`）

**Files:**
- Create: `src-tauri/src/devocal/{mod,gate,link,supervisor}.rs`
- Modify:
  - `src-tauri/Cargo.toml`：依赖 `devocal-core = { path = "../devocal/core" }`；dev-dependencies 中也加上它，并开启 `fake` feature。
  - `src-tauri/src/lib.rs`

**Interfaces:**
- Consumes: `protocol::*`、`restore::restore`、`WinSessions`；`audio::process::native::resolve(source_id) -> ProcessIdentity`（取 `record.pid`、`record.created`）；`MediaState::audio_target()`。
- Produces:
  ```rust
  pub struct AttenuationGate;  // gate.rs
  impl AttenuationGate { pub fn set(&self, gain: Option<f32>); pub fn read(&self) -> (u64 /* epoch */, Option<f32>); }
  // gain = None：丢弃录到的数据；Some(g)：乘以 g。每次 set 都让 epoch 加 1。初始为 Some(1.0)。
  pub trait EngineLink { fn send(&mut self, cmd: &Command) -> Result<(), String>;
      fn try_recv(&mut self) -> Option<Event>; fn exited(&self) -> bool; fn kill(&mut self); }
  pub struct ProcessLink;  // 启动 exe；2 s 内重试连接；GetNamedPipeServerProcessId 必须等于子进程号；后台线程逐行读到 mpsc
  pub struct RestartBudget { max: 3, window_ms: 60_000 }  // fn allow(&mut self, now_ms) -> bool
  pub struct Supervisor<L: EngineLink> { .. }
  impl<L: EngineLink> Supervisor<L> {
      pub fn new(spawn: Box<dyn FnMut() -> Result<L, String> + Send>, restore: Box<dyn FnMut() + Send>, gate: Arc<AttenuationGate>) -> Self;
      pub fn enable(&mut self, model: Option<PathBuf>);  // 记下“要去人声”；model 为 None → phase=unavailable，不 spawn
      // 下一次 tick 拿到 target 时，若尚未持有：spawn、hello、set_model、attach、set_mode(true)；已持有则只发 set_mode(true)
      pub fn disable(&mut self);                                       // set_mode(false)，仍然持有
      pub fn release(&mut self);                                       // release，held = false
      pub fn tick(&mut self, now_ms: u64, target: Option<PlayerProcess>, media_playing: bool);
      pub fn status(&self) -> DevocalStatus;
      pub fn shutdown(&mut self);  // 发 shutdown，最多等 1 s；没退出就 kill，再执行 restore
  }
  pub struct PlayerProcess { pub source_id: String, pub pid: u32, pub created_at: u64 }
  #[serde(rename_all = "camelCase")]
  pub struct DevocalStatus { pub phase: &'static str /* off|attaching|passthrough|devocal|fallback|releasing|restarting|failed|unavailable */,
      pub held: bool, pub latency_ms: Option<f32>, pub load_ratio: Option<f32>, pub fallback_reason: Option<&'static str>,
      pub session_overridden: bool, pub input_silent: bool, pub error: Option<String> }
  #[tauri::command] pub fn get_devocal_status(state: State<DevocalState>) -> DevocalStatus;
  #[tauri::command] pub fn devocal_command(request: DevocalRequest /* {action: "enable"|"disable"|"release"} */, ..) -> Result<DevocalStatus, String>;
  pub fn model_path(data_dir: &Path) -> Option<PathBuf>; // 依次查 env AUTOTUNE_HELPER_STEMGENRT_ONNX、<data>/models/stemgenrt-hop128.onnx，取第一个存在的
  ```
- 闸门规则：
  - 状态为 `attaching` 或 `releasing`，或者引擎被判定异常退出时：`gate.set(None)`；
  - 收到 `Active` 状态或 `Metrics` 时：`gate.set(Some(1.0 / attenuation))`，只在 `attenuationEpoch` 变化时才 set；
  - 回到 `Idle`，或者恢复完成后：`gate.set(Some(1.0))`。
- 崩溃处理：
  - 发现 `exited()` 时，先 `restore`，再 `gate.set(Some(1.0))`；
  - 如果仍需要持有，而且 `RestartBudget.allow` 允许，就重新走一遍 enable 流程，相位为 `restarting`；
  - 否则相位为 `failed`，`error = "engine_crashed"`。
- 换播放器：持有期间，`tick` 收到的 `target.pid` 或 `created_at` 与当前不同时，先 `release`，等到 `Idle` 后再 `attach` 新的。
- 状态字段：
  - `session_overridden`：最近 10 s 内 `metrics.sessionOverridden` 增加过时为 true。
  - `input_silent`：`media_playing` 且 `inputSilentMs ≥ 3000` 时为 true。
- `lib.rs` 的改动：
  - `setup` 里最先做一件事：若存在 `app_local_data_dir/devocal-restore.json`，用 `WinSessions` 执行 `restore`。
  - 注册 `DevocalState` 与两个命令。
  - 后台线程每 100 ms 调一次 `Supervisor::tick`，`target` 来自 `MediaState::audio_target()` 加 `resolve`；只在 `source_id` 变化时才重新 `resolve`。
  - `RunEvent::Exit` 和主窗口销毁时，调用 `shutdown()`。

- [ ] **Step 1: 写失败测试**（`FakeLink`：内存队列，可模拟退出和事件序列）
  - `enable_sends_handshake_in_order`：先 `enable(Some(path))`，再带 target `tick`。发出的命令依次为 `hello`、`set_model`、`attach`、`set_mode(true)`。没有 target 时不 spawn。
  - `crash_restores_then_restarts_up_to_three_times_per_minute`：
    - 60 s 内连续崩溃 4 次：`restore` 被调用 4 次，`spawn` 被调用 1 + 3 次，最终相位为 `failed`；
    - 在 61 s 时再 `enable`，预算已恢复。
  - `gate_drops_during_attach_and_scales_when_active`：
    - attaching 时 `read()` 为 `(_, None)`；
    - 收到 `attenuation = 1e-4/0.3` 后为 `Some(3000.0)`，误差 ±0.1；
    - epoch 单调递增。
  - `player_change_releases_then_attaches_new`。
  - `disable_keeps_hold`：`disable` 后 `held = true`，只发出了 `set_mode(false)`。
  - `missing_model_reports_unavailable`：`model_path` 返回 `None` 时 `enable` 不 spawn，相位为 `unavailable`。
  - `shutdown_kills_and_restores_if_engine_hangs`。
- [ ] **Step 2: 运行，确认失败。** 在 `src-tauri/` 下运行 `cargo test devocal`。
- [ ] **Step 3: 实现。**
- [ ] **Step 4: 运行，确认通过。** `cargo test` 全部通过，包括原有测试。
- [ ] **Step 5: 检查点。**

### Task 11: 识调衰减还原（`audio/mod.rs`）

**Files:**
- Modify:
  - `src-tauri/src/audio/mod.rs`：`AudioState::new`、`Inner::publish`；
  - `src-tauri/src/lib.rs`：创建 `Arc<AttenuationGate>` 并传给 `AudioState` 和 `DevocalState`。

**Interfaces:**
- Consumes: 任务 10 的 `AttenuationGate`。
- Produces: `AudioState::new(media: MediaState, gate: Arc<AttenuationGate>) -> Self`。所有调用方，包括 `examples/*.rs` 与测试，都要同步改。
- 规则（在 `publish` 中、写入环形缓冲之前）：
  - 读 `gate.read()`。epoch 与 `Inner` 记录的上一个值不同时，先 `invalidate_pcm()` 再记下新值。
  - `None` 时丢弃这个包，返回 false，电平不更新。
  - `Some(g)` 且 `g != 1.0` 时，样本逐个乘以 g，再截到 `[-4.0, 4.0]`，防止异常值污染分析。

- [ ] **Step 1: 写失败测试**（在 `audio/mod.rs` 现有的 `mod tests` 中补充）
  - `gate_scales_samples`：gate 为 `Some(3000.0)`，送入 1e-4 的常数包，环形缓冲中的样本为 0.3 ± 1e-6。
  - `gate_epoch_change_invalidates_history`：先送入一个包，再 `set` 一次，`capture_generation` 加 1，且缓冲被清空。
  - `gate_none_drops_packet`：`publish` 返回 false，`sample_end_sequence` 不变。
- [ ] **Step 2: 运行，确认失败。** `cargo test audio::`。
- [ ] **Step 3: 实现。**
- [ ] **Step 4: 运行，确认通过。** 运行全部 `cargo test`；`cargo build --examples` 也要编译通过。
- [ ] **Step 5: 检查点。**

### Task 12: 构建接入（externalBin）

**Files:**
- Create: `scripts/build-devocal-engine.mjs`
- Modify: `src-tauri/tauri.conf.json`、`package.json`（scripts）、`.gitignore`（如存在则加 `src-tauri/binaries/`）

**Interfaces:**
- Produces:
  - `npm run build:engine [-- --release]` 做两件事：
    - 在 `devocal/` 中运行 `cargo build -p devocal-engine [--release]`；
    - 把产物复制到 `src-tauri/binaries/devocal-engine-x86_64-pc-windows-msvc.exe`。
  - `tauri.conf.json` 改为 `"bundle": { "active": false, "externalBin": ["binaries/devocal-engine"] }`。
    - tauri-build 会在构建时把它复制到 `target/<profile>/devocal-engine.exe`；
    - `ProcessLink` 从 `std::env::current_exe()?.with_file_name("devocal-engine.exe")` 启动它。

- [ ] **Step 1: 写脚本并改配置。**
- [ ] **Step 2: 验证。**
  - 运行 `npm run build:engine`，然后在 `src-tauri/` 下运行 `cargo build`。
  - 期望：`src-tauri/target/debug/devocal-engine.exe` 存在。
  - 如果这个 tauri-build 版本不会复制 externalBin，改为由脚本直接复制到 `target/debug/`，并在脚本注释里记录原因。
- [ ] **Step 3: 检查点。**

### Task 13: 前端开关、状态与设置页

**Files:**
- Create: `src/devocal.ts`, `src/devocal.test.ts`, `src/useDevocal.ts`, `src/components/DevocalSettings.tsx`, `tests/devocal.spec.ts`
- Modify:
  - `src/components/PlayerControls.tsx`：第 37 行和第 111–115 行的预览状态与提示，以及第 134 行的无障碍说明；
  - `src/SettingsPage.tsx`；
  - 现有的 `tests/*.spec.ts`：凡是模拟 IPC 遇到未知命令会抛错的，都补上 `get_devocal_status`，返回 `{phase:"off", held:false, ...}`。

**Interfaces:**
- Consumes: 任务 10 的命令 `get_devocal_status` 与 `devocal_command({request:{action}})`，以及 `DevocalStatus`（camelCase）。
- Produces:
  ```ts
  export type DevocalPhase = "off"|"attaching"|"passthrough"|"devocal"|"fallback"|"releasing"|"restarting"|"failed"|"unavailable";
  export interface DevocalStatus { phase: DevocalPhase; held: boolean; latencyMs: number|null; loadRatio: number|null;
    fallbackReason: "overload"|"model_error"|null; sessionOverridden: boolean; inputSilent: boolean; error: string|null }
  export function devocalLabel(s: DevocalStatus): string | null;
  export function useDevocal(): { status: DevocalStatus; enabled: boolean; toggle(): Promise<void>; release(): Promise<void> };
  ```
  `useDevocal` 每 500 ms 轮询一次；`enabled` 在 `phase ∈ {attaching, devocal, restarting}` 时为 true。
- 固定文案。`devocalLabel` 按以下顺序取第一条满足的：

  | 条件 | 文案 |
  |---|---|
  | `sessionOverridden` | `播放器音量被调整，请调本应用音量` |
  | `inputSilent` | `未录到播放器声音（可能是独占模式）` |
  | `devocal` | `` `去人声中 · 延迟 ${Math.round(latencyMs)} ms` `` |
  | `fallback` + `overload` | `性能不足，已退回原声` |
  | `fallback` + `model_error` | `去人声模型出错，已退回原声` |
  | `attaching` 或 `restarting` | `正在接管播放器…` |
  | `failed` | `去人声引擎多次异常，已保持原声` |
  | `unavailable` | `未找到去人声模型` |
  | `passthrough` 且 `held` | `原声直通` |
  | 其余 | `null` |

- 按钮：
  - `aria-pressed = enabled`；
  - 提示文字仍为 `开启去人声` / `关闭去人声，恢复原唱`；
  - 删除“前端预览：去人声处理尚未接入”的提示；
  - 无障碍说明改成“去人声会接管当前播放器的声音输出”。
- 设置页 `DevocalSettings`：
  - 标题“去人声”；
  - 按钮“释放播放器”：`held = false` 时禁用，点击后调用 `release()`；
  - 说明：“开启去人声后，本应用会接管当前播放器的声音输出。音量合成器里播放器那一栏接近 0 是正常的，声音由本应用发出；要调音量请调本应用。点‘释放播放器’可立即交还。”

- [ ] **Step 1: 写失败测试**
  - `src/devocal.test.ts`（vitest）：表中每一行至少一个用例。覆盖优先级（`sessionOverridden` 优先于 `devocal`）；`latencyMs` 为 45.4 时显示 `去人声中 · 延迟 45 ms`。
  - `tests/devocal.spec.ts`（Playwright，模拟 IPC，写法同 `player-controls.spec.ts`）：
    - 点击按钮后发出 `devocal_command` 且 `action = "enable"`；
    - 模拟状态变为 `devocal` 后，按钮 `aria-pressed = "true"`，并出现状态文字；
    - 再点一次，发出的是 `disable`。
  - 设置页用例：`held = true` 时“释放播放器”可点，点击后发出 `release`。
- [ ] **Step 2: 运行，确认失败。** 运行 `npm test` 与 `npx playwright test tests/devocal.spec.ts`。
- [ ] **Step 3: 实现。**
- [ ] **Step 4: 运行，确认通过。** `npm test` 和 `npm run test:ui` 全部通过，原有用例也要通过；`npm run build` 无类型错误。
- [ ] **Step 5: 检查点。**

### Task 14: 真机验收（每一项执行前都要征得用户同意）

**Files:**
- Create:
  - `work/devocal-accept-<日期>/`：`scope.md`、`timeline.md`、`evidence/*.json`；
  - `docs/<日期>_devocal-engine-acceptance-report.md`；
  - 并更新 `docs/README.md` 的索引。

- [ ] **Step 1: 征得同意。** 向用户说明将要做的事，得到同意后才继续：
  - 用 Folia 播放；
  - 本应用会接管 Folia 的会话音量；
  - 会强杀引擎和应用各一次；
  - 会切换一次默认输出设备。

  执行前核验 Folia 的进程号、映像路径与创建时间，记入 `scope.md`。
- [ ] **Step 2: 直通。** 开启后保持直通，用 `latency_probe` 录 60 s。
  - 期望：延迟 ≤ 40 ms，相关系数 ≥ 0.99，断音 0 次。
- [ ] **Step 3: 去人声。** 打开去人声，录 60 s。
  - 期望：延迟 ≤ 50 ms，断音 0 次，`loadRatio < 0.7`；记录 CPU 占用。
- [ ] **Step 4: 强杀引擎。**
  - 期望：1 s 内 Folia 会话音量回到原值；随后引擎自动重启并重新接管；恢复文件被删除。
- [ ] **Step 5: 强杀应用。**
  - 期望：引擎自行恢复音量后退出；再启动应用时没有残留的恢复文件。
- [ ] **Step 6: 播放器退出，以及切换默认设备。**
  - 期望：播放器退出时，引擎回到 `idle`，没有残留；切换设备后，输出跟随到新端点，没有长时间静音。
- [ ] **Step 7: Review Focus 第 1 条复测。** 把 Folia 的音量设为 30% 后接管。
  - 期望：接管前后输出电平差 ≤ 1 dB；释放后 Folia 的音量回到 30%。
- [ ] **Step 8: 用户试听。** 请用户判断两件事：切换卡顿是否可以接受；去人声后的音质如何。如实记录，不用代理指标代替。
- [ ] **Step 9: 写报告并检查点。**
  - 报告写清每项的结果与证据哈希；未通过的项如实记录。
  - 现场恢复：释放播放器；删除测试期间新建的文件（证据除外）。
