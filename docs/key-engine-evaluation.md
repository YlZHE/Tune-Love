# 调式识别候选引擎与原生优化评测

**当前方向：保留 libkeyfinder，提高现有流程准确度，不再寻找替换引擎。** 最新见 [2026-09-30 输入链路排查](2026-09-30-native-input-accuracy-study.md)：归一化无总体收益；默认谐波分离和全局声道特征合并均退步；局部相位抵消得到受控复现，但尚无可以部署的整体准确度提升。

**上一阶段研究记录：** [2026-09-30 持续跟踪、S-KEY 与分数融合](2026-09-30-key-tracking-followup.md)。以下引擎比较保留为历史证据，不代表当前继续替换引擎的技术路线。

本轮完成的是**离线实验和开发接口**，没有替换正在运行的识别引擎，也没有调整 UI、音频采集、六秒最短窗口、首次 3 票 / 替换 5 票规则或 Auto-Tune 控制。

**后续真实音乐验证（2026-09-30）：** 已完成 48 首预选 FMA/FMAKv2 样本的短窗口试验。第 12 秒确认结果与参考标签一致为 libkeyfinder 22/48、Essentia 20/48；没有建立 Essentia 更准确的证据，暂不替换主引擎。详见 [真实音乐短窗口试验](2026-09-30-essentia-real-music-pilot.md)。另已完成 [八配置开发与 46 首独立留出试验](2026-09-30-key-accuracy-holdout-study.md)：开发集胜出配置在留出集为 14/46，默认 Essentia 15/46，原生 16/46，仍未建立升级依据。以下保留原合成实验及其结论，不将旧数字改写成真实音乐结果。

## 结论

1. **现有算法仍有低风险的计算优化空间。** 仅将 libkeyfinder C++ 编译改为 O2，Rust 仍使用相同 debug profile，普通增量调用平均从 **74.08 ms 降到 49.16 ms，约减少 33.6%**。这些样本的首次稳定正确结果仍是音频第 9 秒；更快算完不等于少听几秒就一定知道调式。
2. **Essentia WASM 是值得继续验证的候选。** 在这批简单合成和声中，其窗口计算约 11.96 ms，首次稳定正确结果在第 8 秒。不能据此推断它在真实歌曲中更准确或必然更快；运行时和预处理成本不同，且没有真实歌曲标注集。
3. **S-KEY CPU 在本轮没有速度优势。** 约 188.62 ms / 窗口，还需模型加载与冷启动；和声样本在第 8 秒确认正确。没有证据支持现在将它部署为本机低延迟主引擎。
4. **准确度尚不能宣布提高。** 四个有标签用例只包含 C 大调、A 小调两种构造和声及低音量、反相变体，不是四首独立真实歌曲。三个引擎都会对某些纯音、鼓点给出调式；不能只降低票数或把相似度当“准确率”。

推荐下一步保留当前线上算法，单独验证 O2 在实际调试程序中的 CPU / GUI 表现，并用可信标注的真实歌曲比较 Essentia 与现有算法。相对大小调、纯鼓前奏、808、弱和声、转调等都需要单列测试。**这些下一步没有在本轮自动执行。**

## 复用的代码与实验边界

| 引擎 | 固定版本 / 来源 | 本轮接入 |
|---|---|---|
| libkeyfinder | 2.2.6 / `a409c7447e9f440a12627ff4a540a43e41b48a55` | 复用当前增量实现；新增只供开发评测使用的 24 候选分数 |
| Essentia | 官方 `essentia.js 0.1.3` / MTG/essentia.js | Windows 本地 WASM KeyExtractor，不是原生 C++ 性能 |
| S-KEY | Deezer/skey / `918b83d273568d5041569bb8068843d19a335726` | 官方模型与特征路径，私有 Python 3.11.16、Torch 2.7.1 CPU、单线程 |

完整来源、许可证、模型与包哈希、隔离安装方式见 `experiments/key-evaluation/README.md`。现有 libkeyfinder 为 GPL-3.0-or-later；Essentia 包标注 AGPL-3.0；S-KEY 源码为 MIT。实验不改变或解决整个产品的发布许可问题。下载的运行环境、源码及模型保留在忽略目录 `artifacts/key-engine-evaluation/`，未加入生产依赖清单。

