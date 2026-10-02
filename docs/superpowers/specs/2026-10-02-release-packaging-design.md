# 改名 Tune Love、安装包与自动构建设计

日期：2026-10-02。状态：设计已逐段获用户确认，待审阅书面稿。仓库：https://github.com/YlZHE/Tune-Love（GPL-3.0）。

## 1. 目标

1. 项目正式更名为 **Tune Love**。
2. 每次推送和 PR 都在 GitHub Actions 上自动构建并测试。
3. 推送 `v*` 标签后，自动生成**草稿 Release**，附 NSIS 安装包。用户在 GitHub 上看过后再手动发布。
4. 用户安装后不需要另装 Python，也不需要源码目录，Auto-Tune 控制就能用。

成功标准：

| 项目 | 标准 |
|---|---|
| 改名 | 对外显示与内部名称都已改为新名；除历史报告与实验脚本外，`git grep` 找不到旧名；现有测试全部通过 |
| CI | `ci.yml` 的三个任务在 `windows-latest` 上全部通过 |
| 安装包 | 本机 `npx tauri build` 生成的安装包能装到当前用户、能启动，并能找到随包的桥接目录与 Python |
| Release | 推 `v0.1.0` 后得到一个草稿 Release，内含安装包与 `SHA256SUMS.txt` |

## 2. 用户已确认的决定

- 安装版附带官方 Windows 嵌入式 Python，不要求用户自己安装 Python，也不把桥接改写成 Rust。
- 安装包用 NSIS `.exe`，装到当前用户，不需要管理员权限。
- 推 `v*` 标签后生成草稿 Release，由用户确认后发布。
- identifier 改为 `io.github.ylzhe.tunelove`，并迁移旧数据（见 §3）。
- 以下两项**放到最后**，不在本稿实现（见 §8）：
  - 程序内置下载；
  - 首次启动的设置向导。

## 3. 改名

| 类别 | 旧 | 新 |
|---|---|---|
| 显示名称（`productName`、窗口与网页标题、`KeyTitle` 默认文字、启动失败提示、安装包与开始菜单） | AutoTune Helper | Tune Love（设置窗口为 `Tune Love · 设置`） |
| identifier | `dev.autotunehelper.nowplaying` | `io.github.ylzhe.tunelove` |
| npm 包名 / Rust 包名 / 程序文件 | `autotune-helper-now-playing` | `tune-love`（`tune-love.exe`；Rust lib 名 `tune_love`） |
| 环境变量 | `AUTOTUNE_HELPER_REFERENCE`、`AUTOTUNE_HELPER_PYTHON`、`AUTOTUNE_HELPER_STEMGENRT_ONNX` | `TUNE_LOVE_REFERENCE`、`TUNE_LOVE_PYTHON`、`TUNE_LOVE_STEMGENRT_ONNX` |
| 去人声管道名（设计稿与计划） | `autotune-helper-devocal-<pid>` | `tune-love-devocal-<pid>` |

**改名范围：**
- 示例程序、测试、`scripts/`、README，以及仍在使用的设计稿和计划，都要同步改。
- 已完成的历史报告和实验脚本保留旧名，按项目约定不改写历史证据。

