# 去人声延迟测量手册

本文只写方法，不含任何本机数据。结果、校准输入表和异常记录写进各次测量的不入库报告里。

适用对象：`devocal` 引擎的“接管播放器 → 直通 / 去人声 → 释放”链路的延迟与进程回环交付偏移。测量全程是被动的：探针只录音，不连引擎，不改任何会话音量；会改变播放器状态的只有应用本身（接管、释放），那些操作由用户在界面里做，或按下文“驱动方式 B”由测试者发命令。

## 0. 安全与授权边界

- 真机测量须由用户本人同意，并说明：用哪个播放器、应用会接管它、需要在界面反复开关去人声并释放、会同时录两路回环音频。
- `--small-period` 那一次会临时改变该输出端点的共享模式引擎周期，影响所有向该端点出声的程序，必须**单独**征得同意（见第 6 节）。没有同意就不做，并在报告里记为未做。
- 每次接管前都核验播放器身份（第 2 节），不沿用上一次的 PID。
- 不强杀任何进程；关闭测试应用走正常关闭。
- 不得用 1 次运行的数字代替统计；每个状态至少 5 次有效运行。

## 1. 构建

在仓库根目录：

```
node scripts/build-devocal-engine.mjs --release
```

在 `devocal` 目录（工作区）：

```
cargo build -p devocal-engine --release --examples
```

示例程序在 `devocal\target\release\examples\` 下：`latency_probe.exe`、`split_probe.exe`、`device_periods.exe`。

应用按 2026-10-03 验收的方式构建测试版（与正式 release 配置不同，带 WebView2 调试端口）：

```
npm run tauri -- build --debug --no-bundle --config src-tauri/tauri.verify.conf.json
```

构建后记下所测提交（`git rev-parse HEAD`）和各 exe 的 SHA-256，写进 `scope.md`。应用与引擎须由同一提交构建；示例程序若在更早的提交上构建，不得用来判定有效性（见第 5 节“旧转储”）。

先正常关闭正在运行的本应用（关闭前核对其路径与创建时间），再以环境变量 `DEVOCAL_DIAG=1` 启动 debug exe，使引擎继承该变量并每秒输出一行诊断。不要同时运行旧 release 与测试 debug（WebView2 可能报 `0x8007139F`）。

引擎的 stderr 追加写入应用数据目录下的 `logs\devocal-engine.log`（超过 1 MB 在下次引擎启动时轮转为旧文件）。每次运行前后都要从该日志摘取对应 `run=` 的行（第 4 节）。

## 2. 身份核验与记录

每次接管前，记录播放器的：根 PID、映像路径、进程创建时间（用 `Get-CimInstance Win32_Process` 取 `ProcessId`、`ExecutablePath`、`CreationDate`），写入 `scope.md`。探针的 `--pid` 必须是播放器的**根**进程 PID（探针录的是其进程树）。PID 或创建时间与上次不同，视为新播放器实例，重新记录。

同时在 `scope.md` 记录：

- 播放器原始音量（接管前的会话音量，用于收尾核对）。
- 输出端点（默认渲染端点）的名称。
- 歌曲：应是持续有内容的音乐（长时间无静音段，探针在静音窗口不出结果）。每次运行用同一首或同一段落，不在运行中途切歌、暂停或拖动进度。

## 3. 状态定义

- (a) 直通 a：接管后**从未**开过去人声的直通（`phase = passthrough`）。
- (b) 直通 b：开、再关一次去人声之后的直通（`phase = passthrough`）。
- (c) 去人声：去人声开启并稳定至少 10 s（`phase = devocal`，`fallbackReason` 为空，`inputSilent = false`）。

状态用 `get_devocal_status` 的 `phase`、`held`、`fallbackReason` 判定，并与诊断日志里的 `stage=` 对照；状态与目标不符的运行作废重来。

## 4. 驱动应用的两种方式

**方式 A：用户在界面操作。** 用户点界面上的去人声开关和“释放”。测试者只喊口令（“开始录”“释放”“结束”），并负责启动探针、读状态与日志。

**方式 B：测试者通过 WebView2 调试端口驱动。** 与 2026-10-03 验收相同：用测试配置构建的应用开放本地调试端口 19224（`http://127.0.0.1:19224`，仅回环），用 CDP 连接后在页面里用 Tauri 的 invoke：