S-KEY checkpoint 先做静态检查，再以 `weights_only=True` 和明确的 NumPy 类型白名单加载；不使用上游不受限的 pickle 加载入口。模型每次启动验证固定 SHA256。没有修改上游源文件。

## 方法

- 机器：本机 Windows 11、AMD Ryzen 7 9850X3D；保留用户现有程序运行，非独占硬件环境。
- 原始输入：48 kHz stereo，所有引擎使用相同请求端点 6–12 秒，窗口最多回看 8 秒。没有采集系统声音、麦克风或播放器；全部本轮音频来自确定性合成器。
- 统一反相保护、RMS 门限 `5e-5`、目标 RMS `0.10`、最大增益 `64` 和截幅。Rust 使用 f64 mono；WASM / 模型使用 f32。策略一致，不声称浮点逐位一致。
- S-KEY 额外进行 48 kHz → 22.05 kHz 重采样，计入耗时；不再做其文件加载器的额外峰值归一化。模型保存的 audio duration 配置是 15 秒，本实验特意使用符合当前产品的 6–8 秒窗口。
- 原生增量 FFT 网格沿流起点对齐，左边界可能少于一个 hop 的差异保留；相同请求窗口不意味着各引擎内部特征相同。
- 三轮串行运行；不与编译、测试或另一项基准并行。候选模型加载、首个冷运行、正常窗口计算分开记录。
- “首次稳定”是离线复用 3/5 连续票规则的音频时间端点，不是新的 GUI 实测延迟。
- 真实歌曲 ground truth 未提供，相关 accuracy 字段为 `null`。没有用搜索结果、歌曲名或引擎自己的答案充当标签。

## 现有普通调用：默认编译与 O2

使用原有 `key_benchmark --synthetic`，**不请求诊断分数**。四个合成和声用例各 32 秒，6–32 秒每秒分析。每种构建三轮，顺序为默认/O2、O2/默认、默认/O2；每种模式有 312 个非首次窗口，冷上下文另计。

| 路径 | 默认平均 / P95 | O2 平均 / P95 | 确认结果 |
|---|---:|---:|---|
| 增量（当前路径） | 74.08 / 91.25 ms | **49.16 / 52.45 ms** | 所有候选和确认序列一致；首次正确均第 9 秒 |
| 批处理（基线） | 255.73 / 344.16 ms | 75.81 / 82.26 ms | 同上 |
| 增量首次上下文，平均 | 201.29 ms | 56.29 ms | 单独计量，不混入 warm 均值 |

依据：`artifacts/key-engine-evaluation/ordinary-native/summary.json` 与六份原始运行 JSON。普通调用 6 次完整运行之间，全部候选 / 确认序列一致。

O2 仅用于 libkeyfinder C++ 和桥接层，FFTW 保持原来的 Release 构建，Rust profile 相同。保存的编译器命令 `o2-compiler-evidence.log` 包含实际 MSVC `-O2`，不是只根据配置字符串推断。`KEYFINDER_NATIVE_OPT_LEVEL` 只接受未设置、0、2；无效值在 CMake 前失败。**没有把 O2 改成项目默认值。**

上述约 33.6% 是本机 **Rust debug 构建中，原生 C++ 默认 O0 对比 O2** 的计算耗时降幅，不是 release 构建的收益，也不是播放器切歌到 UI 显示调式的端到端耗时降幅。

## 三引擎窗口对比

下表只为便于比较同一批有标签和声，统一使用端点 7–12 秒，4 用例 × 6 窗口 × 3 轮 = 72 个记录。原生此表开启诊断分数，与上一节普通调用实验分开。原始 JSON 同时保留所有负样本，没有删去它们。

| 引擎 | 平均计算耗时 | P95 | 这四个用例首次稳定正确端点 |
|---|---:|---:|---:|
| libkeyfinder 默认 | 71.70 ms | 75.75 ms | 9 s |
| libkeyfinder O2 | 48.93 ms | 52.32 ms | 9 s |
| Essentia WASM | **11.96 ms** | 13.47 ms | 8 s |
| S-KEY CPU | 188.62 ms | 200.28 ms | 8 s |

