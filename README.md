# Tune Love

第一阶段：独立的 Windows 音乐信息悬浮窗。Tauri 2 + React + TypeScript + Rust。

2026-10-01 控制层续开发：已加入独立的 `autoTune.setRetuneSpeed(value)`，接受卡卡已核对页面的整数速度输入 0～400，按精确 profile 转换后进入现有四参数控制通道；401 项表值与生产函数一致。主窗仍调用原来的归一化百分比入口，尚未切换到新速度输入，不把新入口的数值称作毫秒。最新实现和边界见 [Retune 转换报告](../docs/2026-10-01_reverse-kaka-retune-table-report.md)；以下早期界面描述中的“全部前端预览”不代表当前已显式连接后的四参数调试能力，连接功能见 [应用桥接方案](../docs/plans/2026-10-01-app-control-bridge.md)。

当前显示正在播放媒体的歌曲名、歌手/作者、封面、已播放时间和总时长，并可控制当前播放器的上一首、下一首、播放和暂停。采用约 448 × 222 的横向布局，支持拖动、置顶，固定深色外观。后台会从当前已确认播放器进程树的真实 PCM 识别稳定的 12 个主音 × 大调/小调，并在左上角以 `A 小调` 等标签替换应用标题；未识别或归属不匹配时仍显示 `Tune Love`。Auto-Tune 参数写入需在设置页显式连接后才可用（四个连续参数为调试入口；Key/Scale 的自动写入见下文「自动写入 Key/Scale」，默认关闭）。没有歌词、已接入的去人声处理或音频指纹识曲。

### 悬停控制区（真实播放控制 + 电音前端预览）

鼠标移入音乐卡片，底部来源和“正在播放”区域原位切换为控制按钮：左侧为闪电、电音详细参数、人物加声波图标的去人声开关；右侧为上一首、播放/暂停、下一首。移出后恢复真实来源和播放状态，原有音频画布保持挂载，不重建采集。键盘聚焦同样显示控制区，无悬停能力的触摸设备常驻显示按钮；面板打开时移出鼠标不会收起控制区。

