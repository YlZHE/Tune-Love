# Essentia 真实音乐短窗口试验

**后续独立留出验证：** 已完成八套特征/模板配置的开发集比较及 46 首、46 位新艺人的留出试验。开发集选出的 36 格 HPCP + EDMA 未在留出集保持优势，仍不替换主引擎。详见 [准确度独立留出试验](2026-09-30-key-accuracy-holdout-study.md)。下文保留原始试验口径与结果。

## 决策摘要

**本轮不将 Essentia 提升为主识别引擎。** 它的记录计算耗时较低，但在这批预先选定的真实音乐上，沿用当前 3/5 连续票规则后，没有比 libkeyfinder 得到更高的参考标签一致率。

这是一项离线、小规模、分层抽样试验，不是应用整体准确率认证，也没有进行 GUI 接入或 Auto-Tune 控制验收。原引擎、UI、IPC、采集、六秒最短窗口和确认规则均未修改；没有构建、替换或重启主程序。

## 数据与事先固定的抽样

- 标签：FMAKv2 官方 `fmakv2.csv`，Zenodo 记录 **12759100**，DOI `10.5281/zenodo.12759100`。下载文件 MD5 为 `3b2d16784ffbda850c8ddf0519478bfd`，与官方记录一致。
- 音频：FMA 官方 `fma_small.zip` 的 30 秒 MP3 选段；通过 `track_id` 对应标签，并通过官方 `fma_metadata.zip` 的 `tracks.csv` 保留作者、类型与每首的许可声明。
- 两者交集中有 606 个满足本次大小及格式边界的样本。使用固定种子 `autotune-helper-fmakv2-pilot-v1`，按 track ID 的 SHA256 排序；每个大小调最多取两首，不重复作者。抽样不参考任何引擎输出。
- 最终 **48 首、48 位作者、24 个大小调各两首**。类型为器乐 9、电子 5、流行 8、民谣 8、Hip-Hop 6、摇滚 6、国际音乐 4、实验音乐 2。
- 选样文件在推理之前写入并固定，SHA256：`bff277f8c14c200097b2a2f282ee035ebb8dc74588d9694e390d0fa3c0088080`。

最初检查过 Beatport EDM Key Dataset，但 Essentia 官方说明其默认 `bgate` 模板参考该数据集，因此没有用它支撑独立比较结论。保留已下载的小型标注资料，不使用其音频或分数。FMAK 的 Zenodo 音频分卷范围请求返回 403 后，改用 FMA 作者明确提供的公开音频源；没有绕过账号、授权或访问限制。

音频使用 HTTP Range 按成员读取，复用 `remotezip 0.12.6` 和 Python `zipfile`，没有下载整个大型音频包。成功的准备与音频范围请求合计约 66.5 MB；48 个原始 MP3 合计 47,065,916 字节。每个完整 ZIP 成员的 CRC 均已检查，另保存每首 SHA256；**没有声称校验过整个远程 ZIP 的总哈希**。

## 方法和标签边界

复用既有 `experiments/key-evaluation/compare.mjs`，不修改引擎或调参：

1. 每首以公开 30 秒选段的开头为起点，评测第 6–12 秒七个端点，最多回看八秒。
2. 三引擎接收同一请求窗口，保留既有反相保护、音量处理和静音门限。
3. 普通消费者结果以离线重放的首次 3 票、替换 5 票规则为准；不只挑选最后一窗的原始候选。
4. 核心比较为 libkeyfinder O2 与 Essentia WASM 0.1.3 的固定 `bgate` 默认配置。既有 runner 同时运行并保留 S-KEY 原始输出；本轮没有确立 S-KEY 模型与该数据集的独立性，因此不拿它做选型排名。
5. 每个样本运行一轮；后续另用默认 O0 的原生探针进行结果一致性对照。所有运行串行，不与构建、测试或另一项基准重叠。

**标签通过原始 FMA track ID 对应，不是本轮对每个 30 秒选段或 6–8 秒窗口重新进行的人工标注。** 因此下面报告的是“短窗口确认结果与 track 级专家参考标签的一致性”，不能宣称为逐窗口音乐学真值、全曲准确率或用户实际曲库准确率。旧 manifest 的 `whole 30-second clip label` 注释应按本段的更严格限制解读；原始文件不改写，保留取证链。

抽样刻意均衡 24 个调性，且每首来自不同作者；这不是按用户曲库自然分布抽样。各音乐类型的数量也不足以建立该类型的可靠排名。未单独人工标注中文人声、808、转调、纯鼓前奏等条件；不能把这次试验当作它们已全部通过。

## 结果

在第 12 秒的最终确认状态：

| 指标 | libkeyfinder O2 | Essentia WASM |
|---|---:|---:|
| 与参考标签完全一致 | **22 / 48（45.8%）** | 20 / 48（41.7%） |
| 已确认但与参考标签不一致 | 25 | 27 |
| 未形成确认 | 1 | 1 |
| 第 8 秒已经确认且一致 | 19 / 48 | 17 / 48 |
| 最后一窗的原始候选一致 | 16 / 48 | **21 / 48** |
| 最终一致样本中的首次正确确认中位端点 | 8 秒 | 8 秒 |