这些不是严格相同系统边界的部署成本：原生记录包含 Rust downmix / 增益、诊断及少量指标组装；候选 worker 的记录排除了共享 JS downmix、管道/base64 和输入校验，包含自身特征与推理（S-KEY 包含重采样）。进程启动另计，不能把表中的倍数直接宣称为产品端到端加速倍数。

本轮 worker 启动至就绪：Essentia 约 91 ms，S-KEY 约 2293 ms；第一个冷推理分别约 26.90 ms、2721.00 ms。其后才进行 warm 测量。

诊断默认/O2 对齐比较 168 个窗口：候选和请求边界差异 **0**；24 分数中的最大绝对差 **0**。这是当前合成用例的回归结果，不是对任意输入的数值等价证明。

依据：`three-engine-default.json`、`native-o2.json`、`derived-summary.json`，均在 `artifacts/key-engine-evaluation/`。

## 保留的负样本结果

以下为三轮结果中的第一轮；完整逐窗口候选、分数和确认过程均保留。没有为这些没有明确调式的信号计算“正确率”。

| 输入 | libkeyfinder | Essentia | S-KEY |
|---|---|---|---|
| 静音 | 不输出 | 原始猜 A 小调；共享静音门限抑制 | 原始猜 B 小调；共享静音门限抑制 |
| 单一 440 Hz 纯音 | 确认 A 大调 | 确认 A 小调 | 确认 A 小调 |
| 固定种子噪声 | 确认 G♯ 小调 | 确认 F♯ 大调 | 候选不稳定，12 秒内未确认 |
| 无固定音高的点击鼓点 | 确认 G♯ 小调 | 确认 C 大调 | 确认 F 小调 |

因此，多次一致的输出仍可能只是模型对非调性声音的稳定猜测。后续“证据不足时不出调式”的规则需要独立验证，不能在此临时加阈值掩盖负样本。

## 开发者可直接参考的入口

- `src-tauri/examples/key_window_probe.rs`：只读 stdin PCM，输出候选、24 分数、best/runnerUp/margin、边界、耗时和构建身份。
- `src-tauri/src/key_detection/engine.rs`：`detect_with_diagnostics`；原有 `detect` 不请求分数计算。
- `src-tauri/src/key_detection/stream.rs`：`analyze_with_diagnostics`；原有 `analyze` 保持原接口。
- `experiments/key-evaluation/compare.mjs`：三引擎对比、显式本地音频 manifest、证据与时序。
- `experiments/key-evaluation/measure-native.mjs`：原有普通调用的三轮串行 A/B 测量。
- `experiments/key-evaluation/README.md`：复现命令、管道协议、来源及限制。

余弦相似度、Essentia strength 和 S-KEY softmax 不是同一种分数，也未校准成正确概率。候选分数仅对开发工具开放，没有增加 UI 或 IPC 字段。

## 本轮保留的操作记录

第一次 RED 使用 Cargo integration-test 目标，Cargo 意外尝试重建主程序；Windows 阻止替换正在运行的 exe。随后测试移到 `--lib`，后续所有 Cargo 命令只选择库或明确 example。两份主 exe 的哈希在事件后确认未变。未停止、重启任何用户播放器、宿主或应用。失败日志保留在 `native-red.log`，不能把这次命令称为纯库测试成功。

独立 Task 1 审查要求补充分数数值映射和多输入 parity，并建议 stdin 端到端测试；均补齐后复审通过。诊断同分顺序按上游 A-major-first 保留，C-first 仅为公开分数数组的展示顺序。

本轮工程取舍：没有 Git 仓库，因此采用原文件副本、哈希、保留 diff/日志，不初始化仓库；回滚便利性不如 Git。Windows 上先用官方 Essentia WASM，代价是其耗时不能代表原生 C++。早先代理未产出实现后由当前会话完成，代价是减少独立实现上下文，之后通过独立审查和回归补充检查。

## 回归与恢复证据

