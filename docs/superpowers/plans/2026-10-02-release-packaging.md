# 改名 Tune Love、安装包与自动构建实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 项目更名为 Tune Love；每次推送都在 GitHub Actions 上构建并测试；推 `v*` 标签后自动生成附 NSIS 安装包的草稿 Release。安装版附带嵌入式 Python 与 Auto-Tune 桥接，用户不需要任何配置。

**Architecture:**
- 应用代码只改两处：
  - 名称与 identifier；
  - Auto-Tune 桥接的“桥接目录 / Python”查找顺序，抽成纯函数。
- 打包相关的内容放在 `scripts/`（下载 CMake、嵌入式 Python，以及版本号检查）和 `src-tauri/tauri.release.conf.json`（随包资源清单，只在打安装包时合并）。
- CI 分为可复用的 `ci.yml` 和 `release.yml`。

**Tech Stack:** Tauri 2 CLI（NSIS）、Rust、Node 22 + vitest/Playwright、PowerShell 7、GitHub Actions（`windows-latest`）、Python 3.13 embeddable（amd64）。

**Spec:** [2026-10-02-release-packaging-design.md](../specs/2026-10-02-release-packaging-design.md)。执行者必须同时读设计稿和本计划。

## Global Constraints

- 显示名称为 `Tune Love`，设置窗口为 `Tune Love · 设置`。
- 内部名称：
  - identifier 为 `io.github.ylzhe.tunelove`；
  - npm 包与 Rust 包名为 `tune-love`，Rust lib 名为 `tune_love`，程序文件为 `tune-love.exe`。
- 环境变量：`TUNE_LOVE_REFERENCE`、`TUNE_LOVE_PYTHON`、`TUNE_LOVE_STEMGENRT_ONNX`。旧的 `AUTOTUNE_HELPER_*` 不再读取。
- 历史报告（带日期的 `docs/*.md` 中已完成的验证、研究报告）和 `experiments/` 下的脚本保留旧名，不改写。
- 固定的版本与哈希：
  - CMake 3.30.5 `cmake-3.30.5-windows-x86_64.zip`，SHA-256 `5ab6e1faf20256ee4f04886597e8b6c3b1bd1297b58a68a58511af013710004b`；
  - 嵌入式 Python 用 3.13 系列最新补丁版，版本号与 SHA-256 固定在脚本里（任务 4 确定）。
- 不入库：`src-tauri/python/`、`src-tauri/vendor/tools/`、`reference/build/`、`reference/vendor/`、安装包与一切二进制。
- 用户不需要配置任何东西；`TUNE_LOVE_*` 只给开发调试用。
- 外部 Action 只用 GitHub 官方的 `actions/checkout`、`actions/setup-node`、`actions/cache`、`actions/upload-artifact`、`actions/download-artifact`，以及 `dtolnay/rust-toolchain`，都固定到完整的提交 SHA，并在注释里写明对应的版本号。
- workflow 权限：`ci.yml` 为 `contents: read`，`release.yml` 为 `contents: write`。
- 推送、打标签、发布 Release 都由用户执行或明确同意后执行。

## Review Focus