最后一窗候选和最终确认两个指标的排名不同，说明不能只挑其中有利的一项；用户实际显示的是确认状态。本轮没有调整稳定器来改变排名。

配对计数：两者都一致 14 首；仅 libkeyfinder 一致 8 首；仅 Essentia 一致 6 首；两者都未正确确认 20 首。探索性精确 McNemar 双侧检验 p = **0.7905**；这个小试验没有建立两者谁更准确的证据，也不证明两者等效。

错误类型按最终已确认结果划分：

| 类型 | libkeyfinder | Essentia |
|---|---:|---:|
| 同主音大小调混淆 | 4 | 7 |
| 关系大小调混淆 | 4 | 3 |
| 其他调性不一致 | 17 | 17 |

Hip-Hop 子组为 3/6 对 1/6，电子子组均为 4/5，流行子组均为 5/8。这些只作为错误定位线索，样本太少，不能推出对应音乐类型的普遍优劣。逐首结果及完整类型计数保留在 `pilot-summary.json`。

## 计算耗时

为统一端点，以下都取第 7–12 秒的 288 个调用记录：

| 引擎 | 平均 | 中位数 | P95 |
|---|---:|---:|---:|
| libkeyfinder O2 | 66.42 ms | 51.99 ms | 117.27 ms |
| Essentia WASM | 11.29 ms | 11.07 ms | 13.28 ms |

原生计时包含预处理和诊断分数；WASM 不含共享 JS 预处理、管道/base64 传输。这是单轮交互桌面记录，不是相同边界的部署成本，也不是 release 或 GUI 端到端速度比较。原生这 288 个调用中有 70 个 batch fallback、74 个 reset（两类可能重叠），不能把它们都称为无重置的纯增量 warm 调用。

另外，较低计算耗时没有在这批样本里转化为更早、更多的正确确认。不要用本表直接设置更低票数或更短最小窗口。

## 验证与未改动范围

- 实际完成 48 首 × 7 窗口 × 3 引擎，共 1,008 个窗口结果，无以假输出替代的后端；另有 336 个默认原生对照窗口。
- 输入独立检查：48 个 ID、48 个作者、24 调性各两首；所有 expected 与原始标注解析一致；音频 SHA256 全部吻合冻结的下载清单。
- 默认 O0 与 O2 的 336 个窗口候选、请求边界及全部确认序列相同，有诊断分数的窗口最大绝对分数差为 0。22/48 并非 O2 改变分类结果造成。
- 直接使用 PowerShell 从原始结果独立重计：22 / 25 / 1 与 20 / 27 / 1，与汇总脚本相符。
- 完成测量后执行 `npm test --prefix experiments/key-evaluation`：**16/16 通过，0 失败、0 跳过**。没有重新构建 GUI 或原生库。
- 主程序 debug/release 两份 exe、`App.tsx`、音频模块和 `stability.rs` 的前后 SHA256 相同。没有操作播放器、DAW、Auto-Tune 或用户进程，也没有新开采集、保存用户音频或上传媒体。
- 原 runner 的全局 `realSongAccuracy` 字段仍为 null；本报告的预选小样本计数独立呈现，不能冒充应用全局准确率。

## 证据与复现

证据根目录：`artifacts/key-engine-evaluation/real-music-pilot-20260930/`。

主要文件：`selection.json`、`manifest.json`、`download-evidence.json`、`input-validation.json`、`three-engine-pilot.json`、`native-default-control.json`、`native-control-parity.json`、`pilot-summary.json`、`final-lab-tests.log`、`protected-before.json`、`protected-after.json`。

`prepare_pilot.py` 和 `analyze_pilot.mjs` 是本轮临时研究辅助脚本，保留用于解释证据，不作为已验收或待发布的产品模块。下载依赖使用单独 `fetch-deps` 目录，没有修改 S-KEY venv 或生产依赖。音频各自的许可不同，作者与许可字段保存在选样/下载清单；本轮只在本地实验目录留存，不随产品发布。

```powershell
# 复跑时必须使用新的输出文件名，禁止覆盖本轮证据。
node experiments/key-evaluation/compare.mjs --manifest artifacts/key-engine-evaluation/real-music-pilot-20260930/manifest.json --probe artifacts/key-engine-evaluation/bin/o2-v2/key_window_probe.exe --repeat 1 --output artifacts/key-engine-evaluation/real-music-pilot-20260930/new-comparison.json
```

研究过程还保留了早期的元数据大小保护中止日志：官方 tracks.csv 展开后约 260 MB，超过最初 128 MiB 界限；确认其压缩大小约 19.5 MB 后，改为有上限的流式 CSV 解析。未放开网络总量、单次范围请求或成员路径边界，没有删除失败证据。

## 后续建议

先保留当前主引擎。把 Essentia 继续作为候选，后续另行验证“声音证据不足时暂不锁定”“同主音/关系大小调歧义”与不同模板/特征设置，使用独立开发集与留出集，避免在这 48 首上边调边宣称准确率提高。本轮没有自动实施这些新规则或生产切换。

来源：FMAKv2 Zenodo 记录 12759100；FMA 官方 `mdeff/fma` 仓库及其音频、元数据下载地址；Essentia 官方 `Key` / `KeyExtractor` 文档中对 `bgate` 的说明。数据与算法版本均通过上述原始文件和既有引擎固定版本记录追溯。