- 闪电弹出约 230 × 50 的单行浮层，布局为“自然 — React Bits Elastic Slider — 明显”，文字位于滑条左右且垂直居中；不显示重复标题、数字输入、预览说明或关闭按钮。点击外部、再次点击闪电或按 Esc 收起。范围 0–100%，默认 50%；这是独立 UI 值，尚未定义或调用 Retune Speed 的实际数值转换。
- Elastic Slider 保留官方 sigmoid 越界衰减、50 px 弹性越界及松手回弹，悬停尺度为 1.12。背景与滑条共用弹性状态，同向拉伸并轻微压缩，松手一起回弹；背景是独立视觉层，文字仅随对应端点让位，不被横向拉扁。弹层在原生窗口两侧预留 80 px 的视觉空间，无需放大窗口，也不能绘制到原生窗口之外；实际数值仍限制在 0–100。动画沿用现有 Motion，滑条捕获/键盘/焦点/ARIA 沿用 Radix；不新增依赖或音量图标。系统减少动态效果时仍可调值，但禁用缩放和弹性。颜色跟随现有重点色过渡。独立设置窗口不变。
- 拖动时复用 Warm Tooltip，在可见调节点上方立即显示精确整数百分比，启用 Lean、黑色背景且无箭头。受控显示和实时内容更新不重播入场动画；只在提示可见时跟踪动画锚点。拖动到范围外仍显示 0% 或 100%，松手后约 530 ms 开始淡出（450 ms 数值停留，加上组件原有的 80 ms 离开缓冲）；键盘调节也显示数值。失去指针捕获、关闭面板或卸载时清理提示和计时器。该数值仍是前端强度预览，不是 Retune Speed 的毫秒值。
- 详细参数改为约 340 × 144 的非模态小浮层，横排三个 86 px 的 React Bits Comet Dial：Flex-Tune（0–100）、Natural Vibrato（−12–12，步长 0.1）、Humanize（0–100）。名称在旋钮下方，精确数值位于中央并可直接点击输入；不显示大标题、关闭按钮或遮罩。点击外部、再次点击详细参数按钮或按 Esc 关闭。此浮层没有手动 Key / Scale；自动识别结果只在用户打开「自动写入 Key/Scale」开关并已连接插件时写入，见下文。
- Comet Dial 直接复用官方 TS/CSS 源码，保留圆弧输入、速度驱动的分段彗尾和弹簧回弹。采用 sweep=260、speed=59、tapBounce=0.26、flickBounce=0.12；产品将 momentum 设为 0，松手后不惯性修改参数值，动画回弹与真实值分离。支持方向键、Home/End、PageUp/PageDown 和触摸，系统减少动态效果时停用彗尾/回弹但仍能调值；弧线、彗尾和数字跟随重点色平滑过渡。
- 旋钮中央数字清除 Radix 输入框默认文字缩进，包含负号与小数在内整体居中。Natural Vibrato 的亮弧以顶部 0 为起点：负值向左、正值向右，归零后不显示亮弧；另外两个旋钮仍从最小值开始显示进度。
- 悬停旋钮或名称、键盘聚焦时，由独立 WarmTooltipGroup 显示参数作用说明，保持黑色、无箭头与 Lean 动画，连续切换名称保留原组件的平滑滑动。提示选择上下空间较充足的一侧，不重复显示数值，拖动时隐藏。中央数字提交时限制到预览范围，清空后离开输入框恢复原值；从超范围草稿直接操作旋钮时，以后发生的旋钮选择为准，输入、旋钮、ARIA 保持同步。重开保留当前会话数值，重启不保存这些预览值。
- 去人声开关使用 Phosphor 的 UserSound（人物加声波），表示去除歌曲原唱、保留伴奏的独立开关，不是电音启用或麦克风静音。默认关闭，关闭为灰色线框、提示“开启去人声”；开启为重点色实心、提示“关闭去人声，恢复原唱”。保持原按钮尺寸和 Warm Tooltip 动画。目前仅预览开关状态，点击明确提示实际去人声处理尚未接入，实际音频不变；不改变电音强度或 Auto-Tune 参数。
- 上一首、下一首、播放／暂停复用已安装的 `nowplaying 0.1.1` 控制接口，经独立 `control_media` IPC 调用 Windows 媒体会话，不发送全局媒体按键、不注入播放器。只有主窗口可请求控制。点击携带所显示的来源、会话 ID、歌曲标识与目标代次，后端检查快照新鲜度、目标代次、来源／歌曲一致性、当前原生选择与会话唯一性，再提交一次；不排队、不自动重试。已观测到的目标切换／失效会递增代次，切回同一歌曲也拒绝旧请求；进度、封面和普通播放状态变化不递增。重复请求会被拒绝，不支持的按钮按真实能力禁用。
- 播放按钮只跟随真实媒体状态，不乐观伪造暂停／播放；封面、歌曲、进度和音频动画仍由原媒体链路更新。请求过程中禁用播放区按钮；失败、目标变更、超时显示简短提示。IPC 等待 5 秒超时并不取消已经提交的原生操作；后台任务继续持有提交锁到实际结束，后续请求返回忙而非并行发送。原生操作如果永不返回，将保持拒绝新提交，需重启应用恢复。只读发现／校验阶段各有限时，不会无限占用锁。
- 检查时发现同 AUMID 的多个会话会禁用／拒绝控制。上游提交接口仍按 AUMID 重新寻找会话，校验与提交不是原子操作；未被轮询观察到的瞬间变化、校验后替换或新增同 AUMID 会话、相同元数据的实例替换，不能承诺完全消除竞态。播放器必须暴露系统媒体会话及相应操作，不能据此宣称支持全部软件。
- 电音强度、详细参数、去人声仍仅前端预览，不发送插件控制或执行音频处理。播放控制不修改这些本地预览值。
- 复用已安装的 Radix Themes Popover、TextField、IconButton 与 Radix Slider primitives；Comet Dial 的旋转输入与彗尾来自官方组件。弹层仍由 Radix 管理焦点和关闭，参数面板适配原来的 468 × 242 原生窗口，无需放大窗口或滚动，不新增 npm 依赖。

播放来源显示为「真实应用图标 + 应用／进程名称」，例如 `[Folia 图标] Folia`，不显示 `.exe`、程序路径或内部 AUMID。优先通过 Windows Shell 的 AppsFolder 注册信息读取名称与图标；只提供可执行文件名的来源会只读匹配本地运行程序路径，多个不同路径同名时不猜测归属。未注册／无法解析的来源保留可读名称或“媒体播放器”，图标缺失或损坏时使用通用图标。长名称截断，Warm Tooltip 显示完整名称。图标只在本机读取并转换成 PNG，不联网下载，不修改或注入播放器。

来源解析在后台执行，缓存当前来源；已有图标每 5 分钟刷新，缺失图标每 30 秒重试。最多一个解析任务，旧来源的迟到结果不会应用到新来源，也不会阻塞音乐进度。歌曲动画使用稳定的 `sourceId`，名称／图标异步补齐时不会重播切歌动画。

标题栏中间为设置按钮，替代原来的明暗主题切换。点击打开独立的约 760 × 540 设置窗口，可以调整大小，不占用音乐悬浮窗的内容区域。窗口按需创建，重复点击会恢复并聚焦同一个窗口；关闭设置或按 Esc 仅隐藏设置窗口，悬浮窗继续工作，关闭主应用则一并退出。旧的明暗主题偏好不再读取。

