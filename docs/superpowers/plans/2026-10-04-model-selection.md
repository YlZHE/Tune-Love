# 去人声模型选择与 DirectML：实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 StemgenRT、bytesep、HTDemucs 三个模型在设置里可选。计算设备可选“自动 / CPU / GPU（DirectML）”。高质量档用窗口型分离器实现，GPU 平均占用不超过 30%。

**Architecture:**
- **引擎**：新增 `WindowedSeparator`。它按 128 帧实现现有的 `Separator` trait，内部攒够步长后交给专属推理线程，再按固定延迟交出结果。
- **处理器**：直通延迟和模型延迟分开，高质量档开关时会跳一下。
- **DirectML**：ORT 打开 `directml` 特性，首次用 GPU 前做一致性自检，运行中算不过来就退回。
- **app**：模型清单改为多设备结构。前端保存所选的模型和设备，每次开启去人声时一起发给后端。

**Tech Stack:** Rust（devocal engine/core、src-tauri）、ort 2.0.0-rc.13（ONNX Runtime 1.28，DirectML）、React/TS、Playwright、Python（模型转换，用 `artifacts/separation-bench/export-venv`）。

**Spec:** `docs/superpowers/specs/2026-10-04-model-selection-design.md`

## Global Constraints

**模型与参数**
- 模型 id：`stemgenrt-hop128`（现有）、`bytesep-mobilenet-1s`、`htdemucs-ft-vocals-1s`。三个模型都是 44.1 kHz。
- 窗口参数：
  - bytesep 用 GPU：W=1000 ms、H=100 ms、L=100 ms，固定延迟 230 ms；
  - bytesep 用 CPU：W=1000 ms、H=200 ms、L=100 ms、2 线程，固定延迟 360 ms；
  - HTDemucs：只能用 GPU，W=1000 ms、H=100 ms、L=100 ms，固定延迟 230 ms。
- 固定延迟 = H + L + 0.3·H。推理时限为 0.3·H。

**运行时退回与自检**
- 运行中满足任一条件就退回：连续 3 次超时，或者 10 s 内平均占用（推理耗时 ÷ H）> 0.3。
- GPU 一致性自检：GPU 与 CPU 的结果对比，SDR ≥ 40 dB 才通过。结果按“适配器描述 + 驱动版本 + 模型 SHA-256”缓存。

**选择规则**
- 设备取值为 `auto` / `cpu` / `gpu`，默认 `auto`。
- 用户明确选 `gpu` 却用不了时，不改用 CPU，而是报原因。只有 `auto` 会退回 CPU。
- 高质量档下开关去人声：直通延迟保持 128 帧，切换时有跳变，沿用现有的 20 ms 交叉淡化。
- 只有 StemgenRT 保留“下载完成后自动开启”。

**来源、许可与文案**
- 新模型的文件地址：`https://github.com/YlZHE/Tune-Love/releases/download/models-v1/<file>`，可以走镜像加速。
- 确认框必须写明：原始来源、许可、署名、“已由本项目转换修改”。HTDemucs 还要写明“训练数据来源不明，仅限非商业使用”。
- 高质量档确认框的文案为：`附加延迟约 <N> ms，歌词可能与播放不同步。` 其中 N 取所选设备的 `latencyMs`。

**工程约束**
- `process()` 在音频处理线程上运行，不分配内存、不加锁。
- StemgenRT 的现有行为和测试全部保持不变。
- 提交身份：`YlZHE <59366419+YlZHE@users.noreply.github.com>`，提交信息末尾加 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。不推送。
- 模型权重、ONNX 文件和音频不入库。上传 Release 由用户执行。

## Rulings made while planning

