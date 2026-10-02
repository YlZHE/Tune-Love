# Auto-Tune 六参数直控参考原型

一个独立 Python 客户端和 Windows 进程内 C++ agent，通过 VST3 `IAudioProcessor::process` 的参数队列控制六个参数。控制链不包含宿主名称判断、MIDI loop 或需要用户加载的控制插件。

这是用于继续开发的参考实现。已实现并测试卡卡流程中已还原的接入、profile 匹配、参数缓存和 process 提交环节；**尚未完整复现卡卡的所有兼容细节**。商业 Auto-Tune 的声音、GUI、工程保存同步及其他真实宿主仍未完成本实现的验收。当前不是可直接用于直播演出的稳定产品。

## 已交付内容

| 文件 | 后续应用可复用的部分 |
|---|---|
| [client.py](client.py) | Python API、CLI、进程发现、精确 profile 选择、注入及 IPC |
| [native/agent.cpp](native/agent.cpp) | 临时插件探测、process 入口接入、真实实例识别、状态和生命周期 |
| [native/delivery.hpp](native/delivery.hpp) | 缓存、去重、VST3 队列、原指针恢复；可独立读懂和测试 |
| [native/provider.hpp](native/provider.hpp) | 临时 component/controller 初始化、连接、断开和释放 |
| [profiles/autotune-pro-38c42d0b-x64.json](profiles/autotune-pro-38c42d0b-x64.json) | 本机精确 Auto-Tune 构建的参数映射、选项及验证状态 |
| [examples/control_parameters.py](examples/control_parameters.py) | 一次显式六参数调用示例 |
| [PROTOCOL.md](PROTOCOL.md) | 可由 C#、Rust、C++ 等重新实现的 ATR2 协议 |
| [ACCEPTANCE.md](ACCEPTANCE.md) | 构建、测试和真实宿主验证边界 |

## 构建与自测

环境：Windows x64、原生 x64 Python 3.11+、Git、Visual Studio 2022 C++ Build Tools 与 Windows SDK。当前实测 Python 3.14。客户端仅用标准库。

以下命令在包含 `reference/` 的父目录运行；源码压缩包已按这个目录结构打包。

```powershell
./reference/bootstrap.ps1
./reference/build.ps1 -Target all
./reference/build.ps1 -Target fault
python -m unittest discover -s reference/tests -p test_client.py -v
python reference/tests/test_runtime.py --evidence reference/evidence/local-runtime.json
```

`bootstrap.ps1` 只下载固定 revision 的两个公开依赖；不下载商业软件。重复运行会检查依赖是否被修改。`-Target all` 编译 agent、元数据探测器、自主测试插件和测试宿主，并执行 C++ 队列测试。`-Target fault` 是故障注入测试专用构建，应用请使用 `reference_agent.dll`。runtime 测试仅启动自带的无窗口宿主；不打开现有 DAW，不加载商业插件。证据文件已存在时需另选文件名。

```powershell
./reference/build.ps1 -Target tests -Architecture x86
./reference/build.ps1 -Target agent -Architecture x86
```

x86 队列测试和编译已通过，但当前 Python loader 只支持 x64；x86 不是已交付的运行时接入能力。Linux/macOS、VST2/AU/AAX 没有实现。

## 只读查找目标和选项

```powershell
python reference/client.py scan
python reference/client.py --profile reference/profiles/autotune-pro-38c42d0b-x64.json options key
python reference/client.py --profile reference/profiles/autotune-pro-38c42d0b-x64.json options scale
```

`scan` 枚举可访问进程中已加载的 VST3 模块，不注入。读取失败会列在 `errors`，不代表该进程没有插件。桥接或沙箱宿主应选择**实际加载插件 DLL 的进程**。

`options` 只读 profile 的名称与 normalized 值，返回 `source: "profile"`。它不需要 PID，不连接宿主，也不是实时查询插件。应用可以用结果生成下拉菜单；选项标签须精确匹配。

## 在明确选择的测试工程中控制

先在干净测试宿主中加载与 profile 精确匹配的 Auto-Tune，保证音频 process 正在运行。关闭另一个助手的窗口不一定能移除其已注入 DLL；出现冲突需使用干净的测试进程。

下面交互式输入的是目标测试进程的 PID，不要输入工作工程所在进程。`attach` 会向选定进程加载自主 agent，`apply` 会改变该进程内所有匹配此 profile 的活动实例。