设置页保留「从封面提取主色」开关，自动模式不再提供选色按钮。extract-colors 在本地提取封面色板：先排除接近黑、白、灰的候选，再按占比自动选取彩色重点色，并向特效提供三色。彩色候选的 HSL 饱和度至少为 25%、亮度在 18–85%；不足三种时，用重点色的协调色阶补齐。黑白封面优先使用最亮的白色；没有白色像素的纯黑/灰封面采用柔和白色，特效使用灰白色阶。只有无封面、全透明或读取失败才回退手动配色。切歌提取期间保留已提交的色板，防止闪回手动色；旧封面的迟到结果不能覆盖新封面。

关闭自动取色后，仍通过 react-colorful 调色盘、8 个预设或 HEX 选一个主色，音频特效自动生成三个同色系色阶，切歌不改变配色。重点色用于底色、进度条、品牌图标及激活状态；过暗的颜色经 colord 调亮后用于小字和图标，原始选色保留。配置保存在 `helper-colors-v1`，向后兼容旧数据；同源 storage 事件同步主窗口与设置窗口。背景、重点色与三条线的颜色都平滑过渡，不上传图片或颜色数据。

特效渲染对接近黑色的颜色设置最低可见亮度，避免有音频响应却看不到线条；静态降级图形也会轻度提亮。旧版 `coverIndex` 字段不再参与选色，升级保留原有自动/手动模式和手动色。

标题栏按钮、歌名、歌手／作者及播放来源统一使用 React Bits 官方 Warm Tooltip，并共享一个 WarmTooltipGroup：首次悬停等待 400 ms 后缩放、淡入并从模糊变清晰；连续移到相邻目标时，共用提示框以约 320 ms 的弹簧过渡移动、改变宽度，文字按移动方向切换。启用官方演示的 Lean（10° 最大倾斜，随移动速度变化），关闭箭头。保留官方 300 ms 的 warmWindow、键盘与减少动态效果支持。应用使用黑色背景、浅色文字、小字号；长文本限制宽度并换行。顶部按钮、歌名及歌手的提示向下显示，底部播放来源向上显示。歌名和歌手按实际可见文字宽度定位，短名称右侧的空白不会触发提示，长名称仍可省略显示。切歌时清除旧提示，避免残留上一首歌的信息。设置页也使用相同的黑色、无箭头配置。

切歌时，封面以约 520 ms 的滑动、缩放和轻旋转交替。文字采用逐行遮罩揭露：旧文字用约 120 ms 退出，新歌名从下方进入，歌手晚 60 ms 跟随，整组约 520 ms 完成；两行独立裁切，不做模糊或逐字等待。普通进度、暂停/恢复及封面延迟到达不重复播放整组动画；系统开启“减少动态效果”时使用 120 ms 的短淡入。动画复用已有 Motion，未增加动画库。

播放进度条使用 React Bits Star Border 包裹整条进度槽，保留原组件上下两层反向平移、淡入淡出及往返动画，每个单程 6 秒，不再使用先前的单束内部扫光。整体高度 6 px，上下各留 1 px 流光边缘，内层 4 px Radix Progress 继续显示真实媒体时间线；不透明内层隔开边缘动画，避免把装饰光误看成已播放进度。跳转只改变内层填充，外框始终覆盖整条槽。颜色跟随当前重点色平滑过渡。暂停、停止、无有效时间线、进度为 0% / 100%、窗口隐藏或启用“减少动态效果”时不渲染动画层。保留无障碍进度语义；进度条本身不新增播放控制、WebGL 或依赖。

### 可选音频响应背景

设置页新增一个「音频响应背景」开关，默认关闭，配置单独保存在 `helper-background-v1`，跨窗口同步。启用后使用用户指定的 React Bits Aero Shards 原组件，保留其碎片几何、材质、着色器与渲染管线；本地适配负责音频驱动、配色及生命周期，不用相似自制特效替代。背景位于内容下方、裁切在悬浮窗内，不接收鼠标事件，叠加暗色遮罩以保留文字和控件可读性。

- 碎片密度 `density=1.35`、固定尺度 `scale=1.15`、辉光 `glow=1.8`、泛光 `bloom=0.8`；相比最初预览增加密度和光感，保留前景暗色遮罩。
- 低频仅驱动纵深 `depth=1.00–1.05`，不再改变 scale 或 spread。中频在基础速度 `0.45` 的 ±10% 内控制流速（正常活动时 `0.405–0.495`）并驱动原有扰动；归一化最强频段低于 `0.08` 时，运动包络进一步平滑减速至静止。此包络只影响背景运动，不改变音频采集或频段分析。高频继续驱动局部高光、辉光和有限亮度变化，不作全屏闪烁。
- 读取现有三色调色板的插值颜色，自动封面色与手动同色系均适用；静音时切换配色仍完成过渡，不推动碎片位置。
- 与 Strands 共用 `useAudioSignal` 的一个串行采样流，复用既有播放器进程音频回环，不额外捕获、不读取麦克风或系统混音。背景开关不改变捕获对象，关闭背景不影响 Strands。
- 普通静音平滑衰减后停止绘制；暂停、来源/歌曲切换及隐藏窗口时清除旧频段，恢复窗口不会重播旧能量。音频纵深与基础缩放分离，不阻止原组件的自适应画质恢复。
- 关闭背景释放所属 GPU 资源；初始化期间关闭也执行清理。系统减少动态效果、WebGPU 不可用或初始化失败时保留静态背景和可用前景，不强行模拟音频活动。