- 最终原生回归：**88 项库测试、15 项原有 evaluator 测试、3 项新 probe/options 测试通过**（`final-native-tests.log`）。未修改的 `Stabilizer::begin` 在普通非测试库构建中仍有既存 dead_code 警告；未为消除警告改动生产稳定逻辑。
- 实验工具初轮完整回归：11 项通过，包括实际 Essentia / S-KEY 推理与真实 native stdin 测试（`final-lab-tests.log`）。后续 manifest 边界修复的结果独立保留，不覆盖该日志。
- 修复后再次独立执行完整实验工具测试：**16/16 通过，0 失败、0 跳过**，包含实际三引擎调用；日志为 `final-lab-after-review-tests-20260930-020711.log`。
- `default-restored-probe.json`：`rustProfile=debug`、`nativeRequested=default`、`nativeEffective=0`。最终进程环境未设置原生优化 override。
- `after-verification.json`：主程序 debug / release 两份 exe、App.tsx、音频状态模块和 stability.rs 均与本轮开始哈希一致；原调试进程 PID 79504 的路径、创建时间一致，仍在运行。PID 仅作身份核对，没有对其执行停止/启动操作。
- 最后复核 `final-protected-check-20260930-020754.json`：上述五项哈希和进程身份再次一致，优化环境变量未设置；实际 stdin 探针 `final-default-identity-20260930-0209.json` 再次确认 default / 0 / debug 构建身份。
- 主调试 exe SHA256：`17D3FB8309039AEF4094B125EB0B7D4EDCE2BACE84CFA29D3267C2B094F3DC4A`。
- 主发布 exe SHA256：`78FB7563BBEA3EC48E1478CFFC9AAFC3415D7CD65DC817140FC8CD5C550396CB`。
- 本轮没有进行新的 GUI 实际识别验收或真实歌曲准确度验收，也没有生成、启动或发布新版应用。

离线 manifest 工具经独立审查后另做了边界修复：仅接受经文件头校验且显式指定 demuxer 的 WAV/FLAC/MP3/Ogg/ADTS AAC/AIFF 单文件输入，拒绝播放列表及可能引用外部文件的容器；按用例惰性解码，不同时保留 100 首 PCM；允许选择 batch / incremental，并把 batch 全部标作重建上下文；内部短请求 ID 不受长显示名称影响。完整实验工具测试 **16/16 通过**（`task-2-fix-full-tests.log`），生成的长 ID WAV 也通过三个真实引擎的 batch 接线 smoke。这是工具边界验收，不是新增真实性能或歌曲准确率结果。原三轮性能文件保持不变。

该边界修复的独立复审确认四项发现全部解决，未发现新的具体回归。允许的其他编码格式尚未逐个解码实测；文件头校验和固定 demuxer 不是恶意媒体沙箱。当前只对生成的 WAV 与播放列表拒绝路径进行了针对性验收。

## 最终整体审查

独立整体审查结论为 **Spec PASS / Quality PASS（保留非阻塞小项）**，没有 Critical 或 Important 发现。审查者从保存的数据独立复算了三引擎耗时与默认/O2 的 168 窗口一致性；没有重跑基准或操作用户进程。完整记录在 `.superpowers/sdd/2026-09-29-key-engine-evaluation/final-review.md`。

仍保留三类明确限制，均不代表已完成验证：

- 将工具泛化到任意外部 benchmark 可执行文件前，应校验构建身份；当前普通 A/B 脚本的 debug 标签不是自动鉴别任意二进制的结果。本轮另有编译命令、哈希与探针身份共同证明。
- 完全同分时的排序经过上游源码核对与 Rust 映射测试，但没有原生 chromagram 完全同分的端到端用例。
- WAV 之外的允许输入格式尚未逐个解码实测，不能把输入白名单当作已经验证的全格式兼容性。

README 的主测试命令已经更正为 `npm test --prefix experiments/key-evaluation`，覆盖当前全部 16 项，而非早先只列出两个测试文件的局部命令。本轮交付仍仅为离线工具、开发接口和实验报告，不是正式版发布或生产引擎切换。

这处文档修正另经一次范围限定复审，结论为已解决、未发现新增问题（`final-fix-review.md`）。全部计划项已经完成；保留上述限制和所有原始证据，不自动部署实验构建。
