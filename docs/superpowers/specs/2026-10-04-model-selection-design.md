# 去人声模型选择与 GPU（DirectML）：设计

日期：2026-10-04。状态：待用户审阅。

依据：
- [候选与测量计划](../../../../work/model-select-prep-20261004/prep.md)（本地，不入库）；
- [DirectML 摸底报告](../../../../work/directml-spike-20261004/report.md)（本地，不入库）；
- `docs/2026-10-02_model-weights-licensing.md`（上级目录）。

## 1. 用户决定（2026-10-04）

- 三个模型都在设置里可选：
  - **StemgenRT**：默认，低延迟。
  - **bytesep MobileNet**：高质量档。
  - **HTDemucs ft vocals**：高质量档。

  后两者只在用户主动选择时启用；选择时必须提示延迟，并提示歌词可能与播放不同步。
- 计算设备分**自动 / CPU / GPU**，默认自动。GPU 走 DirectML，不做只支持 NVIDIA 的 CUDA 方案。
- GPU 不能跑满：播放时持续的平均占用 ≤ 30%。
- HTDemucs 只提供 1 秒窗口配置，只能在 GPU 上运行。
- 高质量档下开关去人声：直通保持低延迟，切换时接受约 200 ms 的重复或跳过（带淡化）。
- 模型来源：
  - bytesep 和 HTDemucs 由本项目把转换、改写后的 ONNX 发布在 GitHub Release 上，确认框写明原始来源、许可、署名和“已由本项目转换修改”；
  - HTDemucs 另须写明“训练数据来源不明，仅限非商业使用”；
  - StemgenRT 照旧从作者仓库下载。

  这一条修改了此前“只从官方来源下载、不托管权重”的规则。

## 2. 不做

- StemgenRT 上 GPU：它的 DFT 在 DML 上报错，每 2.9 ms 调用一次，也不适合放到 GPU 上。
- CUDA、WebGPU 后端；HTDemucs 的 2 秒窗口配置。
- 高质量档的歌词延迟补偿。
- 接管与切换体验的改进（另立计划）。
- 原生采样率（P2）：三个模型都是 44.1 kHz，与现状一致。

## 3. 模型与参数

| id | 来源 | 设备 | 窗口 W | 步长 H | 前瞻 L | 线程 | 固定延迟 |
|---|---|---|---|---|---|---|---|
| `stemgenrt-hop128` | 作者仓库（现有） | CPU | — | 128 帧 | — | 1 | 128 帧（现有） |
| `bytesep-mobilenet-1s` | 本项目 Release | GPU | 1 s | 0.1 s | 0.1 s | — | H + L + 0.3·H = 230 ms |
| 同上 | 同上 | CPU | 1 s | 0.2 s | 0.1 s | 2 | 0.2 + 0.1 + 0.06 = 360 ms |
| `htdemucs-ft-vocals-1s` | 本项目 Release | GPU | 1 s | 0.1 s | 0.1 s | — | 230 ms |

- 固定延迟中的 0.3·H 是推理时限：推理必须在 0.3·H 内完成，这与“占用 ≤ 30%”是同一条线。
- 表中延迟是模型在现有渲染延迟之上额外增加的部分。

## 4. 架构

### 4.1 窗口型分离器（引擎，新文件 `devocal/engine/src/windowed.rs`）

`WindowedSeparator` 实现现有的 `Separator` trait（`separator.rs`）。对处理线程来说，它和 StemgenRT 一样：每次输入、输出 128 帧（`hop() = 128`），只是 `latency_frames()` 更大。

**输入与推理：**
- 每块输入写进内部输入环。
- 每攒够 H 帧，就把“截至最新输入的 W 帧窗口”交给专属推理线程。
- 推理线程用 ORT 会话完成推理：GPU 用 DirectML EP，CPU 用 intra 线程数。

**结果拼接：**
- 取窗口中 `[W − L − H, W − L)` 这一段作为该步长的伴奏。
- 相邻两段之间做 5 ms 等功率交叉淡化。
- 参考实现是摸底中的 `stream_rt.py`；Rust 实现必须与它逐样本对齐，用离线测试锁定。

**输出：**
- 结果按固定延迟写进输出环，`process()` 每块从输出环取 128 帧。
- 开始时预填 `latency_frames` 帧静音。这段静音处在现有的 200 ms 预热期内，那时听到的是直通，所以听不见。

