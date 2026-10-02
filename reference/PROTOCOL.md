# ATR2 本地控制协议

这是自主定义的客户端/agent 协议，不是 Auto-Key、Auto-Tune 或卡卡 wire 协议。任何能访问 Windows named pipe 的应用都可实现。商业插件仅收到标准 VST3 `IParameterChanges`。

## 连接

管道名：`\\.\pipe\autotune-reference-{pid}`。本地、双向、message mode、单实例；DACL 只授予目标进程当前用户。客户端必须核对目标创建时间、用户身份和 `GetNamedPipeServerProcessId`。

一次连接发送一个完整请求 message，接收一个 UTF-8 JSON message，然后服务端断开连接。不附换行，不使用 TCP。最大请求 8192 字节，客户端最大响应 65536 字节；服务端状态大于 65000 字节时返回明确错误。

## 公共头

Little-endian、无 padding，Python 格式 `<IHHIIQ`，总长 24 字节。

| offset | 长度 | 字段 | 值 |
|---:|---:|---|---|
| 0 | 4 | magic | `0x32525441`，字节 `41 54 52 32` |
| 4 | 2 | version | 1 |
| 6 | 2 | op | 见下表 |
| 8 | 4 | group | prepare 返回的 profile group；不是 processor 地址 |
| 12 | 4 | payload_size | 头之后的准确字节数 |
| 16 | 8 | serial | apply 的非零去重 token；其他 op 为 0 |

| op | 名称 | 当前 agent 行为 |
|---:|---|---|
| 1 | status | 无 payload、serial=0；读取全部 group/instance |
| 2 | legacy prepare | 客户端保留旧编码函数；当前 agent 不支持，不要发送 |
| 3 | apply | 有效 group、非零 serial；提交 1–256 个值 |
| 4 | clear | 有效 group、无 payload、serial=0；显式清缓存 |
| 5 | prepare_profile | group=0、serial=0；准备一个精确 profile |

## prepare_profile

payload 顺序：

| 长度 | 数据 |
|---:|---|
| 16 | CID 原始 TUID bytes |
| 4 | uint32 flags：bit0=free_component，bit1=vst3_context_not_null，其余位为 0 |
| 4 | uint32 path_bytes：UTF-16LE 字节长度，非零、偶数、最多 2048 |
| 4 | uint32 valid_count：0–16 |
| path_bytes | 绝对盘符路径 UTF-16LE，不带 NUL |
| valid_count × 260 | 每条 uint32 ParamID + 256 字节 UTF-16LE title |

title 最多 127 个 UTF-16 code units，余下补零，最后 code unit 必须为 NUL。路径必须指向本进程已加载的 PE 文件；prepare 不会从路径强行加载一个未加载的商业插件。

返回示例：

```json
{"ok":true,"group":1,"state":"awaiting_process_identification","free_component":true}
```

相同 path/CID 重复 prepare 返回同一 group，`already_prepared: true`。已登记候选保留首次的 `valid_params`、探测 flags、队列与实例识别结果；后来请求中合法但不同的规则/flags 不替换它，也不会重新探测插件。此 ACK 表示复用既有候选，不表示后来配置已生效。请求边界与标题终止符仍先校验；非法报文不因身份已存在而放行。不同 path 或 CID 不属于重复登记。此行为对应已核对的候选保留步骤，尚不代表上游全套配置索引覆盖、模块 Hook 分配或候选遍历次序已完全复刻。

未加载时返回 `pending: true, reason: "plugin_not_loaded"`。SHA-256 和 architecture 校验由客户端在 prepare 前完成，wire 中不包含 hash。agent 不是给不受信任客户端提供的任意写入服务。

## apply

payload 为连续的 8 字节记录，格式 `<If`：uint32 ParamID + float32 normalized。所有数值必须有限且在 `[0,1]`。同一批重复 ID 以最后一个值为准。

```python
from reference.client import encode_request

packet = encode_request("apply", group=1, serial=1001, values=[
    (4, 0.5), (90, 0.25), (62, 0.5), (61, 0.25),
    (2, 6 / 11), (162, 2 / 14),
])
assert len(packet) == 24 + 6 * 8
```