1. **交叉淡化改用线性。** spec §4.1 写的是“5 ms 等功率交叉淡化”，计划改为线性。相邻段来自重叠的同一模型窗口，彼此高度相关，用等功率淡化会让中段响约 3 dB。参考实现 `stream_rt.py` 本身是硬切；这里在硬切的基础上，在每段开头加 5 ms 线性淡化（xf = 220 帧）。Python 夹具与 Rust 实现采用同一定义。
2. **自检缓存的位置。** spec 写的是 `<app_local_data>/gpu-check.json`，计划改为 `<models_dir>/gpu-check.json`，即模型路径的上两级目录。这样引擎不需要新增参数。
3. **GPU-only 模型运行中退回 StemgenRT 由 app 发起。** 引擎上报 `DeviceNote::GpuOverloaded`，supervisor 收到后改为加载 StemgenRT。原因是引擎不知道 StemgenRT 的路径。bytesep 改用 CPU 则由引擎自己完成。

## Review Focus

1. **选了高质量档，但它的模型还没下载，就开启去人声。** 应提示“未找到所选模型”，提示指向该模型的下载行，不悄悄改用 StemgenRT。由 Task 7 和 Task 8 测试。
2. **播放中切换模型或设备。** 应走“先淡出、再换模型”的流程，不留空洞，旧的推理线程退出。由 Task 3（`reset` 和析构时推理线程退出）和 Task 6 测试。
3. **GPU 推理偶发尖峰（例如 56 ms）。** 在时限内正常输出；超过时限的那一段输出对齐的原音，不出空洞。由 Task 3 测试。
4. **引擎重启（被强制结束后由 supervisor 拉起）。** 应沿用用户所选的模型和设备，而不是退回 StemgenRT。由 Task 7 测试。
5. **自检缓存文件损坏，或者驱动更新了。** 文件损坏时当作未检查，重新自检；缓存键变了也要重新自检。由 Task 6 测试。

---

### Task 1: 模型转换脚本与许可文件

**Files:**
- Create: `scripts/models/rewrite_graph.py`。来自 spike 的 `work/directml-spike-20261004/rewrite_convtr.py`，整理为可复用的函数：
  - `rewrite_convtranspose(model) -> model`
  - `split_to_slice(model) -> model`
  - `drop_unused_initializers(model) -> model`
- Create: `scripts/models/convert.py`。命令行用法：`convert.py bytesep|htdemucs --src <path> --out <dir>`。
  - bytesep 以 `artifacts/separation-bench/src/export_bytesep.py` 的 1 s 导出为基础；HTDemucs 以 `models/htdemucs_ft_vocals_1s.onnx` 为基础。
  - 依次做改写、CPU 一致性检查（≥ 100 dB），最后写出 ONNX 和 `sha256.txt`（字节数与 SHA-256）。
- Create: `scripts/models/tests/test_rewrite_graph.py`
- Create: `licenses/bytesep.txt`（CC BY 4.0 署名、Zenodo DOI 5804160 与 5513378，并注明“已由 Tune Love 转换为 ONNX 并改写”）和 `licenses/htdemucs.txt`（Demucs MIT、StemSplitio 模型卡、训练数据来源不明）。

**Interfaces:**
- Produces：两个 ONNX 文件（本地，不入库），文件名为 `bytesep-mobilenet-1s.onnx` 和 `htdemucs-ft-vocals-1s.onnx`；以及它们的字节数和 SHA-256，供 Task 2 使用。
- Produces：每个模型的输入名、输出形状，以及人声在输出中的序号（`vocalsIndex`）。bytesep 的输出就是人声，序号记为 0；HTDemucs 的输出为 `[1,4,2,W]`，人声序号以导出结果为准，spike 中为 3。

- [ ] **Step 1: 写失败测试**（`test_rewrite_graph.py`，用 onnx helper 构造小图，在 onnxruntime 的 CPU 上比较）
  - `test_convtranspose_rewrite_matches`：1 维 ConvTranspose，Cin=8、Cout=1、K=256、S=64，权重随机常量。改写后最大误差 ≤ 1e-6。
  - `test_split_to_slice_matches`：二等分的 Split 改写为 Slice 后，结果逐位相等。
  - `test_drop_unused_initializers`：未被引用的常量被删除，图仍然可以运行。