WebGPU 运行时使用 `vgpu 0.3.1`，以独立分块按需加载；许可见 `licenses/vgpu.txt`。当前只提供调试版预览，未更新正式版安装包。

### 自动写入 Key/Scale

设置页「调性」区新增「自动写入 Key/Scale」开关，默认关闭，保存在 `helper-auto-apply-v1` 并跨窗口同步。开启且已连接支持 Key/Scale 的插件时，主窗口按当前歌曲的调性分析建议（与标题调式共用同一个每秒一次的 `get_key_detection` 读取）对每个（连接、歌曲、Key/Scale）组合只写入一次 Major/Minor，两项合并为一次 apply；建议为全音阶（证据不足）时不写入，插件保持现有 Key/Scale。切歌、重新连接或关闭开关后重置记忆；写入失败只提示一次，不自动重试。标题栏在调式标签旁显示「建议 F♯ 小调 · 已写入/未写入」，悬停查看分析秒数与原因。浏览器测试 `tests/auto-apply-key-scale.spec.ts` 使用受控 IPC 验证开关持久化与写入次数，不代表真实插件已验收音频效果。

## 运行

当前预览请使用 `src-tauri/target/debug/tune-love.exe`，然后在兼容的音乐软件中播放。`target/release` 中的旧正式构建未随本轮功能更新。

运行要求：Windows 10/11 和 WebView2 Runtime。当前仅构建和验证 Windows x64；不代表已经验证全部播放器和操作系统。

音乐信息来自 Windows GSMTC 媒体会话；Strands 从当前已确认播放器的进程音频回环接收真实 PCM。应用不读取麦克风、不注入播放器或宿主、不保存或上传录音及歌曲信息。播放器不向系统提供会话时显示等待状态；不提供封面或时间线时显示缺省状态。歌手信息按播放器提供的 Artist / Album Artist 显示，不推测作词、作曲人员。

多个会话并存时使用 `nowplaying` 的自动选择规则：正在播放的会话优先，其次系统当前会话。浏览器视频也可能被选中，这是系统媒体会话的范围，尚未做音乐类型筛选或手动选择器。

## 当前播放器音频与 Strands

播放状态文字复用 React Bits Shiny Text，约 2.4 秒完成一轮柔和扫光，底色跟随强调色。字形位置和尺寸固定；暂停、停止、窗口隐藏或启用“减少动态效果”时卸载动画，显示静态可读文字。使用项目已有 Motion，不新增依赖；源代码及许可出处见 `src/components/react-bits/ShinyText.tsx` 和 `licenses/React-Bits.txt`。这是同类视觉效果，不宣称与 ChatGPT 私有界面实现完全一致。

“正在播放”左侧基于 React Bits Strands 改造，保持 46 × 22 尺寸及文字间距。三条线固定对应低频（20–250 Hz）、中频（250–2,000 Hz）、高频（2,000–20,000 Hz），分别使用色板第 1/2/3 色；画面中低频始终在下、中频固定居中、高频始终在上。低频、高频由各自能量驱动真实上下位移和弯曲变化；中频直线不改变位置，只响应强弱与亮度。位移方向固定，不再推进自主旋转、相位或颜色轮换。固定能量下形态固定。静音后外侧两线平滑收拢并停止绘制；暂停、隐藏页面、数据过期或来源不匹配时不使用旧信号。系统减少动态效果或 WebGL2 不可用时显示同样上下顺序的三色静态图形。

频段分析复用 RustFFT：48 kHz PCM、2048 点 Hann 窗、512 帧步长。左右声道独立计算功率再取平均，避免反相音频抵消。每个频段独立维护最近 96 个分析帧（约 1.024 秒），取有效能量的第 80 百分位作为参考，以 `1 - exp(-1.6 × RMS / reference)` 得到持续能量；不是把每帧最大值直接拉满。高频保持这一曲线。低频、中频另外比较各自历史的第 20 / 95 百分位，将持续能量与短时起伏融合，避免 808 的持续底音掩盖鼓点、连续中频掩盖发音强弱。起伏小于参考值 8% 时不扩展，到 20% 时完全使用增强曲线；保留持续能量，避免放大小幅 FFT 波动形成假节奏。

