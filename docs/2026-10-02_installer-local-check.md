# 本机安装包验证记录（2026-10-02）

安装包：`src-tauri\target\release\bundle\nsis\Tune Love_0.1.0_x64-setup.exe`

SHA-256：`8614c37aecd8537a30b0ba8e674ac32d66c8707f53333433d63b5c5d7e651d98`（安装前核对一致）

结论：安装、随包 Python 自检、程序启动、worker 只读 scan、卸载均通过；迁移复制路径未被覆盖（旧文件不存在，见步骤 4 与偏差 4）；存在 4 处与计划不一致的地方，见“偏差”。

## 起始状态

- 旧目录 `%LOCALAPPDATA%\dev.autotunehelper.nowplaying\song-keys-v1.json`：不存在。
- 新目录 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\`：首次启动前不存在。
- 安装目录、`tune-love.exe` 进程：均不存在。

## 步骤

1. 安装：`"<安装包>" /S`，退出码 0。
   实际安装目录是 `%LOCALAPPDATA%\Tune Love\`（注册表 `InstallLocation` 同），不是 `%LOCALAPPDATA%\Programs\Tune Love\`。路径仍含空格，覆盖带空格路径的目的达到。
   目录内共 52 个文件：`tune-love.exe`、`uninstall.exe`、`licenses\`（11 个文件）、`python\`、`reference\`（`app_bridge.py`、`client.py`、`sender.py`、`build\x64\reference_agent.dll`、`profiles\autotune-pro-38c42d0b-x64.json`）。资源直接位于安装目录下。`python313._pth` 内容含 `..\reference`。
2. 随包 Python 自检：工作目录设为 `python\`，运行 `python.exe -c "import client, app_bridge, ctypes; print('ok')"`，输出 `ok`，退出码 0。另在 `%TEMP%` 下重复一次，也输出 `ok`。
3. 启动程序：
   - (a) 启动 `tune-love.exe`（PID 29832），等待 10 秒，进程存活，`MainWindowTitle` 为 `Tune Love`。随后对该 PID 执行 `Stop-Process`，确认进程已退出。
   - (b) 直接运行 `python\python.exe -u "<安装目录>\reference\app_bridge.py"`，写入 `{"op":"scan"}` 加换行，在 20 秒超时内读到一行响应（427 字符），是合法 JSON：`{"ok": true, "state": {"phase": "disconnected", ...}, "candidates": [{"processName": "FL64.exe", "pluginName": "Auto-Tune Pro.vst3", "profileId": "autotune-pro-38c42d0b-x64", "compatible": true, ...}], "skippedCount": 149}`。关闭 stdin 后进程退出码 0，stderr 为空。命令行经 `Get-CimInstance Win32_Process` 确认为随包 `python.exe` 与随包 `reference\app_bridge.py`。
   - 未发送 connect/apply/clear；未触碰 FL Studio 或任何 DAW 进程（scan 只枚举，结果里出现了正在运行的 FL64.exe，仅作为候选列出）。
4. 迁移检查：旧文件本就不存在，所以没有可复制的内容。启动后新目录已创建（仅有 `EBWebView`），`song-keys-v1.json` 未生成，行为符合“旧文件存在才复制”的约定。拷贝路径本身没有被这次检查覆盖。
5. 卸载：`%LOCALAPPDATA%\Tune Love\uninstall.exe /S`，退出码 0，等待 3 秒后确认安装目录已删除，卸载注册表项已消失，没有残留 `tune-love` 或 `python` 进程。数据目录 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\` 保留（NSIS 默认行为）。

## 偏差

1. 安装目录是 `%LOCALAPPDATA%\Tune Love\`，不含 `Programs`。需要同步修正文档中的默认路径描述；不影响功能。
2. `python\LICENSE.txt` 实际存在（33861 字节，与 `licenses\Python.txt` 大小相同），而计划是不随包放它。当前行为是两处各有一份。
3. scan 响应的字段是 `candidates`（含 `skippedCount`），不是 `targets`。形状是合法的 ok 响应。
4. 迁移步骤因旧文件不存在无法验证复制行为（见步骤 4）。如需覆盖，要先造一个旧目录文件再装一次，本任务未做。