1. **安装路径含空格或非 ASCII 字符**，例如 `C:\Users\张三\AppData\Local\Programs\Tune Love\`。期望：桥接与 Python 都能正常启动。测试在任务 3 和任务 7。
2. **开发或 CI 构建的程序旁边没有随包资源，或只有不完整的资源。** 期望：只有当 `<resource_dir>/reference/app_bridge.py` 真实存在时，才使用随包资源；否则回到开发目录，绝不选中半套资源。测试在任务 3。
3. **标签与版本号不一致**，例如 `v0.1` 或 `v0.1.0` 对 `0.1.1`，或标签带预发布后缀。期望：Release 构建在打包前就失败，并说明哪一处不一致。测试在任务 5。
4. **迁移时旧缓存文件损坏、新目录不可写，或新旧目录都有数据。** 期望：程序照常启动，永远不覆盖新目录中已有的数据。测试在任务 2。
5. **下载中途断网或哈希不符。** 期望：脚本返回非 0，不留下残缺的目标文件；重跑可以恢复。测试在任务 4。

---

## 文件结构

```text
now-playing/
  package.json, package-lock.json        # name → tune-love；加 scripts：fetch:cmake, fetch:python, release:check
  index.html, src/main.tsx, src/components/KeyTitle.tsx, src/components/SettingsButton.tsx   # 显示名称
  src-tauri/Cargo.toml, Cargo.lock        # package tune-love, lib tune_love
  src-tauri/src/main.rs, src/lib.rs       # 调用 tune_love::run；setup 里迁移与资源目录
  src-tauri/src/migrate.rs                # 新：迁移按歌缓存
  src-tauri/src/autotune.rs               # 新纯函数 resolve_bridge_dir / resolve_python；AutotuneState::set_resource_dir
  src-tauri/examples/*.rs                 # helper_now_playing:: → tune_love::
  src-tauri/tauri.conf.json               # 名称、identifier、bundle(nsis, currentUser, licenseFile)
  src-tauri/tauri.verify.conf.json        # 窗口标题
  src-tauri/tauri.release.conf.json       # 新：bundle.resources
  scripts/fetch-cmake.ps1                 # 新
  scripts/fetch-python-embed.ps1          # 新
  scripts/release-version.mjs             # 新：checkVersions() + CLI
  scripts/release-version.test.ts         # 新（vitest include 扩展到 scripts/**/*.test.ts）
  scripts/fetch-stemgenrt.mjs             # 默认路径改为新 identifier
  tests/*.spec.ts, scripts/verify-key-detection.mjs   # 断言里的名称
  README.md                               # 新名、构建步骤、未签名与误报说明
  docs/superpowers/specs/2026-10-02-devocal-engine-design.md, plans/2026-10-02-devocal-engine.md   # 管道名与环境变量
  .github/workflows/ci.yml                # 新
  .github/workflows/release.yml           # 新
  .github/release-notes.md                # 新
  .gitignore                              # + src-tauri/python/
```

---

### Task 1: 改名为 Tune Love

**Files:**
- Modify：上表中所有标注了“名称”的文件，以及 `src-tauri/Cargo.toml`、`src-tauri/src/main.rs`、`src-tauri/examples/*.rs`、`tests/key-detection.spec.ts`（第 138、167、247 行）、`scripts/verify-key-detection.mjs`（第 16 行）、`README.md`。
- 另外两份文档：去人声设计稿与计划中出现的 `autotune-helper-devocal-<pid>` 改为 `tune-love-devocal-<pid>`；`AUTOTUNE_HELPER_STEMGENRT_ONNX` 改为 `TUNE_LOVE_STEMGENRT_ONNX`。

**Interfaces:**
- Produces：
  - crate `tune_love`；
  - 窗口标题 `Tune Love` 与 `Tune Love · 设置`；
  - identifier `io.github.ylzhe.tunelove`；
  - 环境变量 `TUNE_LOVE_REFERENCE`、`TUNE_LOVE_PYTHON`（`autotune.rs` 中读取）。

- [ ] **Step 1: 先改测试断言。** 把 `tests/key-detection.spec.ts` 中的 3 处 `"AutoTune Helper"` 改为 `"Tune Love"`。
- [ ] **Step 2: 运行，确认失败。** `npx playwright test tests/key-detection.spec.ts`，期望这 3 处断言 FAIL。
- [ ] **Step 3: 改名。**
  - 在 `src-tauri/Cargo.toml` 中：`name = "tune-love"`、`default-run = "tune-love"`、`[lib] name = "tune_love"`。
  - `main.rs` 与 `examples/*.rs` 中的 `helper_now_playing::` 改为 `tune_love::`。
  - `lib.rs` 的启动失败提示改为 `"Could not start Tune Love"`。
  - `SettingsButton.tsx` 的预览窗口名改为 `"tune-love-settings"`。
  - README 标题改为 `# Tune Love`；第 64 行的程序路径改为 `tune-love.exe`。
  - `fetch-stemgenrt.mjs` 的默认目录改为 `io.github.ylzhe.tunelove`。
  - 运行 `cargo generate-lockfile --offline`，若失败则运行 `cargo update -p tune-love`，让 `Cargo.lock` 跟上。
- [ ] **Step 4: 运行，确认通过。**
  - `npm test`、`npx playwright test`、`cargo test --manifest-path src-tauri/Cargo.toml` 全部 PASS；`cargo build --manifest-path src-tauri/Cargo.toml --examples` 编译通过。
  - 运行 `git grep -n -i -E "autotune ?helper|autotune-helper|autotunehelper|AUTOTUNE_HELPER|helper_now_playing" -- . ':!docs/2026-*' ':!docs/key-*' ':!experiments' ':!package-lock.json' ':!src-tauri/src/migrate.rs'`，期望无输出。`migrate.rs` 由任务 2 新建，其中有意保留旧目录名用于迁移，所以排除在外。
- [ ] **Step 5: 提交。** 提交信息：`chore: rename project to Tune Love`。

### Task 2: 迁移按歌缓存

**Files:**
- Create: `src-tauri/src/migrate.rs`
- Modify: `src-tauri/src/lib.rs`（setup 中调用 `use_song_cache` 之前先迁移）

**Interfaces:**
- Produces：
  ```rust
  pub const LEGACY_IDENTIFIER: &str = "dev.autotunehelper.nowplaying";
  pub const SONG_CACHE_FILE: &str = "song-keys-v1.json";
  #[derive(Debug, PartialEq, Eq)]
  pub enum Migration { Copied, AlreadyPresent, NoLegacy, Failed(String) }
  pub fn migrate_song_cache(new_dir: &Path) -> Migration; // 旧目录 = new_dir.parent()/LEGACY_IDENTIFIER
  ```
- 规则：
  - 新目录已有 `song-keys-v1.json` 时，返回 `AlreadyPresent`，什么也不做。
  - 旧目录没有该文件时，返回 `NoLegacy`。
  - 其余情况：建好新目录；把文件复制到新目录下的 `song-keys-v1.json.migrating`，再改名为正式文件名；返回 `Copied`。
  - 任何 I/O 错误都返回 `Failed`，并删除临时文件，不 panic。
  - 不校验文件内容：损坏的缓存由 `SongCache::load` 照常处理。
  - 不删除旧目录。
- `lib.rs` 中：`Failed` 只用 `eprintln!` 记录，然后继续启动。

- [ ] **Step 1: 写失败测试**（`migrate.rs` 的 `mod tests`，用 `std::env::temp_dir()` 下唯一的子目录）
  - `copies_when_new_is_empty`：旧文件内容为 `{"v":1}`，复制后新文件内容相同，旧文件仍然存在，返回 `Copied`。
  - `never_overwrites_existing_new`：新旧都有文件、内容不同时，返回 `AlreadyPresent`，新文件内容不变。
  - `no_legacy_is_noop`：返回 `NoLegacy`，新目录没有被创建。
  - `corrupt_legacy_is_copied_verbatim`：旧文件内容为 `{oops`，原样复制，返回 `Copied`。
  - `unwritable_new_dir_fails_cleanly`：在新目录的位置先放一个同名**文件**，使建目录失败。返回 `Failed(_)`，并且没有残留的 `.migrating` 文件。
- [ ] **Step 2: 运行，确认失败。** `cargo test --manifest-path src-tauri/Cargo.toml migrate`。
- [ ] **Step 3: 实现，并在 `lib.rs` 中接入。**
- [ ] **Step 4: 运行，确认通过。** `cargo test` 全部 PASS。
- [ ] **Step 5: 提交。** `feat: migrate per-song key cache to the Tune Love data folder`

### Task 3: 桥接目录与 Python 的查找顺序

**Files:**
- Modify: `src-tauri/src/autotune.rs`（第 386–399 行一带；`AutotuneState`）、`src-tauri/src/lib.rs`（setup）

**Interfaces:**
- Produces：
  ```rust
  pub fn resolve_bridge_dir(env: Option<PathBuf>, resource_dir: Option<&Path>, dev_dir: &Path) -> PathBuf;
  pub fn resolve_python(env: Option<OsString>, resource_dir: Option<&Path>) -> OsString;
  impl AutotuneState { pub fn set_resource_dir(&self, dir: PathBuf); }
  ```
- `resolve_bridge_dir` 的查找顺序：
  1. `env`，只要设置了就用，不检查是否存在，方便开发者覆盖；
  2. `resource_dir/reference`，仅当其中 `app_bridge.py` 是文件时才用；
  3. `dev_dir`，即 `CARGO_MANIFEST_DIR/../reference`。
- `resolve_python` 的查找顺序：
  1. `env`；
  2. `resource_dir/python/python.exe`，仅当它是文件时才用；
  3. 系统的 `"python"`。
- `lib.rs` 的 setup 中，`app.path().resource_dir()` 成功时调用 `set_resource_dir`。
- 启动 worker 时，从 `TUNE_LOVE_REFERENCE` 和 `TUNE_LOVE_PYTHON` 读取环境变量，然后调用这两个函数。

- [ ] **Step 1: 写失败测试**（`autotune.rs` 的 `mod tests`，用临时目录）
  - `env_wins`：设置了 `env` 时，即使它不存在也直接返回。
  - `bundled_bridge_used_when_complete`：在 `res/reference/app_bridge.py` 放一个文件，返回 `res/reference`。
  - `incomplete_bundle_falls_back_to_dev`（对应 Review Focus 第 2 条）：`res/reference/` 存在，但里面没有 `app_bridge.py`，返回 `dev_dir`。
  - `bundled_python_used_when_present`：`res/python/python.exe` 存在时返回它，否则返回 `"python"`。
  - `paths_with_spaces_and_cjk`（对应 Review Focus 第 1 条）：资源目录名为 `Tune Love 测试\张三`，上面两种查找都返回包含这段路径的完整路径，原样不变。
- [ ] **Step 2: 运行，确认失败。** `cargo test --manifest-path src-tauri/Cargo.toml resolve`。
- [ ] **Step 3: 实现并接入。** 用 `Command::new(python).arg("-u").arg(script)` 传参，不经过 shell 拼接，这样路径里的空格不需要额外处理。
- [ ] **Step 4: 运行，确认通过。** `cargo test` 全部 PASS。
- [ ] **Step 5: 提交。** `feat: find the bundled bridge and Python in installed builds`

### Task 4: 下载脚本（CMake、嵌入式 Python）

**Files:**
- Create: `scripts/fetch-cmake.ps1`、`scripts/fetch-python-embed.ps1`
- Modify: `package.json`（scripts：`"fetch:cmake": "pwsh -File scripts/fetch-cmake.ps1"`、`"fetch:python": "pwsh -File scripts/fetch-python-embed.ps1"`）、`.gitignore`（加 `src-tauri/python/`）、`README.md`（“从源码构建”一节）

**Interfaces:**
- Produces：两个脚本都接受 `-Destination <dir>`。
  - `fetch-cmake.ps1` 默认解压到 `src-tauri/vendor/tools`，得到 `cmake-3.30.5-windows-x86_64/bin/cmake.exe`。
  - `fetch-python-embed.ps1` 默认解压到 `src-tauri/python`，得到 `python.exe`。
  - 成功时退出码为 0，否则非 0。
- 共同规则：
  - 先下载到 `<dest>/.download-<pid>.zip`，校验 SHA-256，解压到 `<dest>/.extract-<pid>`，再整体改名到目标位置；
  - 失败时删除所有临时文件；
  - 目标已存在且关键文件哈希正确时，跳过下载。CMake 检查 zip 解压后的 `bin/cmake.exe` 是否存在；Python 检查 `python.exe` 与 `.pth` 补丁行。
- `fetch-python-embed.ps1` 的额外规则：
  - 在 `python313._pth` 中追加一行 `..\reference`，已存在时不重复追加；
  - 解压后运行 `python.exe -c "import ctypes, json, sys; print(sys.version)"` 自检。
- 下载地址：
  - CMake：`https://github.com/Kitware/CMake/releases/download/v3.30.5/cmake-3.30.5-windows-x86_64.zip`
  - Python：`https://www.python.org/ftp/python/<ver>/python-<ver>-embed-amd64.zip`

- [ ] **Step 1: 固定 Python 版本。**
  - 查 python.org 上 3.13 系列的最新补丁版。
  - 下载一次到临时目录，计算 SHA-256，并与该版本发布页公布的哈希核对；python.org 只提供 sigstore 签名或 MD5 时，按它提供的方式核对。
  - 把版本号和哈希写进脚本常量，核对结果写进脚本注释。
- [ ] **Step 2: 写验证用例**，作为脚本的 `-SelfTest` 开关，在 `$env:TEMP` 下运行：
  - 正常下载到空目录：成功，`cmake.exe` / `python.exe` 存在。
  - 再运行一次：输出 “already present”，没有网络请求，以修改时间不变来判断。
  - 伪造错误哈希（`-ExpectedSha256 0000…`，仅限测试用的参数）：返回非 0，目标目录不存在，没有 `.download-*` 或 `.extract-*` 残留（对应 Review Focus 第 5 条）。
- [ ] **Step 3: 运行，确认失败。** 脚本还不存在，期望报错。
- [ ] **Step 4: 实现。**
- [ ] **Step 5: 运行，确认通过。** 分别运行 `pwsh -File scripts/fetch-cmake.ps1 -SelfTest` 和 `pwsh -File scripts/fetch-python-embed.ps1 -SelfTest`，期望都输出 `SELFTEST PASS`。本机现有的 `src-tauri/vendor/tools` 保持不动。
- [ ] **Step 6: 提交。** `build: add pinned CMake and embedded Python fetch scripts`

### Task 5: 版本号检查

**Files:**
- Create: `scripts/release-version.mjs`、`scripts/release-version.test.ts`
- Modify: `vitest.config.ts`（include 加上 `"scripts/**/*.test.ts"`）、`package.json`（`"release:check": "node scripts/release-version.mjs"`）