**推理超时：**
- 某个窗口在时限内没有算完，这一段就输出对齐的原音，也就是延迟后的输入，并计为一次超时。不留空洞。
- 连续 3 次超时，或者 10 秒内的平均占用（推理耗时 ÷ H）超过 0.3，就按 §6 退回。

**`reset()`：** 清空两个环，丢弃在途推理。迟到的结果以代号核对后丢弃。

**无分配约束：** `process()` 走在音频处理线程上，不分配、不加锁，只用 SPSC 环与原子量。推理线程可以分配。

### 4.2 开关去人声（处理器）

- `Separator` 新增 `passthrough_latency_frames()`，默认等于 `latency_frames()`；窗口型分离器返回 128。
- `Processor::configure` 用它构造直通延迟线。所以：
  - StemgenRT 的行为完全不变；
  - 高质量档下，直通仍然低延迟。
- 去人声开、关时，仍用现有的 20 ms 等功率交叉淡化，在“直通（低延迟）”和“去人声（延迟 230 ms 等）”之间切换。听感是重复或跳过约等于两者的延迟差，这是用户的选择 A。
- 处理线程发布当前路径的延迟：直通阶段发布直通延迟，去人声阶段发布模型延迟。渲染的延迟估计随之变化，状态里显示的延迟据此计算。

### 4.3 DirectML

- `devocal/engine/Cargo.toml` 中 `ort` 加 `directml` 特性。pyke 的 Windows 预编译库本来就带 DirectML，不需要新下载；体积变化以重编译后为准，记入报告。
- GPU 会话参数：
  - `ep::DirectML` 用默认适配器，注册失败时返回错误，不悄悄改用 CPU；
  - `with_memory_pattern(false)`；
  - `with_parallel_execution(false)`；
  - 最高优化级别。
- **GPU 一致性自检**：每个“适配器描述 + 驱动版本 + 模型 SHA-256”组合，第一次用 GPU 前执行一次。
  - 用一段固定的 1 秒测试音（内置于引擎，不依赖用户音频），GPU 和 CPU 各算一次，SDR ≥ 40 dB 才算通过。
  - 结果记在 `<app_local_data>/gpu-check.json`，之后不再重复。
  - 不通过的组合不使用 GPU，状态里给出原因。

### 4.4 设备选择

app 在 `SetModel.device` 中发送 `auto`、`cpu` 或 `gpu`；协议里已有这个字段，只是放宽取值。引擎负责决定实际用哪个设备：

| 选择 | StemgenRT | bytesep | HTDemucs |
|---|---|---|---|
| 自动 | CPU | GPU（自检通过）否则 CPU | GPU（自检通过）否则不可用 |
| CPU | CPU | CPU | 不可用：`gpu_required` |
| GPU | CPU（该模型没有 GPU 路径，如实显示） | GPU，失败则不可用 | GPU，失败则不可用 |

- 引擎在状态中回报实际设备，界面显示“正在使用：GPU / CPU”。
- 用户明确选 GPU 却用不了时，不改用 CPU，而是报原因。只有“自动”才会退到 CPU。

## 5. app 与界面

- **清单**（`src-tauri/models.json`）：
  - 去掉 `runtime`，改为 `kind`（`streaming` 或 `windowed`）和 `devices`。`devices` 中每种设备一项，各自写明 `windowMs / hopMs / lookaheadMs / threads / latencyMs`。
  - 新增两个模型条目：
    - 文件地址：`https://github.com/YlZHE/Tune-Love/releases/download/models-v1/<file>`；
    - 可以走镜像加速；
    - 带上许可、署名，以及“已修改”说明。
  - Rust 和 TS 的清单类型、校验和测试同步更新。
- **选择的保存**：
  - 前端沿用现有的“一个偏好一个模块”的写法（localStorage 加 `storage` 事件）。新模块 `devocalModelPreferences.ts` 保存 `{ modelId, device }`。
  - 每次 `devocal_command` 的 `enable` 都带上它：`DevocalRequest` 增加可选的 `modelId` 和 `device`。
  - supervisor 记住最后一次的选择。引擎重启后重发 `SetModel` 时，用的也是这个选择。
  - 以下几处原先写死 StemgenRT，都改为按所选 id 处理：
    - `MODEL_ID`；
    - `model_path`；
    - 找不到模型时的提示。