**数据迁移：**
- 启动时，若新数据目录 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\` 中没有 `song-keys-v1.json`，而旧目录 `%LOCALAPPDATA%\dev.autotunehelper.nowplaying\` 中有，就把它复制过来。
- 旧目录不删除。
- 复制失败只记日志，不影响启动。

`scripts/fetch-stemgenrt.mjs` 的默认目标路径同步改到新目录。

## 4. 安装版的组成与运行时查找

**打包配置**（`tauri.conf.json`）：

```json
"bundle": {
  "active": true,
  "targets": ["nsis"],
  "windows": { "nsis": { "installMode": "currentUser" } },
  "resources": { … 见下 … },
  "licenseFile": "../LICENSE"
}
```

**随包资源**（安装后位于 `resources/` 下，保持与源码相同的相对结构）：

| 资源 | 来源 |
|---|---|
| `reference/app_bridge.py`、`client.py`、`sender.py`、`profiles/*.json` | 仓库 |
| `reference/build/x64/reference_agent.dll` | 由 `reference/build.ps1 -Target agent` 构建，只放这一个 DLL，不放测试插件或测试宿主。`app_bridge.py` 按自身所在目录查找它，所以脚本不用改。 |
| `python/` | `scripts/fetch-python-embed.ps1` 下载的官方嵌入式 Python（amd64） |
| `licenses/` | 本项目 GPL-3.0、现有 `licenses/*`、`reference/LICENSE` 与 `THIRD_PARTY_NOTICES.md`、MinHook 与 VST3 pluginterfaces 的许可原文（bootstrap 拉取后从 `reference/vendor/*/LICENSE.txt` 复制）、Python 许可 |

**`scripts/fetch-python-embed.ps1`：**
- 下载 python.org 上 Python 3.13 最新补丁版的 `python-3.13.x-embed-amd64.zip`。脚本中固定具体版本号与 SHA-256：首次下载时计算，并与 python.org 公布的值核对。
- 校验通过后，解压到 `src-tauri/python/`（不入库）。
- 在 `python313._pth` 中追加一行 `..\reference`。
  - 原因：嵌入式 Python 存在 `._pth` 时，不会把脚本所在目录加入 `sys.path`，`app_bridge.py` 会导入不了同目录的 `client.py`。
  - 两个目录在安装包里是同级的，所以用这个相对路径。
- 重复运行时，已校验过的文件不会重复下载。

**`scripts/fetch-cmake.ps1`：**
- 下载 Kitware 官方 GitHub Release 上的 `cmake-3.30.5-windows-x86_64.zip`，校验 SHA-256 `5ab6e1faf20256ee4f04886597e8b6c3b1bd1297b58a68a58511af013710004b`。
- 校验通过后，解压到 `src-tauri/vendor/tools/`，即 `build.rs` 查找的位置。
- 已存在且完整时跳过。
- 这个脚本也写进 README，供新克隆仓库的人使用。

**运行时查找顺序**（`src-tauri/src/autotune.rs`，抽成可单测的纯函数）：
- 桥接目录：`TUNE_LOVE_REFERENCE` → `<resource_dir>/reference`（存在 `app_bridge.py` 时）→ 开发路径 `CARGO_MANIFEST_DIR/../reference`。
- Python：`TUNE_LOVE_PYTHON` → `<resource_dir>/python/python.exe`（存在时）→ 系统的 `python`。

**版本号：** 标签 `vX.Y.Z` 必须同时等于 `tauri.conf.json`、`package.json` 和 `src-tauri/Cargo.toml` 中的版本号。

**不变的部分：**
- 识调库仍静态编进主程序。
- StemgenRT 权重不打包。
- 去人声引擎在其代码完成后，按去人声计划以 `externalBin` 加入。

## 5. Workflows

全部运行在 `windows-latest` 上。外部 Action 只用 GitHub 官方的 `checkout`、`setup-node`、`cache`、`upload-artifact`、`download-artifact` 和 `dtolnay/rust-toolchain`，都固定到提交号。

**`.github/workflows/ci.yml`**

- 触发：推送到 `main`、提交 PR、手动运行（`workflow_dispatch`），也可被 `release.yml` 调用（`workflow_call`）。
- 权限：`contents: read`。
- 三个任务并行：

| 任务 | 步骤 |
|---|---|
| `frontend` | `npm ci` → `npm run build` → `npm test` → `npx playwright test`（Edge）；失败时上传 `artifacts/browser-tests` |
| `backend` | `fetch-cmake.ps1` → `npm ci && npm run build`（Tauri 编译时要嵌入 `dist`）→ `cargo test --manifest-path src-tauri/Cargo.toml` |
| `bridge` | `reference/bootstrap.ps1` → `reference/build.ps1 -Target all`（含 C++ 队列测试）→ `python -m unittest`（`test_client.py`、`test_app_bridge.py`、`test_sender.py`）→ `test_runtime.py --evidence $RUNNER_TEMP/...` → 上传产物 `reference_agent.dll` |

- 缓存：npm；cargo（registry 与 target）；CMake（键为版本号）；`reference/vendor`（键为两个固定的提交号）。

**`.github/workflows/release.yml`**

- 触发：推送 `v*` 标签。权限：`contents: write`。
- 步骤：
  1. **`version`**：检查标签与三处版本号一致。
  2. **`ci`**：`uses: ./.github/workflows/ci.yml`。
  3. **`package`**（依赖前两步）：
     - 下载 `reference_agent.dll`，放到 `reference/build/x64/`；
     - 运行 `fetch-cmake.ps1` 与 `fetch-python-embed.ps1`；
     - `npm ci`，再 `npx tauri build`；
     - 计算 `SHA256SUMS.txt`；
     - 运行 `gh release create <tag> --draft --title "Tune Love <版本>" --notes-file <说明>`，附上安装包与 `SHA256SUMS.txt`。
- Release 说明模板（`.github/release-notes.md`）写明：
  - 安装包未签名，SmartScreen 会提示，要点“仍要运行”；
  - agent 注入宿主，可能被杀毒软件误报，并说明原因；
  - StemgenRT 权重需另行下载；
  - 仅供免费、非商业使用；
  - 许可证为 GPL-3.0。

## 6. 测试与验证

**本机：**
- 改名后，`npm test`、`npx playwright test`、`cargo test` 全部通过；`git grep` 旧名只出现在历史报告与实验脚本中。
- 数据迁移单元测试：新目录为空时复制；已有数据时不覆盖；旧目录不存在时什么也不做；复制失败时不报错。
- 查找顺序单元测试：环境变量、安装目录、开发目录各一种情况，桥接目录与 Python 都要选对。
- 两个下载脚本：
  - 下载到临时目录，校验哈希；
  - 改坏哈希时必须失败，且不留下残缺文件；
  - 不覆盖本机现有的 CMake。
- 本机打包：
  - `npx tauri build` 后，检查安装包中的资源清单；
  - 用随包的 `python\python.exe` 运行 `import client, app_bridge, ctypes`；
  - 装到当前用户后启动，确认找到了随包的桥接目录与 Python；
  - 不连接 Auto-Tune，也不操作 FL；
  - 装完卸载。
- `actionlint` 检查两个 workflow。

**GitHub 上：**
- 推送后，CI 第一次运行，按日志修复失败项，可能要来回几轮。
- 发版时由用户推 `v0.1.0`，检查草稿 Release。

## 7. 风险

- **未签名**：SmartScreen 会提示“未知发布者”。免费的开源签名（如 SignPath Foundation）需另行申请，不在本稿内。
- **杀毒误报**：agent 会用 MinHook 注入宿主、挂钩插件的 process 入口，可能被误报。这一点在 README 与 Release 说明中解释。
- **CI 运行环境**：`test_runtime.py` 会向自带的测试宿主注入 agent，在 GitHub runner 上未必能跑通；如果是环境原因跑不通，再与用户商量处理方式。Playwright 用的是 runner 自带的 Edge。
- **权限检查**：此前推送 `reference/` 时被 Claude Code 的权限检查拦过，提交和推送可能需要用户手动完成。

## 8. 后续（用户要求放在最后完成，不在本稿）

1. **程序内置下载**：StemgenRT 等模型权重改由程序内下载。下载前显示来源、许可、大小，用户确认后才下载，完成后校验 SHA-256，流程见 [权重许可报告](../../../../docs/2026-10-02_model-weights-licensing.md)。这项与去人声子项目 4 合并。
2. **首次启动设置向导**：做成精美的 setup 页面，包括欢迎、模型下载确认与进度、计算设备与弱电脑档位等，完成后才进入正常界面。到时单独设计。

整体顺序：本稿（改名 → 查找顺序与资源打包 → 下载脚本 → workflows → 本机打包验证）→ 去人声子项目 1 →（中间各子项目）→ 内置下载与设置向导。