- 开去人声 / 关去人声 / 释放：`devocal_command` 的参数为 `{request: {action: "enable"}}`、`"disable"`、`"release"`。
- 读状态：`get_devocal_status`，返回 camelCase 的 `DevocalStatus`（`phase`、`held`、`latencyMs`、`loadRatio`、`fallbackReason`、`sessionOverridden`、`inputSilent`、`waitingForPlayer`、`error`）。

两种方式在报告里写明用了哪一种。方式 B 不操作播放器本身，只发应用命令；不操作播放队列、播放进度和其他应用。

## 5. 单次延迟运行（`latency_probe`）

方法原理（读报告前先理解）：两路录音（播放器进程回环、系统输出回环）之间的**绝对滞后不是延迟**，两条采集路径各有未知且不同的延迟，只是每次运行间大体稳定。**唯一可信的量是同一录音内滞后的跳变**：释放（引擎脱离，输出内容向前跳）时跳变等于负的延迟，在报告里是 `releaseLatencyMs`；接管（`attachLatencyMs`）时跳变等于正的延迟。每个状态在跳变前后至少保持 5 s（探针忽略短于 1 s 的片段，3 s 以上更稳）。

一次运行的步骤，`<evidence>` 为不入库的证据目录，`<state>` 取 `a`/`b`/`c`，`<n>` 为 1 到 5：

1. 把播放器置于要测的状态（按第 3 节），确认 `held = true`，`phase` 符合；核验播放器身份（第 2 节）。
2. 摘录诊断日志的当前 `run=` 编号，记下渲染打开行（形如 `devocal audio: run=<n> render <描述> (period <P_frames> frames, buffer <B>)`），换算周期毫秒数：`P_ms = P_frames / 44100 * 1000`。渲染打开行的周期以引擎帧计，即 44.1 kHz，与端点的混音采样率无关：自动转换路径把设备默认周期按 44.1 kHz 换算（向上取整），低延迟路径只在混音格式正是 44.1 kHz 时使用。所以**不要**用 `device_periods` 的 `mixRate`（通常 48000）换算，例如 `period 441 frames` 是 10 ms。
3. 启动探针：

   ```
   latency_probe --pid <根PID> --seconds 20 --dump <evidence>\<state>-<n> --out <evidence>\<state>-<n>.json
   ```

   可用 `--endpoint <端点 id>` 指定渲染端点，默认是默认渲染端点；`--seconds` 取值范围 (0, 600]。`--dump` 写 `<prefix>-source.f32`、`<prefix>-output.f32`、`<prefix>-times.json`（44.1 kHz 立体声 f32 小端）。
4. 接管状态保持 ≥ 5 s 后，用户（方式 A）或测试者（方式 B，`release`）释放；释放后继续录 ≥ 5 s，直到探针结束。20 s 的录音内释放应落在第 8～12 s。
5. 读报告里的 `releaseLatencyMs`，并用离线分析复核：

   ```
   latency_probe --analyze <evidence>\<state>-<n>
   ```

   两次结果应一致。`--analyze` 只读转储，不需要 `--pid`、不需要设备，且只能与 `--out` 同用。报告里应有且仅有一次可信的跳变；没有跳变（只有一个片段）、出现多于一次的跳变、或跳变对应的前后片段过短，则本次作废。
6. 运行后再从诊断日志摘录该 `run=` 在释放前最后 5 s 的几行：`lat_ms`（引擎自报的站立延迟，含系统段 `system_path_ms(P)`，见第 9 节；当前构建为 3.2 个周期，P0 测量所用的 4115cde 构建为旧的 3 个周期）、`headroom`（渲染余量帧数）、以及是否出现 `underruns`。运行前后的渲染打开行也记下，确认运行中周期未变。
7. 释放之后，由用户（A）或测试者（B）再次接管并回到下一次运行所需的状态；每次运行都按第 2 节重新核验身份。

### 状态 (a)、(b)、(c) 的取得

