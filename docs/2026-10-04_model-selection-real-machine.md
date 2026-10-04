# 模型选择真机测试记录（2026-10-04）

计划：`docs/superpowers/plans/2026-10-04-model-selection.md` Task 9。

- **版本**：分支 `model-selection`，调试构建（`tauri.verify.conf.json`）。
- **构建（SHA-256，最终一版）**：
  - `tune-love.exe`：`fdace0071919fb1c93da9cf8356d59e517f13088728d65d47ef3e14c51d277cd`
  - `devocal-engine.exe`：`1fa3bcd2d121d8f5f7cec00cb59b0db923743c9429cf43af1b2ca5bb9a3b6487`
- **授权**：用户于 2026-10-04 选定以下三项。本地 case 记录在 `work/model-selection-20261004/`，不入库。
  - 开关去人声的听感；
  - 显卡自检；
  - 人为制造超时后的退回。
- **播放器**：每次接管前都核对了身份，是 Folia 根进程 18668（`…\Programs\Folia\Folia.exe`，创建于 03:08:08）。
- **本机**：NVIDIA GeForce RTX 5070，驱动 32.0.16.1088。

**结论：**
- 三项全部通过。
- 测试中发现 2 个问题，已在分支上修复，并在真机上复测通过：
  1. 主窗口读到的模型选择是旧的；
  2. 打开去人声时会重复播放一小段，用户不接受。
- “4 种组合各播 5 分钟”的测量用户没有选。因此 GPU/CPU 占用（≤ 30% 的标准）和丢音率**没有实测**。

## 模型发布

- **Release**：`models-v1`（https://github.com/YlZHE/Tune-Love/releases/tag/models-v1）。
  - 第一次用 `gh` 上传时，令牌权限不足，返回 403。用户重新登录 `gh` 后上传成功。
- **公开地址的校验**：从公开下载地址取回文件，与 `src-tauri/models.json` 对比，两项都一致。

  | 文件 | 字节数 | SHA-256 |
  |---|---|---|
  | `bytesep-mobilenet-1s.onnx` | 9,600,617 | `d70b6b…0d4885` |
  | `htdemucs-ft-vocals-1s.onnx` | 304,759,764 | `fb173f…dd5ab4` |

- **应用内下载**：直连，SHA-256 都一致。
  - bytesep：确认框写明了来源、大小、许可、署名与“已修改”说明。
  - HTDemucs：用时 18 秒。确认框另外写明“仅支持 GPU”和“训练数据来源不明，仅限非商业使用”。
  - StemgenRT：因为之前已经同意过，没有再弹确认框，这是设计如此。

## 步骤与结果

1. **开关听感（bytesep，GPU）**
   - **第一次试听，暴露问题 1**：在设置窗口选了 bytesep + GPU，主窗口点开启后，发出去的仍是默认的 StemgenRT。当时本机没有安装 StemgenRT，所以状态是 `model_not_found:stemgenrt-hop128`。这一步没有接管播放器（held=false）。
     - 根因：偏好模块在页面加载时就缓存了选择，之后只靠“存储变化”事件的订阅来刷新。主窗口没有订阅，所以一直用加载时的值。
     - 修复：`e0d74d2`。每次读取时，先和存储里的原始字符串比较，变了就重新解析。补了单元测试。
   - **修复后，进入去人声**：
     - 约 1.3 秒进入去人声；
     - 状态：模型 `bytesep-mobilenet-1s`，设备 `gpu`，无提示；
     - 延迟约 278 ms：模型 230 ms，加上声卡缓冲；
     - 负载比 0.018。
   - **用户听到问题 2**：打开时会把一小段重复播放一次。这是原方案 A 的固有表现，用户不能接受。用户改选了下面的方案，已写入 spec §4.2：
     - 打开：原声淡出，静音“模型延迟 − 当前延迟”，再从停下的内容处淡入去人声版本；
     - 关闭：去人声对齐到“按模型延迟的原音”，不跳内容。之后保持这个延迟，等到输入出现一段静音或不连续时，再无声地降回低延迟。
     - 实现：`9db1a0e`。独立审查逐帧推导，确认内容连续、降回时只丢静音帧。
   - **用户复测**：“没有重复了”。
2. **显卡自检**
   - **bytesep**：第一次用 GPU 时完成自检。`models\bytesep-mobilenet-1s\gpu-check.json` 中记录的键是“RTX 5070 10de:2f04|32.0.16.1088|d70b6b…”，值为通过。
   - **HTDemucs**：
     - 在去人声开着时从 bytesep 切换过去。加载期间 bytesep 一直在正常播放。
     - 第一次加载（含 CPU 参考计算的自检）约 **18.3 秒**后完成切换。
     - `models\htdemucs-ft-vocals-1s\gpu-check.json` 记录通过。
     - 切换后的状态：设备 `gpu`，延迟约 288 ms，负载比约 0.018。
3. **人为制造超时后的退回**（调试构建，`TUNE_LOVE_WINDOWED_SLOW_MS=35`，即每次推理额外加 35 ms）
   - **HTDemucs（GPU）**：
     - 约 4.7 秒进入去人声；
     - 约 19.7 秒时 GPU 过载，应用改用 StemgenRT，去人声保持开启；
     - 状态为设备 `cpu`、提示 `gpu_overloaded`；
     - 设置页显示“GPU 负载过高，已改用 StemgenRT”。
   - **bytesep（GPU）**：
     - GPU 过载。引擎日志：`model bytesep-mobilenet-1s fell behind on the GPU; no GPU for it in this session`；
     - 改用 CPU 重新加载，状态为设备 `cpu`、提示 `gpu_overloaded`，设置页显示“GPU 负载过高，已改用 CPU”；
     - 注入的延时对每次推理都生效，所以 CPU 配置也跟着过载，最后进入 `fallback(overload)`，并保持 407.5 ms 的延迟。这在预期之内。
4. **收尾**
   - 每次停止应用前都先释放接管，并确认 held=false。
   - 恢复文件已删除；环境变量已清除。
   - 三个模型都保留在 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\`，是否删除由用户决定。

## 偏差与遗留

1. **没有做 4 种组合各 5 分钟的测量**，用户没选这一项。GPU ≤ 30% 的标准还没有在真机上验证，只有负载比可参考（GPU 上约 0.02）。
2. **`gpu-check.json` 的位置**：写在每个模型自己的目录里，不在 `models\` 根目录。这是因为实现时把“模型目录”取为模型文件所在目录（Ruling 8）。功能没有影响。
3. **新的统计字段默认不写进日志**：引擎的日志里没有 `win_timeouts=` / `win_duty=` 这一行。它们只出现在诊断输出里，Metrics 里有。下次测丢音时，需要开诊断输出。
4. **HTDemucs 的超时退回要约 15 秒才触发**，原因没有细查。可能是在等去人声生效后的预热和 Metrics；也可能是早期窗口的平均占用还没攒满 10 秒（I1 修复后，平均占用要攒满才判过载），而“连续 3 次超时”没有在更早的时候触发。下次测丢音时一并核对。
5. **去人声开着时换模型**：会先跳过一小段，再停顿一下（已记录在案）。只有“开、关”做到了既不重复也不跳过。