此示例只对应已识别的本机 Auto-Tune profile。应用应调用 `Profile.map_values()` 获取 ID 和值，避免跨版本硬编码。

返回示例：

```json
{"ok":true,"stage":"cached","duplicate":false,"cache_revision":1}
```

serial 只与此 group 最近接受的 serial 比较；不是“必须递增”的协议限制。相同 serial 忽略，新的 serial 接受并推进内部 revision。缓存按 ID 增量覆盖；没有隐式 clear。内部 revision 和外部 serial 是两个不同字段。

所有匹配此 group 的实例独立消费当前 revision。这是一份当前状态缓存，不是保证每条历史消息都逐一投递的 FIFO；在两个音频 block 之间发布多次更新时，实例可直接消费最新快照。当前没有单个 instance 定向写入命令。

process 前消费 revision；非空新快照构造 offset 0 的 VST3 参数队列并替换输入指针，调用原方法后恢复指针。原方法失败也不自动重投。需要重投时由应用显式发新 serial。

## clear

```json
{"ok":true,"cleared":true,"dsp_reset":false}
```

clear 将活动缓存参数数清零，保留 serial/revision 语义。已提交的 DSP 状态仍存在；同一个 serial 在 clear 后仍不会重新发布。后续新 serial 会从空活动缓存添加参数。

## status 和错误

`route` 为 `direct_process_queue`。`groups` 提供 `received`、`duplicates`、`message_serial`、`cache_revision`、`cached_parameters`、`snapshot_available`。`instances` 提供 `state`、`group`、`blocks`、`consumed_revision`、`submitted`、`last_process_result`、`completed_revision`、`completion_result`。`state` 比数字 group 更可靠：未匹配实例的 group 不是有效的 prepare group。

`consumed_revision` 和 `submitted` 在原 process 调用前更新，后者是尝试计数，不是完成计数。`last_process_result` 是最近一次正常返回的助手提交的 process 返回值，可能仍属于上一笔请求，不可将它与最新 `consumed_revision` 拼接成完成凭据。

2026-10-01 新增的 `completed_revision` / `completion_result` 来自单个原子快照，只在带助手队列的原 process 正常返回后一起发布；结果可以是成功或失败。新实例/匹配 group 改变时清零 revision 并标记 `kNotImplemented`；异常展开不发布完成，原队列指针仍恢复，同一 revision 仍不自动重试。新增字段是自主诊断，不是卡卡 wire 字段，不改变参数提交顺序。

应用只在一致 group 快照可用、且所有当前匹配实例的消费 revision 和完成 revision 都等于当前 apply ACK 的 revision、完成结果均为 0 时确认 `stage: submitted`。缺失新增字段的旧 agent 不获得完成确认；需要在干净测试宿主下一次启动时使用新 DLL，不提供热卸载/替换已加载代码。确认也只代表队列调用完成，不能代替音频/GUI/工程验收。

`snapshot_available: false` 表示忙碌或竞争导致本次没读到缓存。附带的 0 revision/count 不代表实际清空。应用保留连接并等待新的状态采样；此时不发送 apply/clear、不自动重试写入。

失败统一为 `{"ok":false,"error":"原因"}`。冲突检查在执行命令之前，因此包括 status 在内都会在已加载其他助手时返回错误。不要绕过它重发 apply。部分 hook 失败进入终止控制状态，但保留 trampoline 供原宿主继续处理；正常退出测试宿主后重新开始。

识别或缓存忙时，普通 process 透传。识别会在首次 processor 经过等待门槛后读取一次参数元数据快照；结果随后按 processor 身份缓存。候选注册不会使既有匹配或拒绝结果失效；新 processor 使用当时的候选集合。首次识别多个候选同时通过时，原型仍报告 ambiguous；这是尚未修正的卡卡流程差异，不能当作最终兼容规则。IComponent 尚不可取得时仍处于 discovering，后续 process 可继续尝试。当前不提供 detach、进程退出、修改工程、音频捕获或 GUI 同步命令。