**Interfaces:**
- Produces：
  ```ts
  export function checkVersions(tag: string, versions: { tauri: string; pkg: string; cargo: string }): { ok: true; version: string } | { ok: false; error: string };
  // CLI: node scripts/release-version.mjs <tag>  → 读三处文件，ok 时打印版本并退出 0，否则打印 error 并退出 1
  ```
- 规则：
  - 标签必须匹配 `^v(\d+)\.(\d+)\.(\d+)$`，不接受预发布后缀；
  - 三处版本号都必须等于去掉 `v` 之后的部分；
  - 出错信息要写明是哪一处、实际值是什么。
  - `cargo` 版本从 `src-tauri/Cargo.toml` 的 `[package]` 段中读取 `version`，用正则即可，不引入 TOML 依赖。

- [ ] **Step 1: 写失败测试**（对应 Review Focus 第 3 条）
  - `checkVersions("v0.1.0", 三处都是 0.1.0)` 返回 `{ ok: true, version: "0.1.0" }`；
  - `"v0.1"` 和 `"0.1.0"` 返回 `ok: false`，`error` 包含 `tag`；
  - `"v0.1.0-beta.1"` 返回 `ok: false`；
  - cargo 为 `0.1.1` 时返回 `ok: false`，`error` 包含 `Cargo.toml` 和 `0.1.1`。