低、中频的持续曲线固定乘以 0.8，为真实起伏预留显示空间，再向上叠加增强值；不会按历史范围把持续音压低后突然恢复，避免旧鼓点离开历史窗口时出现假跳动。降低播放音量后，参考会随短期历史更新，仍保留相对强弱和短促鼓点。固定 -80 dBFS RMS 静音门限与当前最强频段 1% 的相对门限，抑制极低能量和跨频段泄漏；低于门限的极小声音仍会停止响应，并非任意音量都无条件放大。这是频段内动态对比增强，不是底鼓、808 或人声分离，同频段的其他乐器同样会影响对应线条。

前端音频请求约 16 ms 间隔且不重叠，起跳时间常数 14 ms、回落 32 ms；相比原来的 90 ms 回落，连续短拍之间减少拖尾。这里的时间常数并非完全归零所需时间。隐藏或减少动态效果时停止轮询。初始采样窗约 43 ms，FFT 频率间距约 23.4 Hz；交界附近存在窗函数与频率分辨率造成的交叠，并非理想砖墙分频。来源/歌曲/暂停/断续/过期时，频谱与归一化历史一起清理。该可视化 IPC 只传三个归一化能量值 `bands: [low, mid, high]`，不传输原始 PCM；它与下述独立的 Key / Scale 调性分析不是同一个算法或轮询。

音频需要 Windows build 20348 或更新版本的进程回环接口。仅捕获已确认播放器进程及其子进程，不回退到系统混音。匹配使用精确 AUMID、Windows 注册的本地程序路径或唯一可确认的同名程序树；多个无关联候选、权限失败或无法确认来源时停止采集。进程创建时间和持有的系统句柄用于检查目标存活及 PID 重用。采集、媒体元数据和绘制分别运行，采集失败不会阻塞歌曲信息。

这是应用进程级隔离：同一个播放器的提示音、同一浏览器进程树的其他标签页可能被包含，不等于按歌曲或标签页分离。受保护音频、不同播放器和不同 Windows 版本需要分别验证，不能由一个播放器测试推断全部兼容。

PCM 为 48 kHz、双声道 f32，后台只保留最近最多 8 秒的内存缓冲。来源或歌曲变化、暂停、断流和退出时清理；断续标志重置分析历史，过期信号归零。调式工作者从 `AudioState::analysis_window()` 取得私有副本，至少 6 秒才分析，最多一个任务并在完成后等待 1 秒；原始样本不通过 WebView IPC、不写录音文件。前端约每秒只读一次 `get_key_detection`，严格核对来源、歌曲标识和媒体目标代次，窗口隐藏时停读，暂停同一歌曲时保留已确认标签。

调式引擎固定使用原版 libkeyfinder v2.2.6 与 FFTW 3.3.10；构建来源、哈希、许可和 Windows MSVC 薄适配说明见 [`docs/keyfinder-dependencies.md`](docs/keyfinder-dependencies.md)。连续 3 个新候选才首次显示，替换结果需要连续 5 个新候选；这是稳定策略，不是校准置信度。合成 C 大调/A 小调样例只能验证原生链路和枚举映射，不能证明真实歌曲准确率。已记录的单音、宽带噪声和点击列会被原生引擎误判为具体调式，因此不能把这些输入或强鼓点的结果当成可信音乐学结论；真实歌曲仍需独立标签对照。

采集复用固定版本 `wasapi 0.24.0`，窄范围本地补丁保护 SILENT / null 数据包，出处及改动见 `src-tauri/vendor/wasapi/PATCHES.md`。上游音频激活包含同步等待，因此放在独立线程，并在返回后重新验证来源；不承诺 Windows 激活本身有严格时间上限。

## 从源码构建

新克隆的仓库需要先取得两个未入库的工具（仅 Windows，需要 PowerShell 7 的 `pwsh`）：

- `npm run fetch:cmake`：下载固定版本 CMake 3.30.5 到 `src-tauri/vendor/tools`，校验 SHA-256。任何 Rust 构建（含 `npm run tauri dev`）之前都要先运行，`src-tauri/build.rs` 用它编译 FFTW。
- `npm run fetch:python`：下载固定版本的官方嵌入式 Python 到 `src-tauri/python`，校验 SHA-256，并在 `python313._pth` 中加入 `..\reference`。只在打包安装包时需要，日常开发不用。

两个脚本都可重复运行：目标已存在时只提示 `already present`，不会再次下载。`-Destination <目录>` 可改变解压位置，`-SelfTest` 在临时目录里自检。

## 开发

需要 Node.js、Rust stable、MSVC Build Tools 和 WebView2。

```powershell
npm ci
npm run tauri dev
```

纯浏览器预览 `npm run dev` 只展示界面空状态，不模拟本机音乐读取。设置按钮在浏览器预览中打开单独的命名窗口；桌面版使用 Tauri 原生窗口。设置页地址为 `/?view=settings`。