- (a)：接管后不要开过去人声，直接做 5 次运行。每次运行结束于释放；再次接管后仍是“从未开过”的直通，故 5 次都是 (a)。
- (b)：接管后开、关一次去人声，回到直通后保持 ≥ 5 s，再运行。每次运行都要重做开→关。
- (c)：去人声开启后等待稳定 ≥ 10 s（`phase = devocal`），再运行。

每个状态取 5 次**有效**运行，报告中位数与最大值（以 `releaseLatencyMs` 的绝对值计），并列出各次的原始值；`E_a`、`E_b`、`E_c` 取同批运行释放前 5 s 内 `lat_ms` 的中位数。中间的释放、再接管均由用户在界面操作（方式 A）或测试者发命令（方式 B）；状态之间不要手动改播放器音量。

## 6. 进程回环交付偏移（`split_probe`）

拆分探测回答设计稿 P0 的问题：进程回环的交付偏移是否随引擎周期变小而变小。它与第 5 节不同，**不接管**（`held = false`），并在两路回环之间对齐内容，报告进程回环比端点回环晚（正）或早（负）交付多少。

```
split_probe --pid <根PID> --seconds 20 --dump <evidence>\split-normal --out <evidence>\split-normal.json
```

可选 `--endpoint <端点 id>`。`--dump` 写 `<prefix>-source.f32`、`<prefix>-output.f32`、`<prefix>-packets.csv`、`<prefix>-meta.json`。离线复核：`split_probe --analyze <evidence>\split-normal`（同样只与 `--out` 同用）。

### 报告读法

- 引用 **`deliveryOffsetUs`**。它已校正“输出包内排在匹配帧之后的帧数”造成的偏差（一个包在读到其最后一帧后才交付，原始差偏大至多一个输出包）。`deliveryOffsetRawUs` 是原始差，只作对照，不引用。
- 只接受 **`runValid: true`** 的运行。为 false 时看 `invalidReasons`：内容滞后不恒定（`lagUnstable`）、任一路报告数据间断（discontinuity）、无对齐结果，或小周期静音流中途结束（`smallPeriod.endedEarly`，带错误和时间）。
- **`lagUnstable: true` 的运行直接丢弃**（逐窗滞后 p05 到 p95 的宽度超过 4 帧，常见于暂停或某一路丢包）。重新录。
- **不得用 split_probe 提交 b1a611e 之前的转储判定有效性**：那些转储的 `meta.json` 没有间断键，`--analyze` 无法知道当时是否有间断；这类转储不入表，要么重录要么注明“有效性未知”。
- 记录 `deliveryOffsetUs` 的 `median`（单位 µs）与样本数 `n`，并附源包大小直方图（`packets.csv` 里 source 流的 `frames` 列统计）。

### 小周期（`--small-period`）运行的前置：先查设备

先做设备查询（第 7 节）。若 `device_periods` 显示**没有任何端点的最小周期小于当前周期**（`minFrames` 不小于 `defaultFrames`，如各端点都是 480/480/480/480 帧 @ 48 kHz），则小周期运行不可能产生不同的周期：**跳过此步，在报告里记为 N/A**，并写明依据（设备表）。本测试机当前即是这种情况，所以在其上不做这一步。

只有存在更小最小周期的端点时，才可能做小周期运行，且**必须再次单独征得用户同意**，同意内容需说明：该端点（其上所有程序）的共享模式引擎周期会在这 20 s 内被临时改小，且只在有静音流的 `--small-period` 运行期间。得到同意后：

```
split_probe --pid <根PID> --seconds 20 --endpoint <该端点 id> --small-period --confirm-period-change --dump <evidence>\split-small --out <evidence>\split-small.json
```

没有 `--confirm-period-change`，探针会拒绝运行。运行后核对报告里列出的默认/基础/最小/最大周期与 `GetCurrentSharedModeEnginePeriod` 报告的生效周期；若 `smallPeriod.endedEarly` 为真或 `runValid` 为 false，作废重来。比较 `S`（不开）与 `S_small`（开）的 `deliveryOffsetUs.median`，并附两次的源包大小直方图。运行后让周期自行恢复，并用 `device_periods` 再查一次，确认已回到原值。

## 7. 设备查询（`device_periods`）

只读，不初始化任何流，不改变引擎周期：

```
device_periods --out <evidence>\devices.json
```

