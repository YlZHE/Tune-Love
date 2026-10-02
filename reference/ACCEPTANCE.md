# 参考原型验收记录

## 2026-10-01 补充：当前构建

x64/x86 delivery 各 31 项检查、Python client/应用桥接/源码包 smoke 共 57 项通过；两套 agent 可编译。新 agent 添加版本关联的完成诊断；旧 `submitted` 计数仍发生在 process 调用前，应用不再将它和旧返回值拼作成功。缓存忙和旧连接请求不会误清新连接。新增字段与当前构建说明见 PROTOCOL.md。

本轮继续补充了识别回归：新增的 `identification_tests.cpp` 直接执行生产识别函数，覆盖“一次 metadata 快照、ParamID+标题匹配、空签名、拒绝结果缓存、候选新增不重判既有 processor”20 项检查，x64、x86 均 20/20；同轮 delivery 各 31 项、Python 57 项通过，两个 agent 再次构建成功。该 fixture 不启动宿主，不加载商业插件，也不证明真实音频。当前 x64 DLL SHA-256 为 `561674bba219d16913b10bce9f104535af48b61b2b7a214a84c8b3413ea2775c`。

与卡卡静态证据对照后，原型已移除“注册新 profile 触发全量重识别”的工程规则。首次识别的多候选 ambiguous 分支仍在；卡卡使用哈希容器取首个通过者，完整注册和重哈希历史尚未闭环，所以本轮没有用任意顺序替换这条待修正分支，不能报告为多版本优先级已完全复现。

本轮没有重跑 DLL 跨进程 fixture、商业宿主或人声效果验收。下方 2026-09-28 记录完整保留，是旧构建结果；不能作为 2026-10-01 新 DLL 的运行通过证明。应用调试版已重新构建，仍依赖本工作区 Python/reference 文件，不是独立安装包。

## 2026-09-28 历史记录

日期：2026-09-28。此文件描述当前 `reference/`，不继承旧 `prototype/` 的真实宿主或音频通过结论。

## 已通过

| 层级 | 最新结果 | 验证了什么 |
|---|---|---|
| x64 native 构建 | agent、probe、fixture、故障测试构建成功 | 自有 C++ 与两个固定公开依赖可编译 |
| x64 delivery | 24 项通过 | serial 去重、同 ID 覆盖、缓存保留/清理、独立消费、队列替换/恢复、失败不重投 |
| Python client | 32 项通过 | 编码、profile、离散选项、身份和冲突保护、真实本地 named pipe、CLI 及错误 |
| x64 runtime | 8 组通过；0 failures，0 errors；约 41.2 秒 | 实际跨进程注入、自主 VST3 fixture、原入口 hook 和参数消费 |
| fixture 生命周期 | 全部自然退出，exit code 0，无残留 | 在自有 host/fixture 的既定用例中生命周期正常 |
| x86 delivery / agent | 24 项通过、agent 编译成功 | 源码和队列逻辑可构建；没有 x86 loader/商业运行时验收 |

runtime 用例具体覆盖：

1. 目标模块加载前 pending，加载后成功 prepare。
2. 已有实例晚接入，同 serial 去重、新 serial 重投、显式 clear、新实例消费当前缓存、process 失败后不自动重试。
3. 4 秒前销毁实例、后补 profile 重新识别此前 unmatched 实例。
4. 新增空签名候选后，旧匹配变为 ambiguous 并拒绝错误提交。
5. 无 controller + 空 valid_params 可以提交。
6. 无 controller + 非空 valid_params 保持 unmatched。
7. 部分 hook 安装失败后，status/prepare/apply 持续拒绝；宿主继续处理并自然退出。
8. 共享 process 入口的多个 class、两个插件模块、多个 profile 保持独立缓存与识别。

测试中的多个实例主要由一个测试音频线程串行处理。未执行长期、高并发、多核实时负载压力验收。

## 真实 Auto-Tune 的当前状态

插件 SHA-256：`38c42d0b4b260fa72e7ed4af58cb9e271cc48c9fc72372463254f040bbbf76f3`。原始 CID：`415453563854413175746f2d54756e65`，x64。

六参数 ID 和 Key/Modern Scale 选项已按同 hash 的历史运行材料建成 profile。本轮独立 describe 能枚举 factory class/CID，未取得 controller 参数列表；这不等于插件没有参数。

本轮隔离 FL 已成功 attach/prepare，返回 group 1 和 `awaiting_process_identification`。随后只读核对发现同进程加载了商业助手 `em64.dll`，agent 返回 `another parameter hook is loaded; use an isolated clean process`。写入被阻止，没有完成本实现的六参数消费/提交与声音验收。此测试记为 **环境冲突中止**。

测试窗口已收到正常关闭请求，15 秒同句柄等待超时；后续检查进程已不再存在。没有强杀，但没有取得最终退出码，因此不将其记为正常退出验收通过。

profile 保持：

```json
{
  "status": "identified_not_runtime_validated",
  "mapping_verified": true,
  "reference_agent_in_real_host_verified": false,
  "reference_audio_output_verified": false,
  "gui_project_sync_verified": false
}
```

## 尚未通过的商业兼容矩阵

| 项目 | 状态 |
|---|---|
| 当前 Auto-Tune + FL 的本实现实际音频 | 未完成；冲突环境不能据此评价 |
| 第二真实宿主和多数宿主 | 未验收；控制架构不含宿主适配代码 |
| 其他商业 Auto-Tune 版本/产品 | 尚无本实现的运行验收；多 profile 机制已在自主插件验证 |
| Retune GUI 毫秒及其余显示单位换算 | 未实现；API 仅 normalized |
| Key+Scale 完整商业组合/联动 | 未完整还原；当前按 profile 独立映射并同队列投递 |
| 卡卡自动 clear 的所有触发时机 | 仍属研究缺口；没有自创隐式 clear |
| GUI、宿主自动化与工程保存同步 | 未实现完整同步链，未验收 |
| 音频恢复到基线 | 未验收；六参数相同不构成音频相同证明 |
| 插件热卸载、agent 热 detach | 不提供；模块租约保留至进程退出 |
| x86 客户端、非 Windows 或非 VST3 | 不支持 |

## 复现

2026-10-01 后续修正：相同 path/CID 的重复候选登记现在保留原规则、探测 flags、缓存及 group，不再因合法的新规则不同而报错。新增直接调用 production `prepare` 的测试，不执行 DLL 入口、安装 Hook 或启动宿主。行为 RED 为 35 项中 4 项失败；修正并加强“没有重投/新实例仍用旧规则”断言后，x64/x86 各 37 项识别检查通过，另各 31 项队列检查通过。Python client/bridge/package 57 项通过，两架构 agent 编译通过。跨进程 runtime 断言已更新，仅语法检查，没有把历史 runtime 通过套给新 DLL。

本次属于候选重复登记步骤的修正，不代表完整模块分配、首匹配顺序、商业多版本或实际音频验收完成。商业兼容矩阵中的未验证项继续保留。

见 [README 的构建与自测](README.md#构建与自测)。公开包不含本地商业取证材料。
工作区保存了更完整的交付报告、进程身份记录、只读模块快照和最终 `runtime-07.json`；发布包仅携带这份能力/限制说明。源码包解压后独立执行的 32 项客户端测试也通过。更新 native 代码后应重新构建并重跑 runtime，不应将旧证据套用于新二进制。