- [ ] **Step 2: 运行，确认失败。** `npx vitest run scripts/release-version.test.ts`
- [ ] **Step 3: 实现。**
- [ ] **Step 4: 运行，确认通过。** 先跑 `npm test`，期望全部 PASS；再跑 `node scripts/release-version.mjs v0.1.0`，期望输出 `0.1.0`、退出码 0。
- [ ] **Step 5: 提交。** `build: add release tag/version check`

### Task 6: 打包配置与本机安装包

**Files:**
- Modify: `src-tauri/tauri.conf.json`（`bundle`）
- Create: `src-tauri/tauri.release.conf.json`

**Interfaces:**
- Consumes：任务 3 的查找顺序；任务 4 的 `src-tauri/python/`；`reference/build.ps1 -Target agent` 产出的 `reference/build/x64/reference_agent.dll`；`reference/bootstrap.ps1` 产出的 `reference/vendor/{minhook,pluginterfaces}/LICENSE.txt`。
- Produces：`npx tauri build --config src-tauri/tauri.release.conf.json` 生成 `src-tauri/target/release/bundle/nsis/Tune Love_<ver>_x64-setup.exe`。
- `tauri.conf.json` 中写入 `"bundle": { "active": true, "targets": ["nsis"], "windows": { "nsis": { "installMode": "currentUser" } }, "licenseFile": "../LICENSE" }`。
- `tauri.release.conf.json` 只包含 `bundle.resources`，用映射形式，路径相对 `src-tauri/`：

  | 来源 | 安装后位置 |
  |---|---|
  | `python/` | `python/` |
  | `../reference/app_bridge.py`、`client.py`、`sender.py` | `reference/` 下同名文件 |
  | `../reference/profiles/` | `reference/profiles/` |
  | `../reference/build/x64/reference_agent.dll` | `reference/build/x64/reference_agent.dll` |
  | `../LICENSE` | `licenses/Tune-Love-GPL-3.0.txt` |
  | `../licenses/` | `licenses/` |
  | `../reference/LICENSE` | `licenses/reference-MIT.txt` |
  | `../reference/THIRD_PARTY_NOTICES.md` | `licenses/reference-THIRD_PARTY_NOTICES.md` |
  | `../reference/vendor/minhook/LICENSE.txt` | `licenses/MinHook.txt` |
  | `../reference/vendor/pluginterfaces/LICENSE.txt` | `licenses/VST3-pluginterfaces.txt` |
  | `python/LICENSE.txt` | `licenses/Python.txt` |