```powershell
$testHostPid = [int](Read-Host '已核验的测试宿主 PID')
$profileFile = 'reference/profiles/autotune-pro-38c42d0b-x64.json'
python reference/client.py --pid $testHostPid --profile $profileFile attach --wait 15
python reference/client.py --pid $testHostPid --profile $profileFile status
python reference/client.py --pid $testHostPid --profile $profileFile set retune 0.5
python reference/client.py --pid $testHostPid --profile $profileFile apply '{"retune":0.5,"flex":0.25,"vibrato":0.5,"humanize":0.25,"key":"F#","scale":"Minor"}'
python reference/client.py --pid $testHostPid --profile $profileFile status
```

`attach --wait` 只等待模块加载；成功 prepare 后还要等真实 process 首次出现，并经过约 4 秒识别门槛。`state: "matched"` 才表示实例匹配；停止处理的宿主可能一直没有实例或没有消费队列。

本 profile 的所有数字都是 **normalized 0..1**。Retune 的 `0.5` 不表示 50 ms，Natural Vibrato 的显示域也不能自行线性推断。显示单位/非线性换算尚未实现。Key/Scale 可使用标签或 profile 中精确的离散值。

| 角色名 | 控件 | 当前精确构建的 ParamID | 接受的输入 |
|---|---|---:|---|
| `retune` | Retune Speed | 4 | normalized 0..1 |
| `flex` | Flex-Tune | 90 | normalized 0..1 |
| `vibrato` | Natural Vibrato | 62 | normalized 0..1 |
| `humanize` | Humanize | 61 | normalized 0..1 |
| `key` | Key | 2 | C 到 B，含升号；本表为索引 / 11 |
| `scale` | Modern Scale | 162 | profile 中 15 项；本表为索引 / 14 |

这些 ID 与除数仅用于这一个精确 hash，不能硬套其他版本。`scale` 使用 ID 162 的 Modern Scale，不是旧接口中另一项 Scale。

## 在应用代码中复用

从父目录启动 Python 时可直接导入；将 `reference/` 与你的应用一同保留。

```python
from reference.client import Client, Profile

profile = Profile.load("reference/profiles/autotune-pro-38c42d0b-x64.json")
print(profile.options("key"))
target_pid = int(input("测试宿主 PID: "))
control = Client(target_pid, profile)
attached = control.attach()
if attached.get("pending"):
    raise RuntimeError("先在此进程加载 profile 对应插件，然后重试 attach")

ack = control.apply({
    "retune": 0.5, "flex": 0.25, "vibrato": 0.5,
    "humanize": 0.25, "key": "F#", "scale": "Minor",
})
print(ack)                  # stage=cached，只确认缓存发布
print(control.status())     # completed_revision/completion_result 核对本次调用完成
```

## 桌面应用桥接边界

`reference/app_bridge.py` 提供给 Tauri 应用的 JSON-lines 操作保持显式、无隐式副作用：

| 操作 | 作用 | 重要边界 |
|---|---|---|
| `scan` | 枚举当前用户可访问的候选进程和精确 profile | 只读，不注入 |
| `options` | 按候选 profile 读取 `key` / `scale` 的标签与 normalized 值 | 只读，不连接、不写插件参数 |
| `connect` | 连接用户明确选择的候选实例 | 不发布默认参数、不自动 clear |
| `apply` | 提交用户实际改变的连续参数 | `cached` 不是声音验收；仍需等待实例消费 |
| `clear` | 显式清空自主 agent 的参数缓存 | 不重置 DSP、GUI、宿主自动化或已投递状态 |
| `disconnect` | 结束应用侧连接状态 | 不隐式清理插件参数缓存 |

`clear` 的桥接调用只在当前连接仍然有效、且至少有一个匹配实例时发送。卡卡自动发送清理消息的触发时机尚未从本地证据中闭合，因此应用不会在连接、每次 `apply` 或切歌时自创隐式清理规则。

连续使用一个 `Client` 可保留 PID 创建时间绑定及 serial 序列。客户端检测进程身份、用户、位数、插件 SHA-256 和已加载路径。缺失的参数角色显式报错，不猜一个替代 ID。

`apply(values, serial=None)` 默认生成新 serial；显式使用与**最近一次消息**相同的 serial 会被丢弃，即使 payload 不同。它不是历史所有 serial 的永久去重集合。新的 serial 即使数值相同也会再次提交。