- [ ] **Step 2: 运行测试，确认失败**
  Run: `artifacts/separation-bench/export-venv/Scripts/python.exe -m unittest discover -s scripts/models/tests -v`
  Expected: 因为缺少模块而失败。
- [ ] **Step 3: 实现三个函数和 `convert.py`。** 改写算法见 `work/directml-spike-20261004/report.md` §2；适用条件不变，即“权重是常量、group 1、无膨胀、pads 0、核 ≥ 256”。
- [ ] **Step 4: 运行测试，确认通过。** 再对两个模型各运行一次 `convert.py`，输出到 `artifacts/models-v1/`（不入库），并记录 `sha256.txt`。
- [ ] **Step 4b: README 的致谢里加入 bytesep 与 HTDemucs，写明许可，并注明“已由本项目转换修改”。**
- [ ] **Step 5: 提交** `feat(models): conversion scripts for bytesep and HTDemucs ONNX` 和 licenses。

### Task 2: 模型清单改为多设备结构

**Files:**
- Modify: `src-tauri/models.json`
- Modify: `src-tauri/src/devocal/model/manifest.rs`
- Modify: `src/modelDownload.ts`
- Test: `manifest.rs` 中的现有测试、`src/modelDownload.test.ts`（如有，沿用现有测试文件）

**Interfaces:**
- Produces（Rust）：
  - `ModelSpec` 去掉 `runtime`，新增：
    - `kind: ModelKind`：取值 `Streaming` 或 `Windowed`；
    - `devices: Devices`：结构为 `{ cpu: Option<DeviceParams>, gpu: Option<DeviceParams> }`；
    - `vocals_index: u32`。
  - `DeviceParams { window_ms: u32, hop_ms: u32, lookahead_ms: u32, threads: u16, latency_ms: f64 }`。Streaming 模型的 `window_ms`、`hop_ms`、`lookahead_ms` 都填 0。
  - 序列化为 camelCase。
- Produces（TS）：同名的类型 `ModelKind`、`DeviceParams`、`ModelSpec.devices`。

- [ ] **Step 1: 写失败测试**
  - `bundled_manifest_has_three_models`：三个 id 都在，且 `stemgenrt-hop128.devices.gpu` 为 None。
  - `windowed_latency_matches_rule`：每个 Windowed 设备都满足 `latency_ms == hop + lookahead + 0.3*hop`，即 bytesep GPU 230、CPU 360，HTDemucs GPU 230。
  - `rejects_windowed_without_device` 和 `rejects_cpu_threads_zero`。
  - TS：`parseManifest` 能读出 `devices`，拒绝缺少 `devices` 的条目。
- [ ] **Step 2: 运行测试，确认失败**：`cargo test --manifest-path src-tauri/Cargo.toml manifest`，以及 `npx vitest run src/modelDownload`
- [ ] **Step 3: 实现**
  - 新增两个条目：文件地址按 Global Constraints，字节数和 SHA-256 取自 Task 1 的 `sha256.txt`，`mirrorable: true`，license 和 source 按 spec §1。
  - 更新所有读取 `runtime` 的地方。
- [ ] **Step 4: 全量 `cargo test`（src-tauri）、`npx vitest run`、`npx tsc --noEmit` 通过**
- [ ] **Step 5: 提交** `feat(models): multi-device manifest with bytesep and HTDemucs`

### Task 3: 窗口型分离器（引擎，与模型无关的部分）

**Files:**
- Create: `devocal/engine/src/windowed.rs`
- Create: `scripts/models/stitch_fixture.py`。用参考定义生成夹具：输入为确定性的伪随机立体声，模型为“人声 = 0.5 × 窗口”。
- Create: `devocal/engine/tests/fixtures/windowed_stitch.json`（小于 50 kB）。
- Modify: `devocal/engine/src/separator.rs`（`pub mod`/导出）、`devocal/engine/src/lib.rs`

**Interfaces:**
- Produces:

```rust
pub trait WindowModel: Send + 'static {
    /// input: interleaved stereo, window_frames*2; writes vocals the same shape.
    fn run(&mut self, input: &[f32], vocals: &mut [f32]) -> Result<(), String>;
}
pub struct WindowedParams { pub window_frames: usize, pub hop_frames: usize, pub lookahead_frames: usize }
pub struct WindowedSeparator { /* SPSC rings, worker thread handle, counters */ }
impl WindowedSeparator {
    pub fn new(model: Box<dyn WindowModel>, p: WindowedParams) -> Self; // spawns the worker
    pub fn timeouts(&self) -> u64;
    pub fn duty_10s(&self) -> f32;      // mean(inference_time)/hop over the last 10 s
    pub fn overloaded(&self) -> bool;   // 3 consecutive timeouts || duty_10s > 0.3
}
impl Separator for WindowedSeparator { /* hop()=128, latency_frames()=H+L+ceil(0.3*H), passthrough_latency_frames()=128 (Task 4 adds this method) */ }
```

- 拼接定义（与 Ruling 1 一致）：
  - 第 k 段覆盖输入的 `[kH, kH+H)`。所用窗口是截至 `kH+H+L` 的 W 帧。
  - 取窗口中的 `[W−L−H−xf, W−L)`，伴奏 = 输入窗口对应部分 − 人声。
  - 前 xf=220 帧与上一段的尾部做线性交叉淡化。
- 推理超时：该段改为输出对齐的输入原音（取自内部的输入历史），同样带 xf 淡化，并且 `timeouts` 加 1。
- `reset()`：清空所有环；代号加 1，迟到的结果按代号丢弃。
- `Drop`：通知推理线程退出并 join。

- [ ] **Step 1: 写失败测试**（`windowed.rs` 内的 `mod tests`；假模型 `Scale(0.5)`，可设置每次推理的睡眠时长）
  - `latency_is_fixed`：脉冲输入后，输出在 `latency_frames()` 处出现。
  - `matches_python_fixture`：给定夹具输入，逐块 `process()` 的输出与夹具逐样本一致（≤ 1e-6）。
  - `timeout_outputs_dry_aligned_audio`：假模型睡眠 0.5·H 时，相应段等于对齐的输入，`timeouts()` 加 1，且没有全零的空洞。
  - `three_consecutive_timeouts_overload`，以及 `high_duty_overloads`：睡眠 0.35·H，持续 10 s 的模拟时间后返回 true。用注入时钟模拟时间，不真睡 10 s。
  - `reset_drops_late_results` 和 `drop_joins_worker`。
  - `process_does_not_allocate`：沿用引擎现有的无分配测试方式。
- [ ] **Step 2: 运行测试，确认失败**：`cd devocal && cargo test -p devocal-engine windowed`
- [ ] **Step 3: 实现。** 先写 `stitch_fixture.py` 并生成夹具（用 export-venv 的 numpy），再写 Rust 实现。
- [ ] **Step 4: `cargo test -p devocal-engine` 全量通过**
- [ ] **Step 5: 提交** `feat(engine): windowed separator with a dedicated inference worker`

### Task 4: 直通延迟与模型延迟分开

**Files:** Modify `devocal/engine/src/separator.rs`、`processor.rs`、`processing.rs`（发布延迟处）

**Interfaces:**
- Produces：`Separator::passthrough_latency_frames(&self) -> usize`，默认实现返回 `self.latency_frames()`。
  - `Processor` 的直通 `DelayLine` 用它构造。
  - 处理线程发布的 `proc_latency_frames` 取当前路径：Passthrough、WarmingUp、FadingIn 时取直通延迟；Devocal、FadingOut 时取模型延迟。Fallback 时取直通延迟。

- [ ] **Step 1: 写失败测试**（`processor.rs` tests，使用一个 latency=10000、passthrough=128 的假分离器）
  - `passthrough_uses_its_own_latency`：Passthrough 阶段的输出等于延迟 128 帧的输入。
  - `devocal_after_fade_uses_model_latency`。
  - `published_latency_follows_stage`。
  - 现有 StemgenRT 和 DelayOnly 的测试不改动，全部通过。