- [ ] **Step 1: 先确认日常构建不受影响。** 只改 `tauri.conf.json` 的 `bundle` 之后，运行 `cargo test --manifest-path src-tauri/Cargo.toml`，期望 PASS。此时 `src-tauri/python/` 不存在也必须能编译。
- [ ] **Step 2: 准备打包输入。** 依次运行 `pwsh reference/bootstrap.ps1`、`pwsh reference/build.ps1 -Target agent`、`npm run fetch:python`。
- [ ] **Step 3: 打包。** 运行 `npx tauri build --config src-tauri/tauri.release.conf.json`，期望生成安装包。
- [ ] **Step 4: 检查资源。** 用 7-Zip 列出安装包内容（runner 和本机都有 `7z`），若列不出，就在任务 7 安装后检查安装目录。上表中每个目标路径都必须存在。
- [ ] **Step 5: 提交。** `build: NSIS bundle and release resource manifest`

### Task 7: 本机安装验证

**Files:**
- Create: `docs/2026-10-02_installer-local-check.md`（结果记录）

- [ ] **Step 1: 安装。** 运行 `"<安装包>" /S` 静默安装到当前用户，默认路径为 `%LOCALAPPDATA%\Programs\Tune Love\`。这个路径带空格，同时覆盖 Review Focus 第 1 条。
- [ ] **Step 2: 用随包 Python 自检。** 运行 `"…\Tune Love\python\python.exe" -c "import client, app_bridge, ctypes; print('ok')"`。工作目录设为 `python\`，验证的是 `._pth` 补丁生效，而不是工作目录碰巧对了。期望输出 `ok`。
- [ ] **Step 3: 启动程序。**
  - 启动 `tune-love.exe` 10 秒，确认窗口标题为 `Tune Love`。
  - 打开设置页，触发一次 Auto-Tune 的 `scan`。这个操作只读，不注入。确认 worker 启动时用的是随包的 `python.exe` 和 `reference\app_bridge.py`：用 `Get-CimInstance Win32_Process -Filter "Name='python.exe'"` 查看命令行。
  - 不连接 Auto-Tune，也不碰 FL。
- [ ] **Step 4: 确认迁移。** `%LOCALAPPDATA%\io.github.ylzhe.tunelove\song-keys-v1.json` 已存在，且与旧目录中的文件内容相同。
- [ ] **Step 5: 卸载。** 用安装目录下的 `uninstall.exe /S` 卸载，确认程序目录已被删除；数据目录保留，这是 NSIS 的默认行为。
- [ ] **Step 6: 写记录并提交。** 记录每一步的结果和安装包的 SHA-256。提交信息：`docs: local installer check`。

### Task 8: GitHub Actions workflows

**Files:**
- Create: `.github/workflows/ci.yml`、`.github/workflows/release.yml`、`.github/release-notes.md`
- Modify: `README.md`（CI 徽章、Release 下载说明、未签名与误报说明）

**Interfaces:**
- Consumes：
  - `npm run fetch:cmake`、`npm run fetch:python`（任务 4）；
  - `node scripts/release-version.mjs <tag>`（任务 5）；
  - `tauri.release.conf.json`（任务 6）。
- Produces：
  - `ci.yml` 可以通过 `workflow_call` 调用；
  - `bridge` 任务上传名为 `reference-agent-x64` 的产物，内容为 `reference_agent.dll`。
- `ci.yml`：
  - 触发：`push` 到 `main`、`pull_request`、`workflow_dispatch`、`workflow_call`；`concurrency` 按分支取消旧的运行。
  - 三个任务都设 `runs-on: windows-latest`，`shell: pwsh`。
  - **`frontend`**：
    - `setup-node`（node 22，cache npm）→ `npm ci` → `npm run build` → `npm test` → `npx playwright test`；
    - 失败时上传 `artifacts/browser-tests`。
  - **`backend`**：
    - 缓存 `src-tauri/vendor/tools`（键为 `cmake-3.30.5-5ab6e1fa`），以及 `~/.cargo/registry`、`src-tauri/target`（键含 `Cargo.lock` 的哈希）；
    - `dtolnay/rust-toolchain@stable` → `npm run fetch:cmake` → `npm ci` → `npm run build` → `cargo test --manifest-path src-tauri/Cargo.toml`。
  - **`bridge`**：
    - 缓存 `reference/vendor`（键为 `bootstrap.ps1` 文件的哈希，固定的提交号写在其中）；
    - 用 runner 自带的 Python 3，不加 `setup-python`；
    - 依次运行 `pwsh reference/bootstrap.ps1` → `pwsh reference/build.ps1 -Target all` → `python -m unittest reference.tests.test_client reference.tests.test_app_bridge reference.tests.test_sender`，若这种模块式调用不可行，就按 README 的 `discover -s reference/tests -p test_<name>.py` 逐个运行；
    - 然后 `python reference/tests/test_runtime.py --evidence "$env:RUNNER_TEMP/runtime.json"`；
    - 最后上传产物。
- `release.yml`：
  - 触发：`push: tags: ['v*']`。
  - 任务链：`version` → `ci`（`uses: ./.github/workflows/ci.yml`）→ `package`（`needs: [version, ci]`）。
  - `package` 的步骤：
    - `download-artifact` 把 `reference-agent-x64` 下载到 `reference/build/x64/`；
    - `pwsh reference/bootstrap.ps1`，以取得许可原文；
    - `npm run fetch:cmake`、`npm run fetch:python`、`npm ci`；
    - `npx tauri build --config src-tauri/tauri.release.conf.json`；
    - 生成 `SHA256SUMS.txt`；
    - `gh release create $tag --draft --title "Tune Love $version" --notes-file .github/release-notes.md <安装包> SHA256SUMS.txt`，环境变量 `GH_TOKEN: ${{ github.token }}`。
- `.github/release-notes.md` 的固定内容（中文），逐条写明：
  1. 安装包未签名，SmartScreen 提示时点“更多信息 → 仍要运行”；
  2. Auto-Tune 控制会向宿主注入 agent 并挂钩插件，可能被杀毒软件误报，原因说明；
  3. 升级前请关闭本程序和已连接的宿主，否则 agent 文件被占用、无法覆盖；
  4. StemgenRT 权重不随包，开发阶段用 `npm run fetch:stemgenrt -- --accept` 下载；
  5. 仅供免费、非商业使用，许可证为 GPL-3.0，第三方许可在安装目录的 `licenses\` 下。

- [ ] **Step 1: 写 workflow 与说明。** 用 `gh api repos/<owner>/<repo>/commits/<tag>` 或 `git ls-remote` 查出每个 Action 当前版本对应的提交 SHA，在注释里写明版本号。
- [ ] **Step 2: 本地静态检查。**
  - 从 `rhysd/actionlint` 的 GitHub Release 下载 Windows 版到 `$env:TEMP`，用同一 Release 的 checksums 文件核对后运行。
  - 运行 `actionlint .github/workflows/*.yml`，期望没有输出。
- [ ] **Step 3: 提交。** `ci: build, test and draft-release workflows`
- [ ] **Step 4: 交给用户推送。** 请用户推送 `main`，或者在用户同意后由执行者推送。
- [ ] **Step 5: 看 CI 结果。**
  - 用 `gh run watch` 或 `gh run view --log-failed` 查看第一次运行的日志，逐项修复，每修一项提交一次，推送仍走第 4 步的方式。
  - 如果 `test_runtime.py` 因 runner 环境（例如注入受限）失败，先和用户商量，不擅自删除这个测试。
- [ ] **Step 6: 发版演练。** CI 全绿后，请用户推送 `v0.1.0` 标签。确认草稿 Release 里有安装包和 `SHA256SUMS.txt`；`SHA256SUMS.txt` 中安装包的哈希与 Release 页面上的文件一致。由用户决定是否发布。