```powershell
npm test
npm run test:ui
npm run build
cd src-tauri
cargo test
cargo run --example media_probe
cargo run --example audio_probe
cargo run --example audio_isolation
```

`media_probe` 是只读的真实媒体信息探针。`test:ui` 使用 Edge 无头浏览器，第二个场景明确采用测试数据验证长标题、缺封面、暂停状态。

浏览器调式测试只在 IPC 边界提供受控快照，检查 24 个标签、坏值/旧代次拒绝、暂停保留、相同结果不重播动效、减少动态效果和 468 × 242 视口布局；它不伪造原生 PCM 检测。原生调式链路必须在新的 debug build 正常打开、真实播放器有声时另行运行：

```powershell
node scripts/verify-key-detection.mjs
```

该脚本在当前 CDP 主页面进行最长 40 秒的只读采样，只允许媒体、音频、窗口状态和 `get_key_detection` 读取；它核对来源/歌曲/代次匹配、最终标题标签和 14 px / 650 字重，保存截图及不含歌曲身份、PCM 或 base64 的元数据报告，恢复 IPC 包装后保持应用与播放器打开。读到调式只证明真实链路运行；没有独立歌曲标签时不能据此声称识别正确。

`audio_probe` 只读当前来源并报告 PCM 帧数、RMS、峰值和进程身份，不输出原始音频。`audio_isolation` 创建两个自己的低音量播放进程，依次测试静音、目标单独发声、其他进程单独发声、同时发声和恢复静音；它不会操作你的播放器，并在结束时关闭自己的测试进程。测试音幅度为 0.01。隔离结论要求目标信号非零、非目标信号不混入，还要单独确认非目标进程实际输出了非零音频。

构建可独立启动的 exe：

```powershell
npm run tauri -- build --no-bundle
```

## 桌面集成检查

```powershell
npm run tauri -- build --debug --no-bundle --config src-tauri/tauri.verify.conf.json
```

先正常关闭正在运行的本应用，再从本项目目录启动 debug exe，运行 `npm run test:native`。测试配置对主窗口与设置窗口使用相同的本地 WebView2 调试参数（端口 19224）；正式 release 配置不启用调试端口。本机观察到旧 release 与测试 debug 并存时，后者可能在 WebView2 创建阶段报 `0x8007139F`，因此验收时串行启动，不假设两个配置能够同时运行。测试操作本应用的设置窗口、置顶和关闭，不改变媒体播放器状态。设置验收包含原生窗口尺寸、可调整大小标志、重复/并发打开、关闭和重开、主窗口退出时清理设置窗口。需要已有一个提供封面的媒体会话。

截图和本机验收数据保存在被 `.gitignore` 排除的 `artifacts/`，不要把个人听歌记录发布到开源仓库。关闭测试应用后才能再次链接同名 exe。

当前播放器有声时，以 `$env:HELPER_EXPECT_AUDIO='1'; npm run test:native` 开启音频验收：检查真实 IPC 的来源、歌曲、时间戳和 PCM 强弱，并比较原生 Strands 截图的解码像素。测试仍会恢复用户配色并正常关闭本应用。浏览器界面测试采用受控 IPC fixture，只证明界面行为；真实 PCM 和隔离结论以原生探针与 `audio_isolation` 实测为准。

调试版已启动、播放器正在播放且提供有效时间线时，可运行 `node scripts/verify-progress-star-border.mjs`。该只读检查使用真实媒体 IPC，核对进度前进、6 px 高度、双层反向移动及上下边缘的实际像素变化，并检查装饰光不进入内层填充；不替换歌曲数据、不操作播放器、不改设置，验收后保持调试窗口打开。截图和报告写入独立的 `artifacts/progress-star-border-native-<时间戳>/`；之前的单束扫光验收材料保留为历史记录，不作为当前效果的证据。

背景原生验收使用 `node scripts/verify-aero-background.mjs`：通过真实设置开关开启/关闭，检查当前播放器 IPC 的来源、歌曲与新鲜度，记录真实 WebGPU uniform 与画布像素变化，并核对 GPU 释放和重新创建。默认恢复背景设置、保留原配色并保持主窗口打开；添加 `--leave-enabled` 在验收成功后留下开启的预览。所有 GPU 诊断包装均转发原方法并在 finally 中恢复，不替换真实媒体/音频、不操作播放器。报告及截图存于独立 `artifacts/aero-shards-native-<时间戳>/`。浏览器测试另外覆盖静音、暂停、隐藏、过期/错来源、配色插值、减少动态效果、WebGPU 不可用、写入失败和负载后画质恢复；受控数据测试与真实原生证据分开记录。

