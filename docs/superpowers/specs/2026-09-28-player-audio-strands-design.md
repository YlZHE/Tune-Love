# 当前播放器真实音频与 Strands

用户于 2026-09-28 确认本设计并授权开始实现。范围只限 now-playing 应用，不操作 Auto-Tune、宿主或播放器控制。

## 目标

用 React Bits 官方 Strands 替换“正在播放”左侧图标，响应当前显示播放器的真实 PCM 音频，并保留后续调式分析入口。本阶段不实现 Key/Scale 算法。

## 不变约束

- 只捕获已确认的当前播放器进程树；不捕获系统混音，不打开麦克风，不使用虚拟声卡，不注入播放器。
- 不保存或上传音频，PCM 仅留在有上限的内存环形缓冲中。
- 不按显示名称模糊匹配。源身份不明确、多个无关联候选、权限失败或不支持时关闭采集，安静回退。
- 进程隔离不等于歌曲隔离：同一应用进程树的提示音和浏览器其他标签页也可能包含。
- 保留现有深色外观、窗口尺寸、歌曲切换动画、配色过渡、来源图标和 Warm Tooltip。不新增设置项。
- 官方 Strands 源码和许可必须保留出处；适配生命周期，不自写近似效果替代官方组件。

## 音频通道

使用 wasapi 的 `AudioClient::new_application_loopback_client(pid, true)`。仅支持 Windows build 20348 及以上的进程回环；运行时失败不可改用 endpoint loopback。

进程定位独立于图标解析：优先精确 AUMID，或 AppsFolder 对应的本地可执行文件路径；裸 exe 名只在全部匹配解析为同一路径且同一进程树时接受。按进程快照确定根节点，记录创建时间，保持句柄验证存活，避免 PID 重用。无唯一结果时不猜。

独立后台线程拥有 COM 和捕获资源，PCM 固定为 48 kHz / 双声道 / f32。每个包验证长度、有限值及捕获标志；断流、来源变化、歌曲变化、暂停、退出时清空分析历史。缓冲最多保留最近 8 秒，独立于前端。提供 Rust 侧 `analysis_window` 读取接口，不暴露原始 PCM 给 WebView。

前端通过独立轻量命令 `get_audio_level` 最多约 30 Hz 读取：

```ts
type AudioLevel = {
  sourceId: string | null;
  trackKey: string | null;
  status: "idle" | "resolving" | "capturing" | "unavailable";
  rms: number;
  peak: number;
  level: number; // finite, 0..1, derived solely from captured audio
  updatedAtMs: number;
};
```

`trackKey` 与前端 songKey 一致，为 JSON `[sourceId, title, artist, album]`。状态不是 capturing、来源/歌曲不匹配、帧超过 400 ms 或页面暂停时，动画输入为零。媒体快照本身过期时后台也停止捕获，不能无限延用旧播放器。

## 视觉

Strands 为紧凑透明图形，约 30 × 18 px，放在播放状态文字左侧。固定速度下用真实音频调制 amplitude/intensity，避免不断修改 speed 导致相位跳跃。快速响应上升、平滑回落；静音时收拢，暂停和无信号时最终停止 RAF。

读取计算后的当前强调色，保留 CSS 的颜色中间帧。隐藏页面暂停绘制，恢复后不能使用陈旧音频；reduced-motion 下为静态简化状态，WebGL2 不可用或丢失时安全显示静态回退。清理 RAF、观察器和 GPU 资源。

## 验收

1. Rust 单元：唯一匹配、歧义拒绝、PID/创建时间身份；PCM 静音/幅度/非有限值；有界缓冲；切换清理；旧结果不能套到新来源。
2. 前端单元：音频帧有效性和衰减；界面：真实 Strands 输出随输入变化、过期/暂停归零、颜色过渡、reduced-motion、WebGL 回退、无布局溢出。
3. 原生捕获：先只读 Folia，证明实际 PCM 非零并报告统计（不保存音频）；再用受控独立播放进程验证目标有声、非目标有声不混入。测试音保持较低幅度。
4. 原生 WebView2 CDP：验证真实状态与动画、配色/来源/设置回归，恢复用户配色，正常退出测试实例。构建 release 并正常启动。

## 参考

- https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/
- https://docs.rs/wasapi/0.24.0/wasapi/struct.AudioClient.html
- https://reactbits.dev/r/Strands-TS-CSS.json

本目录不是 Git 仓库；不初始化 Git，不创建 worktree，不修改父目录的原型或取证材料。