- [ ] **Step 2: 运行测试，确认失败**
- [ ] **Step 3: 实现**
- [ ] **Step 4: `cargo test -p devocal-engine` 全量通过**
- [ ] **Step 5: 提交** `feat(engine): passthrough latency independent of the model latency`

### Task 5: ORT 窗口模型、DirectML 与设备选择

**Files:**
- Modify: `devocal/engine/Cargo.toml`（ort 加 `directml` 特性）
- Create: `devocal/engine/src/ort_window.rs`：`OrtWindowModel` 实现 `WindowModel`。
- Modify: `devocal/core/src/protocol.rs`、`devocal/engine/src/engine.rs`（`set_model`、`ModelSpec`、loader）

**Interfaces:**
- Consumes：Task 3 的 `WindowModel`、`WindowedParams`、`WindowedSeparator`。
- Produces（protocol，新字段都加 `#[serde(default)]`）：
  - `Command::SetModel` 增加：
    - `windowed: Option<WindowedSpec>`，其中 `WindowedSpec { window_ms: u32, lookahead_ms: u32, gpu_hop_ms: Option<u32>, cpu_hop_ms: Option<u32>, cpu_threads: u16, vocals_index: u32 }`；
    - `device` 的取值放宽为 `auto` / `cpu` / `gpu`。
  - `Metrics` 增加：
    - `device: Option<Device>`，`Device` 为 `Cpu` 或 `Gpu`；
    - `device_note: Option<DeviceNote>`，`DeviceNote` 为 `GpuUnavailable`、`GpuCheckFailed`、`GpuOverloaded` 之一。
  - `ErrorCode::GpuRequired`。
- Produces（engine）：
  - `OrtWindowModel::load(path: &Path, device: Device, threads: u16, vocals_index: u32) -> Result<Self, String>`。GPU 的会话参数按 spec §4.3；CPU 的会话参数沿用 `stemgen.rs` 的写法。
  - `resolve_device(requested: &str, spec: &Option<WindowedSpec>, gpu_ok: impl FnOnce() -> bool) -> Result<Device, ErrorCode>`，按 spec §4.4 的表格实现。StemgenRT 不论请求什么都返回 `Cpu`。

- [ ] **Step 1: 写失败测试**
  - `resolve_device` 覆盖 spec §4.4 表格的 9 格。`gpu_ok` 返回 false 时：`auto` 得到 Cpu，`gpu` 得到错误，HTDemucs 在 `auto` 下返回 `GpuRequired`。
  - protocol 往返测试：带 `windowed` 的 SetModel、带 `device` 的 Metrics，以及不带新字段的旧报文仍能解析。
  - `set_model` 接受 `gpu`，不再返回 `BadDevice`。
- [ ] **Step 2: 运行测试，确认失败**：`cd devocal && cargo test -p devocal-core -p devocal-engine`
- [ ] **Step 3: 实现。** `set_model` 根据 `windowed` 是否存在，在 StemgenRT 与 `WindowedSeparator::new(OrtWindowModel…)` 之间选择。实际使用的设备写入 Metrics。CPU 配置用 `cpu_hop_ms` 和 `cpu_threads`，GPU 配置用 `gpu_hop_ms`。
- [ ] **Step 4: 全量测试通过；`cargo build --release -p devocal-engine` 成功。** 把 exe 体积的前后对比写进报告。
- [ ] **Step 5: 提交** `feat(engine): DirectML window model and device resolution`

### Task 6: GPU 一致性自检与运行时退回

**Files:** Create `devocal/engine/src/gpu_check.rs`；Modify `engine.rs`、`windowed.rs`（如需）