- **设置页**（去人声部分）：
  - 模型单选，每个模型下方是现有的下载行。选中一个未下载的模型时，提示先下载。
  - 选高质量档时弹确认框，写明附加延迟（按当前设备取值，例如约 230 ms）、歌词可能与播放不同步，以及 HTDemucs 的许可说明。用户确认后才生效。
  - 计算设备：自动 / CPU / GPU。下方一行状态：正在使用的设备，或者不可用的原因。
- **找不到模型的入口**：主窗口里“未找到模型”的提示，指向所选模型的下载行。只有 StemgenRT 保留“下载完成后自动开启”，其他模型不自动开启。

## 6. 退回与错误

- **GPU 运行中超时**（§4.1 的条件）：
  - 模型支持 CPU：改用 CPU 配置，重新加载；
  - 不支持（HTDemucs）：改用 StemgenRT。

  两种情况都通过状态提示原因，并在本次会话里不再自动回到 GPU。
- **模型加载失败、自检不通过、`gpu_required`**：去人声显示不可用并说明原因，不悄悄换模型。上一条的运行中超时是唯一例外。
- **用户切换模型或设备**：沿用现有“先淡出、再换模型”的流程（`SetSeparator`、`try_swap`）。

## 7. 模型转换与发布

- 仓库新增 `scripts/models/`，放 Python 转换脚本：
  - bytesep：从 Zenodo 官方的 `.pth` 和 PQMF 滤波器转换；
  - HTDemucs：从 Hugging Face 上 StemSplitio 的 ONNX 转换。
- 转换流程：导出为固定 1 s 形状 → ConvTranspose 改写为 MatMul 加重叠相加 → Split 改为 Slice → 删除无用常量 → 与原模型做 CPU 一致性检查（≥ 100 dB）→ 输出文件和 SHA-256。
- 上传 Release `models-v1`，以及推送，都由用户执行或授权。
- `licenses/` 中新增 `bytesep.txt`（CC BY 4.0 署名，并注明已修改）和 `htdemucs.txt`；README 的致谢同步更新。

## 8. 测试

- **引擎单元测试**，用假的窗口模型（输出 = 输入乘以常数，加可控的耗时）：
  - 固定延迟准确；
  - 拼接段落正确、交叉淡化无缝；
  - 超时时输出对齐原音；
  - 连续超时和占用超过上限都会触发退回；
  - `reset` 丢弃迟到结果；
  - `process()` 不分配内存（沿用现有的无分配测试方式）。
- **与参考实现对齐**：同一段输入下，`WindowedSeparator` 的拼接结果与 `stream_rt.py` 的拼接逻辑逐样本一致。用离线生成的小夹具锁定，模型用恒等变换代替，不依赖真实权重。
- **处理器**：
  - 直通延迟与模型延迟分开设置；
  - 开关时，交叉淡化在两个延迟之间进行；
  - 发布的延迟值正确；
  - StemgenRT 的现有测试全部保持不变。
- **app**：
  - 清单的新结构及其校验；
  - `DevocalRequest` 带上模型 id 与设备；
  - supervisor 发出的 `SetModel` 正确，引擎重启后仍沿用所选模型；
  - 设备选择表（§4.4）的每一格。
- **界面**（Playwright）：模型单选、高质量档确认框、设备选项、状态行、找不到模型的提示指向所选模型。
- **真机**（需单独授权）：
  - 三个模型 × 设备组合，各连续播放 5 分钟，测量：延迟、GPU 与 CPU 占用、丢音；
  - 开关去人声的听感；
  - 自检文件生成；
  - 人为制造超时后的退回。

## 9. 风险

- 只在本机 NVIDIA 显卡上验证过。其他厂商或较弱的显卡上，速度和正确性不明，由一致性自检和运行时退回来兜底。
- DirectML 已进入维护期，后续需要关注替代方案。
- 现在只用 CPU 的引擎也直接导入了 `DirectML.dll` 和 `d3d12.dll`。在缺少这两个 DLL 的系统上能否启动尚未实测。本项目把它记为已知问题，不在本轮修复。
- HTDemucs 的许可风险已由用户接受，以如实告知为前提。