悬停控制区原生验收使用 `node scripts/verify-player-controls.mjs`：连接真实 debug 窗口，检查悬停切换、紧凑 Elastic Slider 的真实拖动/键盘/边界/回弹、Comet Dial 的圆弧拖动/彗尾/精确数字联动/草稿切换同步、数值文本居中、Natural Vibrato 零点双向进度、参数作用提示不被小窗裁切、面板无溢出及本地按钮状态。旋钮与名称的 Warm Tooltip 解释作用，拖动时不再重复显示数字；电音强度仍保留实时百分比提示。验收期间仅允许媒体/音频/窗口状态只读 IPC，意外控制请求会被阻止并记为失败；不替换媒体数据、不操作播放器。脚本恢复原始前端预览值、移除诊断包装、保持配色和背景配置不变并留下主窗口。证据写入独立的 `artifacts/player-controls-native-<时间戳>/`。

真实播放控制使用独立的显式验收命令 `node scripts/verify-transport.mjs --allow-playback-changes --source-id <当前来源ID>`。它会实际暂停、恢复、下一首和上一首，仅允许指定来源及每次点击对应目标的命令穿过 IPC 边界；核对真实原生响应与媒体状态，不把自然切歌或前端图标变化算作成功。它尝试回到原歌曲及初始播放／暂停状态，不 seek、不改循环／随机模式；上一首遵循播放器本身的规则，若未回到原歌曲则如实失败，不猜测追加跳转。时间进度不会恢复到测试前位置。所有真实请求、结果和恢复检查写入 `artifacts/transport-native-<时间戳>/verification.json`，测试后移除拦截包装并保持调试窗口打开。无法观测响应的 WebView 后备传输会拒绝控制并让验收失败；不把该限制作为产品功能失败。常规 `verify-player-controls.mjs` 仍是只读预览验收，已移除播放区点击。

当前前端构建保留 Vite 的 500 kB 分块告警：入口 JS 约 704.05 kB（gzip 225.38 kB），按需加载的 Aero Shards 分块约 199.56 kB（gzip 64.03 kB）。体积优化尚未完成，没有调高阈值隐藏告警。

## 复用的项目