**Interfaces:**
- Produces：
  - `gpu_check(models_dir: &Path, model_path: &Path, run_gpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>, run_cpu: impl FnOnce(&[f32]) -> Result<Vec<f32>, String>) -> bool`。
    - 测试音：引擎内置、确定性的 1 s 立体声信号（由代码生成，例如几个正弦加噪声），不依赖用户音频。
    - 缓存：写入 `<models_dir>/gpu-check.json`，键为“适配器描述 + 驱动版本 + 模型 SHA-256”。
    - 通过条件：SDR ≥ 40 dB。
  - 运行时：`WindowedSeparator::overloaded()` 为真时，按设备处理：
    - GPU 且模型支持 CPU：重新加载 CPU 配置（走现有的换模型流程），`device_note = GpuOverloaded`；
    - GPU 且模型只支持 GPU：进入 `Fallback(Overload)`，`device_note = GpuOverloaded`，由 app 改加载 StemgenRT（Ruling 3）。
    - 这次会话内不再自动回到 GPU。

- [ ] **Step 1: 写失败测试**
  - `check_passes_and_caches`：假的 GPU 和 CPU 输出相同，返回 true，并写入缓存；第二次调用时不再执行 `run_gpu`。
  - `check_fails_below_40db`。
  - `corrupt_cache_rechecks` 和 `new_driver_rechecks`：缓存键变化时重新自检。
  - 引擎层：自检不通过时，`auto` 得到 Cpu 加 `GpuCheckFailed`，`gpu` 得到错误。
  - 运行时退回的两条分支。
- [ ] **Step 2: 运行测试，确认失败**
- [ ] **Step 3: 实现。** 适配器描述和驱动版本用 DXGI 读取（`dxgi.dll` 已在导入表中）。读取失败时用 `"unknown"` 作为键，每次都重新自检。
  另外，仅在调试构建中支持环境变量 `TUNE_LOVE_WINDOWED_SLOW_MS`：给每次推理额外睡眠这么多毫秒，供 Task 9 人为制造超时；release 构建中这段代码不编译进去。
- [ ] **Step 4: 全量测试通过**
- [ ] **Step 5: 提交** `feat(engine): GPU parity self-check and runtime fallback`

### Task 7: app 端——所选模型与设备

**Files:** Modify `src-tauri/src/devocal/mod.rs`、`supervisor.rs`、`model/mod.rs`（自动开启只限 StemgenRT，现状保持）

**Interfaces:**
- Consumes：Task 2 的清单、Task 5 的 protocol。
- Produces：
  - `DevocalRequest { action: String, model_id: Option<String>, device: Option<String> }`（camelCase）。
  - supervisor：
    - 新增 `selection: Selection { model_id: String, device: String }`，默认为 `stemgenrt-hop128` 加 `auto`；
    - `enable(model, selection)` 只在选择变化时重发 `SetModel`；
    - 引擎重启后发送的 `SetModel` 使用 `selection`；
    - `SetModel.windowed` 由清单生成。
  - `model_path(data_dir, id)`：按 id 查找。环境变量 `TUNE_LOVE_STEMGENRT_ONNX` 只对 StemgenRT 生效。
  - 收到 `DeviceNote::GpuOverloaded`，而当前模型没有 CPU 配置时，改发 StemgenRT 的 `SetModel` 并保持去人声开启。`selection` 不变。
  - `DevocalStatus` 新增：
    - `model_id: Option<String>`；
    - `device: Option<&'static str>`；
    - `device_note: Option<&'static str>`；
    - 所选模型缺失时，`error` 为 `model_not_found:<id>`。

- [ ] **Step 1: 写失败测试**（supervisor 现有的假 link 测试方式）
  - `enable_with_selection_sends_windowed_set_model`：bytesep 加 gpu，SetModel 带上清单中的参数。
  - `restart_keeps_selection`。
  - `gpu_only_overload_switches_to_stemgenrt`。
  - `missing_selected_model_reports_its_id`。
  - `request_without_model_id_keeps_previous_selection`：兼容旧的前端请求。