对每个活动渲染端点输出：`name`、`mixRate`、`channels`、`defaultFrames`、`fundamentalFrames`、`minFrames`、`maxFrames`、`minMs`、`lowLatencyCapable`（出错的端点只有 `error` 一列，其余仍列出）。把输出表原样写进报告“校准输入”的 `devices` 行。第 5 节渲染打开行的 `P_frames` 与此表的 `defaultFrames` 对照，确认应用实际用的周期。两者采样率不同，须先换算：`P_frames` 是 44.1 kHz 引擎帧，`defaultFrames` 是该端点 `mixRate` 下的帧，应有 `P_frames ≈ defaultFrames × 44100 / mixRate`（自动转换路径向上取整）。例如 480 帧 @ 48 kHz 对应 441 帧，都是 10 ms。

## 8. 收尾

1. 最后一次运行后，把应用释放（`held = false`），并用 `get_devocal_status` 确认。
2. **关闭测试应用之前**再确认一次 `held = false`；仍在接管就先释放。
3. 核对：播放器会话音量已回到 `scope.md` 记录的原值；应用数据目录里的恢复文件（`devocal-restore.json`）已不存在。任一项不符，记为异常并先恢复音量、查明原因，不要关闭了事。
4. 正常关闭测试应用（不强杀），确认进程已退出。
5. 计算全部证据文件的 SHA-256，写进 `evidence-hashes.txt`：

   ```
   Get-FileHash -Algorithm SHA256 <evidence>\* | Format-Table Hash, Path
   ```

6. 在 `timeline.md` 记每一步的时间和结果，含作废的运行及原因。

## 9. 报告里固定的“校准输入”表

任务 5 只从这张表取值。键与含义：

| 键 | 含义 |
|---|---|
| `P_frames`、`P_ms` | 渲染周期（渲染打开行），换算成毫秒 |
| `M_a`、`M_b`、`M_c` | 状态 (a)(b)(c) 各 5 次 `releaseLatencyMs` 绝对值的中位数（ms），并列出最大值 |
| `E_a`、`E_b`、`E_c` | 同一批运行中，释放前 5 s 内诊断行 `lat_ms` 的中位数（ms；含产生它的构建的系统段 `system_path_ms(P)`，见下文） |
| `S`、`S_small` | `split_probe` 的 `deliveryOffsetUs.median`（µs），分别为不开、开小周期流；附两次的源包大小直方图。无可用小周期端点时 `S_small` 记 N/A |
| `devices` | `device_periods` 输出的表 |

### `lat_ms` 的系统段与重新校准

引擎自报的 `lat_ms` = 采集包 + 环 A + 环 B + 设备缓冲 + 模型延迟 + 系统段。系统段（音频引擎把播放器的混音交给进程回环、我们的输出再混音一次）按周期估算：

```
system_path_ms(P) = SYSTEM_PATH_FIXED_US / 1000 + SYSTEM_PATH_PERIOD_TENTHS / 10 × P_ms
```

当前（自提交 3341de9 起，按 P0 测量校准）`SYSTEM_PATH_FIXED_US = 0`、`SYSTEM_PATH_PERIOD_TENTHS = 32`，即 3.2 个周期；P = 10 ms 时为 32 ms。更早的构建（含 P0 测量所用的 4115cde）固定按 3 个周期计。

重新校准时，先用**产生该 `E` 的构建**所含的系统段扣出我们自身的部分，再与实测跳变比较：

```
our = E − system_path_ms(P)          （当前构建：E − 3.2 × P_ms；4115cde：E − 3 × P_ms）
sys = M − our                        （各状态分别算，应彼此接近）
SYSTEM_PATH_PERIOD_TENTHS = round(10 × sys / P_ms)   （SYSTEM_PATH_FIXED_US 仍为 0 时）
```

`SYSTEM_PATH_FIXED_US` 只有在拆分探测证明交付偏移与周期无关（`|S − S_small| < 2000 µs`）时才改为 `S`，并相应扣减按周期的部分；`S_small` 为 N/A 时保持 0。

报告必须如实记录作废的运行、异常与未完成项，不得用单次数字、未复核的数字或 `runValid` 为 false 的运行填表。