| 项目 | 用途 |
| --- | --- |
| [Tauri](https://tauri.app/) | 桌面窗口、打包、受限 IPC |
| [React](https://react.dev/) | 界面 |
| [Radix Themes](https://www.radix-ui.com/themes) | IconButton、Switch、Progress、Skeleton、Popover、TextField、深色主题与无障碍 |
| [Radix Primitives](https://www.radix-ui.com/primitives) | Slider 的拖动、键盘与可访问性，直接依赖现有 `radix-ui 1.6.7` |
| [Rare UI](https://rareui.com) | [Animated Counter](https://rareui.com/components/animatedcounter)，播放时间的数字过渡 |
| [Motion](https://motion.dev/) | 数字、切歌与 Warm Tooltip 的动画运行时 |
| [React Bits Warm Tooltip](https://reactbits.dev/micro/warm-tooltip) | 按钮、歌名、歌手与播放来源的共用提示框，含滑动、尺寸、文字与 Lean 动画 |
| [React Bits Strands](https://reactbits.dev/r/Strands-TS-CSS.json) | 基于官方组件改造的独立三频段、固定三色特效 |
| [React Bits Star Border](https://reactbits.dev/animations/star-border) | 原组件的上下双向边缘流光包裹完整进度槽，仅做尺寸、无障碍及动画启停适配 |
| [React Bits Elastic Slider](https://reactbits.dev/components/elastic-slider) | 紧凑电音强度浮层；保留官方弹性视觉，复用现有 Motion 和 Radix 输入 |
| [React Bits Comet Dial](https://reactbits.dev/c/micro/comet-dial) | 三参数紧凑旋钮浮层；保留官方圆弧、彗尾和回弹，接入精确输入与 Warm Tooltip |
| [React Bits Aero Shards](https://reactbits.dev/backgrounds/aero-shards) | 可开关的三频段音频响应碎片背景，保留官方渲染实现 |
| [vgpu](https://github.com/vercel-labs/vgpu) | Aero Shards 的 WebGPU 运行时，固定 0.3.1，MIT |
| [OGL](https://github.com/oframe/ogl) | Strands 的 WebGL 运行时，固定 1.0.11，Unlicense |
| [wasapi](https://github.com/HEnquist/wasapi-rs) | Windows 当前进程树音频回环，固定 0.24.0，MIT，包含本地静音缓冲补丁 |
| [react-colorful](https://github.com/omgovich/react-colorful) | 主色调色盘与 HEX 输入，MIT |
| [extract-colors](https://github.com/Namide/extract-colors) | 封面三色提取，4.2.1，MIT |
| [RustFFT](https://github.com/ejmahler/RustFFT) | 频段能量分析，6.4.1，MIT / Apache-2.0 |
| [libkeyfinder](https://github.com/mixxxdj/libkeyfinder/tree/v2.2.6) | 当前播放器调式识别，固定 v2.2.6 / `a409c7447e9f440a12627ff4a540a43e41b48a55`，GPL-3.0-or-later；原始源码和许可保留在 `src-tauri/vendor/key_detection/` |
| [FFTW](https://www.fftw.org/) | libkeyfinder 的固定 3.3.10 静态依赖，GPL-2.0-or-later；来源、哈希和构建边界见 `docs/keyfinder-dependencies.md` |
| [colord](https://github.com/omgovich/colord) | 颜色格式与深色界面对比度处理，MIT |
| [Phosphor](https://phosphoricons.com/) | 图标与应用图标 |
| [nowplaying](https://github.com/pyanexu/nowplaying) | 媒体发现、会话选择、播放状态、时间线校正 |
| [windows-rs](https://github.com/microsoft/windows-rs) | 封面读取适配、Windows Shell 来源名称／图标及可执行文件来源匹配 |
| [png](https://github.com/image-rs/image-png) | 将 Windows Shell 返回的来源图标编码为 PNG；复用项目已有依赖版本 |

自己编写的主要是产品布局、数据模型适配和有限长度/格式的封面转换。未使用任何商业助手代码或 DLL。

### UI 组件选型边界

优先核对并复用用户指定的 Rare UI；动效提示使用用户指定的 React Bits Warm Tooltip。不把其他组件库或自定义样式标成 Rare UI 实现。

2026-09-28 核对 [Rare UI 公开组件目录](https://rareui.com/components)，目录中的 21 个组件未包含开关、调色盘或通用文本输入框（OTP Input 是验证码输入，不适用于颜色值）。因此当前颜色设置保留明确的替代项：Radix Themes Switch、react-colorful HexColorPicker / HexColorInput；预设色块及布局为本项目实现，不属于 Rare UI。没有为凑齐组件来源而新增无关控件或依赖。

开关通过 Radix 的 `radius="full"` 和局部 `--accent-track` / `--focus-8` 变量跟随主色；不对根按钮绘制背景，避免在内层圆角轨道外出现方形色块。保留组件原有轨道、滑块动画与键盘焦点行为。主窗口的 `--accent` 和 `--theme-color` 使用 CSS `@property` 注册为颜色类型，浏览器可以在两首歌的颜色之间插值；因此图标、进度条和激活状态不会像未注册的 CSS 自定义属性那样直接跳变。

## 许可提醒

Rare UI 组件版权归 Swami Malode，使用 **MIT + Commons Clause + Attribution**，完整文本见 `licenses/Rare-UI.txt`。允许作为应用的一部分使用和分发，但限制将组件本身再次分发为组件库、模板包等；需要保留版权和 Rare UI 可见链接。此 README 即包含署名链接。

**不能把包含该组件的完整源码包一概标成纯 MIT。** 正式开源前需确定主项目许可并明确第三方例外；如果要求所有文件均采用 OSI 开源许可，应先替换该组件。当前未代用户选择整个项目的许可证。

## React Bits 选型笔记

已查看 [React Bits](https://reactbits.dev/) 的组件与官方 registry：

- [Fade Content](https://reactbits.dev/animations/fade-content)：适合切歌时约 150–250 ms 的短淡入。官方组件当前依赖 GSAP；本项目已经使用 Motion，不建议仅为这一处淡入再增加一套动画运行时。
- [Warm Tooltip](https://reactbits.dev/micro/warm-tooltip)：已从官方 [TypeScript/CSS registry](https://reactbits.dev/r/WarmTooltip-TS-CSS.json) 引入，源码保存在 `src/components/react-bits/`。同一窗口的连续悬停提示共享 WarmTooltipGroup，以保留官方演示的目标间滑动；电音强度百分比使用独立的手势提示组，避免切歌清理或其他按钮提示打断正在进行的拖动。两类提示均复用原组件，应用样式及长文本换行通过 `AppTooltip` 统一设置。
- [Animated Content](https://reactbits.dev/animations/animated-content) 与 [Blur Text](https://reactbits.dev/text-animations/blur-text)：用于早期动效方向参考。当前封面保留位移与缩放，文字已改成独立行遮罩揭露，去掉模糊；使用现有 Motion 的 AnimatePresence 实现。整行歌名同时出现，避免长歌名逐字播放造成等待。

Warm Tooltip 已引入 React Bits 源码，复用现有 Motion 与 React DOM，未增加 npm 依赖。Aero Shards 是后续按用户选择加入的可选背景，默认关闭、取消鼠标跟随；静音/隐藏时停止持续绘制，兼顾常驻小窗的资源使用。

React Bits 的[当前许可证](https://github.com/DavidHDev/react-bits/blob/main/LICENSE.md)同样包含 Commons Clause，不应简单当成纯 MIT；完整原文已保存在 `licenses/React-Bits.txt`，源码保留来源与版权。正式开源前需明确第三方例外及其作为应用一部分的分发限制。