```powershell
python reference/client.py --pid $testHostPid --profile $profileFile clear
```

只有主动调用 `clear()` 才清助手缓存。它**不恢复 DSP、不刷新 GUI、不重置宿主自动化**，也不撤回已经投递的参数。要恢复参数必须显式提交已知基线；对于商业 Auto-Tune，六个数值回到基线是否足以恢复音频仍需验证。客户端没有偷偷在每次 apply 前 clear。

## 接入与提交的实际顺序

1. 客户端选定 PID，核对精确二进制身份并发送 `prepare_profile`。
2. agent 从已加载插件的 factory 创建临时 component，按 profile 选择 context、连接 controller，并取得 processor 的 process 槽 9 地址。
3. 安装方法入口 hook。临时 provider 断开连接、terminate、release；`free_component=false` 时额外引用有意保留到进程结束。
4. 实际宿主对象进入 process 时记录其 `this`。约 4 秒后按照 process 入口和 `valid_params` 的 ID/完整标题选择唯一 profile。
5. IPC 工作线程更新 profile 缓存。每个 processor 独立消费 revision；先记录消费，再将这一次 process 的 `inputParameterChanges` **替换**为助手队列。
6. 调用原 process，并在返回/异常展开时恢复宿主原始队列指针。原 process 返回失败时不自动重试同一个 revision。

同 ID 被新值覆盖；其他 ID 在显式 clear 前保留。单参数更新可使当前缓存中的其他参数一同重投，并不是每次都固定发送六参数。每个队列只有 offset 0 的一个点。替换发生的那个 block，宿主原队列不会同时送给插件；其他 block 原样透传。

音频回调采用固定容量，读缓存只尝试一次，不做 IPC、磁盘 I/O 或格式化日志。首次识别仍会调用插件的 QI/参数元数据接口，这部分成本取决于插件；当前不是硬实时性能保证，也没有长时间高并发压力验收。

## 多版本扩展

每个已核验商业构建应新增独立 JSON profile，保留原文件。主要字段：

| 字段 | 含义 |
|---|---|
| `schema_version` / `profile_id` | 当前 schema 为 1；自有版本标识 |
| `architecture` / `plugin_file` / `sha256` | 位数、插件 PE 绝对路径、精确 SHA-256 |
| `cid` | VST3 TUID 的 16 个原始内存字节，32 位 hex；不按 Windows UUID 重排 |
| `valid_params` | 0–16 个 `{id, title}`；ID 是 ParamID，不是枚举位置 |
| `free_component` | 是否释放临时探测额外引用 |
| `vst3_context_not_null` | 临时初始化是否传入自有 metadata host；默认 false |
| `roles` | 自主角色到该版本 ParamID 的映射，仅接受 normalized 域 |
| `roles.key.options` / `roles.scale.options` | 明确标签到 normalized 的只读映射 |
| `compatibility` / `evidence` | 验证状态与映射来源；客户端不会把这些说明当作通过证明 |

Python `select_profile(profiles, plugin_file)` 按精确 hash 和 architecture 筛选；相同 hash 的重复 profile 仍报告 ambiguity，避免把文件身份误当作版本优先级。安装路径不同可更新 `plugin_file`，不能绕过 hash 不同。Windows VST3 bundle 要指向内部实际加载的 PE 文件。

