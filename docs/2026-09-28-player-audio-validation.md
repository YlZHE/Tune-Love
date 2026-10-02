# 当前播放器音频接入验收

2026-09-28 已接续原会话完成 Now Playing 的真实 PCM → Strands 接入，正式版已构建并启动。

## 当前行为

根据当前媒体来源确认播放器进程树，仅捕获其音频。来源不明或有歧义时关闭采集；不回退系统混音。后台保留最多 8 秒的 48 kHz 双声道浮点 PCM，为未来调式分析提供入口；不保存录音或上传数据。官方 Strands 接收真实电平，静音收拢、暂停停止、旧来源或过期帧归零。

## 已执行验证

- 前端单元测试 41/41、浏览器界面测试 28/28、Rust 单元测试 25/25、wasapi 静音/null 缓冲保护测试 2/2，通过。
- Folia 真实音频探针取得 239,040 帧，确认实际 PCM 非零。
- 两个独立测试进程：目标有声时 RMS 0.0069687331；仅非目标发声时目标 RMS/peak 均为 0，非目标自身 RMS 0.0068956137。两个测试进程正常退出。
- 原生 WebView2 验收：24 个样本全部新鲜且与来源/歌曲绑定，24 个不同电平，RMS 0.2302–0.6993；五次 Strands 像素哈希全部不同，真实画面在变化。
- 原生页面无错误、无溢出；真实来源图标、播放进度、Tooltip、置顶、设置、配色同步和退出验证通过，用户原配色已恢复。
- 发布版正常启动并响应，核验时仅有一个实例，19224 调试端口未开启。

本次修复同时解决 IPC 电平超范围导致前端拒绝音频、并发刷新旧来源覆盖新来源，以及取消测试调度窗口过小的问题。原始分析 PCM 未被钳位；只有界面指标限制在 0..1。

## 交付位置与证据

- 程序：`src-tauri/target/release/autotune-helper-now-playing.exe`
- 实际隔离：`artifacts/audio-isolation-verification.json`
- 原生数据及画面：`artifacts/native-verification.json`、`artifacts/native-audio-*.png`
- 发布版身份及 SHA-256：`artifacts/release-verification.json`
- 完整记录：`.superpowers/sdd/2026-09-28-player-audio-strands/task-3-report.md`

本轮实测为 Windows 11 build 26200、Folia 和受控测试进程。隔离单位是应用进程树，不是单首歌曲或浏览器标签页；调式识别算法尚未实现。Vite 有 580.37 kB 入口文件体积告警，构建通过但仍有优化空间。此验收不代表 Auto-Tune 参数控制已经完成或已完整复现卡卡流程。