- [ ] **Step 2: 运行测试，确认失败**：`cargo test --manifest-path src-tauri/Cargo.toml devocal`
- [ ] **Step 3: 实现**
- [ ] **Step 4: 全量 `cargo test`（src-tauri）通过**
- [ ] **Step 5: 提交** `feat(devocal): selected model and device reach the engine`

### Task 8: 设置页与主窗口

**Files:**
- Create: `src/devocalModelPreferences.ts`，按 `src/autoApplyPreferences.ts` 的写法：`parse`、`read`、`subscribe`、`write`；`localStorage` 键为 `tune-love.devocal-model`；默认值 `{ modelId: "stemgenrt-hop128", device: "auto" }`。
- Modify: `src/useDevocal.ts`：每次 `enable` 都带上偏好。
- Modify: `src/components/DevocalSettings.tsx`、`src/components/ModelSection.tsx`
- Create: `src/components/QualityTierDialog.tsx`
- Modify: 主窗口中“未找到模型”提示所在的组件（`PlayerControls.tsx` 一带），指向所选模型。
- Test: `src/devocalModelPreferences.test.ts`、`tests/model-selection.spec.ts`

**Interfaces:**
- Consumes：Task 2 的 TS 清单类型、Task 7 的 `DevocalRequest` 和 status 字段。

**文案**（逐字使用）：
- 单选项标签：`StemgenRT（默认，低延迟）`、`bytesep（高质量）`、`HTDemucs（高质量，需要 GPU）`。
- 设备选项：`自动`、`CPU`、`GPU`。
- 状态行：
  - `正在使用：GPU`
  - `正在使用：CPU`
  - `GPU 不可用，已改用 CPU`（对应 auto 加 `GpuUnavailable` 或 `GpuCheckFailed`）
  - `所选模型需要 GPU，当前不可用`
  - `GPU 负载过高，已改用 CPU` 或 `GPU 负载过高，已改用 StemgenRT`
- 确认框：
  - 标题：`切换到高质量档`
  - 正文：`附加延迟约 <N> ms，歌词可能与播放不同步。`；HTDemucs 另加一段：`训练数据来源不明，仅限非商业使用。`
  - 按钮：`取消`、`切换`

- [ ] **Step 1: 写失败测试**
  - 偏好模块的单元测试：默认值、非法值回退为默认、跨窗口同步。
  - Playwright：
    - 选 bytesep 时弹出确认框；N 随设备变化，CPU 为 360、GPU 为 230；点“取消”后选择不变。
    - 选 HTDemucs 加 CPU 时，显示“所选模型需要 GPU，当前不可用”。
    - 开启去人声时，请求带上 `modelId` 和 `device`。
    - 所选模型未下载时，主窗口提示指向它的下载行。
    - 状态行的各种文案都有覆盖。
- [ ] **Step 2: 运行测试，确认失败**
- [ ] **Step 3: 实现**
- [ ] **Step 4: `npx vitest run`、`npx playwright test`、`npx tsc --noEmit` 全部通过**
- [ ] **Step 5: 提交** `feat(ui): devocal model and compute device selection`

### Task 9: 模型发布与真机验收（需用户授权）

- [ ] **Step 1: 用户上传。** 用户把 Task 1 产出的两个 ONNX 上传到 Release `models-v1`。上传后核对下载地址的字节数和 SHA-256 与清单一致。
- [ ] **Step 2: 列出真机测试项，取得授权。**
  - 测试组合：
    - StemgenRT 用 CPU；
    - bytesep 用 GPU、bytesep 用 CPU；
    - HTDemucs 用 GPU。
  - 每个组合连续播放 5 分钟，测量：延迟、GPU 与 CPU 占用（按 PID 的性能计数器）、丢音。
  - 另测：
    - 开关去人声的听感，由用户来听；
    - 自检文件是否生成；
    - 人为制造超时后能否退回：调试构建中增加 `TUNE_LOVE_WINDOWED_SLOW_MS`，给推理加入延时。

  每次接管前核验播放器进程身份。
- [ ] **Step 3: 执行，写报告** `docs/2026-10-0X_model-selection-real-machine.md`，并提交。