agent 对相同路径/CID 的再次 `prepare_profile` 复用已登记候选，返回原 group 与 `already_prepared: true`。合法的新规则或探测 flags 不替换原候选，不重建 Hook，也不清理参数缓存；这与“选择哪份新配置”是不同阶段。客户端收到该结果不能认为新规则已经生效。完整报文仍先校验；不同路径/CID 保持独立。具体边界见 [协议](PROTOCOL.md#prepare_profile)。

`describe.exe "插件绝对路径"` 可在独立进程枚举 factory class/CID 和可取得的元数据。商业插件可能不暴露 controller；空参数列表不表示插件没有参数，不能据此伪造映射。本机 profile 的六参数和选项来自先前同 hash 的运行记录。

匹配使用一次获取的 ParamID→ParameterInfo 快照，完整标题不做 trim、模糊名称或别名，仅折叠 ASCII 大小写；其他语言标题的等价规则未核验。空 `valid_params` 是允许的。目前首次识别遇到多个可匹配候选仍返回 `ambiguous`，这与卡卡取首个匹配者不同，尚未修正。卡卡样本使用哈希容器的链表遍历，不能擅自用注册顺序作为选择优先级。新 profile 不会重新判定已经匹配或已经拒绝的 processor；新实例才会走当前候选集合。

已实现多 profile 基础设施及自主双模块测试。商业配置的版本阈值、完整选择优先级、Key+Scale 组合映射、清理发送时机、GUI/工程同步还没有全部还原，不能称为已完整支持卡卡的全部版本。

## 状态与失败处理

| 状态/字段 | 可得出的结论 |
|---|---|
| `pending: true` | 所选进程尚未加载目标模块；可等待后再 attach |
| `awaiting_process_identification` / `discovering` | 已准备入口，真实 processor 尚未识别完成 |
| `unmatched` / `ambiguous` | 无匹配或首次识别出现多候选；该实例不提交参数 |
| `stage: cached`、`cache_revision` | 新消息已发布或重复消息已确认；不等于已产生声音 |
| `consumed_revision`、`submitted` | 原 process 调用前记录的消费版本/尝试次数，不能独自证明完成 |
| `last_process_result` | **最近一次助手队列提交**的原 process 返回值，不是普通 block 的最后返回值 |
| `completed_revision`、`completion_result` | 同一次正常返回的版本与返回值；单个原子快照，旧版 agent 缺失时不确认本次完成 |
| `snapshot_available: false` | 本次状态未取到一致缓存快照；不要把返回的 0 当成已清空 |
| `another parameter hook is loaded` | 检出商业/旧助手冲突，控制请求拒绝；使用新干净测试进程 |
| `partial hook setup` | 部分安装失败后 sticky failure，后续控制拒绝；正常关闭此测试宿主 |

容量：16 个 profile、128 个活动 processor、64 个方法 hook、每个缓存 256 个 ParamID。检查 status 中的 overflow/reentries；这些不是自动扩容能力。

应用桥接的 `stage: submitted` 要求所有当前匹配实例完成当前 revision 且返回成功。缓存暂忙保持连接；旧窗口的 clear/apply 不影响新连接，编辑→清理→编辑不会被跨清理合并。这些是自主应用的状态与请求顺序保护，未增加卡卡尚未核对的自动清理规则。详见 [桥接可靠性修复](../docs/plans/2026-10-01-app-control-reliability-report.md)。

管道只接受本机当前用户，客户端验证服务端 PID/创建时间。agent 和方法 trampoline 所在模块会保留到宿主进程退出；当前不提供热 detach，也不应强制 FreeLibrary。真实宿主工程的保存提示及正常退出由宿主负责。

参考客户端还保留本项目两条历史保护 PID（12584、23532）。这是当前工作区的保护约束，不是某种宿主适配；产品化时应设计明确的进程选择/身份绑定策略。

## 验证范围与发布

2026-10-01 当前代码：x64/x86 C++ 各 31 项通过，两套 agent 编译通过；Python client、桥接及源码包 smoke 共 57 项通过。新增完成诊断的 DLL 尚未重跑跨进程 fixture 或商业音频验收。

2026-09-28 历史构建：x64 C++ 24 项、Python 32 项、真实本地 IPC/自有 VST3 fixture 8 组通过；x86 C++ 24 项及 agent 编译通过。该轮 fixture 全部正常退出，无残留进程。多个实例主要由同一个测试音频线程串行处理，不是高并发压力证明；旧 runtime 证据不能套用于本轮新 DLL。

本轮真实 FL 已完成 attach/prepare，但后续发现商业助手 DLL 也进入同一进程，状态查询明确拒绝。没有完成本实现六参数实际投递或音频验收。profile 保持 `identified_not_runtime_validated`。详细矩阵见 [ACCEPTANCE.md](ACCEPTANCE.md)。

```powershell
python reference/package.py --output reference/dist
```

输出源码包、x64 二进制参考包和哈希清单。打包脚本只采用固定文件白名单，排除整个 `work/`、历史 `prototype/`、本地证据、商业软件与账号数据。源码包通过 bootstrap 获得依赖，不包含依赖 Git 仓库。许可证见 [LICENSE](LICENSE) 和 [第三方声明](THIRD_PARTY_NOTICES.md)。
