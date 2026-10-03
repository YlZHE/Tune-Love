# 应用内下载去人声模型 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户在设置页点一次、确认一次，就能把 StemgenRT 模型下载到本机；下载按清单支持多个模型，直连失败时自动换镜像，可续传、可导入，任何来源的文件都必须逐字节通过 SHA-256 校验才能安装。

**Architecture:**
- Rust 侧新增 `src-tauri/src/devocal/model/` 模块（规格写的 `model.rs` 拆成目录模块，按职责分文件）：
  - 清单解析与来源顺序；
  - 校验与目录布局（含旧扁平文件迁移）；
  - HTTP 抓取（`reqwest`，可注入假实现）；
  - 下载核心（`.part` 续传、换源、校验、原子安装）；
  - 应用级状态和两个 Tauri 命令。
- 前端新增：
  - 纯逻辑模块 `src/modelDownload.ts`（解析、文案、格式、同意记录）；
  - 轮询 hook `src/useModelDownload.ts`；
  - 设置页“模型”区块及确认框。
- 主窗口只改提示文案，并让提示可点击，点后打开设置并定位到模型行；主窗口不轮询模型状态。

**Tech Stack:** Rust 2021（rustc 1.98）、Tauri 2.12、`reqwest` 0.13（native-tls/schannel）、`sha2` 0.10、`tauri-plugin-dialog` 2；React 19 + TypeScript、Radix Themes；vitest；Playwright（模拟 `invoke`）。

**Spec:** `docs/superpowers/specs/2026-10-03-model-download-design.md`（在 `model-download` 分支上）。调研：`.superpowers/research/2026-10-03-model-download.md`（不入库）。项目规则：`E:\autotune helper\AGENTS.md`。执行者必须同时读规格和本计划。

## Global Constraints

- **分支**：全部工作在 `model-download` 分支上进行。工作区可能与其他 agent 共用。每个提交步骤之前，先运行 `git -C "E:\autotune helper\now-playing" branch --show-current`，输出必须是 `model-download`；不是就停下，不提交、不切分支，并报告协调者。不得推送（推送须经用户同意）。
- **清单唯一**：`src-tauri/models.json` 是唯一的清单。Rust 用 `include_str!`，Node 脚本和前端都 import 同一个文件，任何地方都不再硬编码 URL、大小或哈希。
- **清单的值**（逐字照抄规格 §3）：
  - `version: 1`；
  - `mirrors: ["https://ghfast.top/", "https://ghproxy.net/", "https://ghproxy.vip/"]`；
  - StemgenRT：`id: "stemgenrt-hop128"`，文件 `model.onnx`，`bytes: 37529132`，`sha256: 77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9`，`origin: https://github.com/sweetspotsoundsystem/stemgen-rt/raw/61df8f4aa1555ef110308d01ea92b54ace770979/model/model.onnx`，`mirrorable: true`。
  - 不收录 `gh-proxy.com` 和 `gh-proxy.net`。
- **目录**：安装位置为 `<app_local_data_dir>/models/<id>/<file>`；临时文件为 `<file>.part` 和 `<file>.part.json`，都放在同一目录。
  - 正式文件名只能通过对完整、已校验的 `.part` 做 `rename` 产生。
- **来源顺序**：直连 → 自定义前缀（填了才有）→ 内置镜像（按列表顺序）。
  - 只有 `mirrorable: true` 的文件才套用前缀；
  - 镜像地址 = 前缀 + 原始地址。
- **重试与弃用**：
  - 每个来源最多重试 2 次（即最多 3 次尝试）；连接超时 10 s，读超时 30 s。
  - 连接失败、超时、读取中断、5xx、429：重试；用完重试次数后换下一个来源。
  - `content-type: text/html`、总大小与清单不符、`206` 的起点不对、其他 4xx：不重试，立即换下一个来源。
- **续传**：
  - `206` 且总大小一致才追加；`200` 就从头开始；
  - 下载完成后哈希不符：删除 `.part`，从第一个来源重新完整下载一次；再不符就报 `sha256_mismatch`。
- **“已安装”的判定**：大小一致，并且完整哈希正确。
  - 哈希按“路径 + 修改时间 + 大小”缓存；
  - 环境变量 `TUNE_LOVE_STEMGENRT_ONNX` 只对 StemgenRT 生效，优先级最高，且不校验。
- **任务数**：同一时间最多一个下载或导入任务；任务进行中再请求，返回 `already_running`。取消时保留 `.part`，只有用户执行 `delete` 才清理。
- **命令**：
  - `get_model_status() -> ModelStatus[]`，每个清单模型一项，按清单顺序；
  - `model_command({ request: { id, action, autoEnable, mirrorPrefix } }) -> ModelStatus[]`，其中 `action` 取 `download | cancel | import | delete`。参数包一层 `request`，与现有 `devocal_command` 一致。
  - 失败时返回机器码字符串。
  - 不改 `capabilities/*.json`，不改 CSP；前端继续 500 ms 轮询。
- **机器码**：`network_unreachable`、`timeout`、`http_status:<n>`、`source_html`、`size_mismatch`、`sha256_mismatch`、`disk_full`、`write_failed`、`install_denied`、`cancelled`、`already_running`、`all_sources_failed`、`invalid_prefix`、`unknown_model`、`delete_denied`、`import_mismatch`、`import_unsupported`。
- **自动开启**：只有请求里 `autoEnable: true`、模型是 `stemgenrt-hop128`、并且安装成功时，后端才走与 `devocal_command("enable")` 相同的路径。在设置里主动下载的不自动开启。
- **界面文案**：一律中文，不用 emoji。大小按 MiB 计算，但单位写作“MB”：
  - 37,529,132 字节显示为“约 36 MB”或“35.8 MB”（规格里的数字就是这样算出来的）；
  - 需要精确值的地方写“37,529,132 字节”。
- **测试与网络**：
  - 自动化测试一律不访问外网：Rust 用假抓取器或本机 `127.0.0.1` 假服务器，Playwright 模拟 `invoke`；
  - 访问真实网络的只有 `scripts/check-model-mirrors.mjs`（发版时手动运行，不进任何测试）和任务 12 的真机测试；任务 12 必须先取得用户授权。
- **许可**：权重许可固定显示为“待作者确认”，不得写成 MIT；训练数据的非商业条款必须出现在确认框里。
- **入库**：模型权重、`.part` 文件、真机日志都不入库（`.gitignore` 已有 `*.onnx`）。
- **构建前提**：Rust 构建前需要 `npm run fetch:cmake`（README“从源码构建”一节）。

## Review Focus

以下输入或失败方式规格没有写出，但最容易让用户碰上。每条都已在所属任务里加了测试。

1. **直连地址 `github.com/.../raw/...` 会 302 跳到 `media.githubusercontent.com`；镜像可能无视 `Range`，直接回 `200` 和整份文件。**
   - 期望：跳转后 `Range` 头仍在；收到 `200` 时清空 `.part` 从头写，绝不能把整份文件接在已下载的一半后面。
   - 测试：任务 3 `redirect_keeps_range_header`，任务 4 `resume_restarts_on_200`。
2. **加速服务返回错误网页，或者不给 `Content-Length`（分块传输），而实际字节比清单多。**
   - 期望：网页立即弃用该来源；没有长度时边收边数，一旦超过清单大小就弃用该来源，不会写出更大的文件。
   - 测试：任务 4 `html_source_is_dropped_immediately`、`body_longer_than_spec_drops_source`。
3. **下载中途应用被强制结束，`.part.json` 与 `.part` 不一致**：
   - 记录的字节数比 `.part` 实际长度多；
   - 只有 `.part`，没有 `.part.json`；
   - `.part` 比清单大小还大；
   - `.part.json` 里的哈希与当前清单不同（清单换了版本）。

   期望：
   - 第一种按两者中较小的值续传；
   - 其余三种都从 0 开始；
   - 不论哪种，都不会把不完整的文件装到正式文件名上。

   测试：任务 4 的 `meta_ahead_of_part_uses_part_length`、`part_without_meta_restarts_from_zero`、`part_longer_than_spec_restarts`、`meta_for_other_sha_restarts`。
4. **用户把已安装的模型换成同样大小的坏文件（或者文件被杀毒软件改写），哈希缓存不能把它当成仍然正确。**
   - 期望：修改时间一变就重新计算哈希，判为未安装。
   - 测试：任务 2 `cache_rehashes_when_mtime_changes`。
5. **“下载加速前缀”填了 `http://…`、漏掉结尾的 `/`、带空格，或者是 `javascript:` 一类的地址。**
   - 期望：界面提示格式要求，并且不发送；后端也再校验一次，返回 `invalid_prefix`。
   - 测试：任务 1 `valid_prefix_rules`，任务 7 `isValidMirrorPrefix`，任务 9 Playwright 用例。

---

## 文件结构

| 文件 | 职责 | 任务 |
|---|---|---|
| `src-tauri/models.json`（新） | 唯一清单 | 1 |
| `src-tauri/src/devocal/model/mod.rs`（新） | 子模块声明；Tauri 命令、`ModelRequest` | 1、6 |
| `src-tauri/src/devocal/model/manifest.rs`（新） | 清单类型、解析校验、`bundled()`、来源顺序、前缀校验 | 1 |
| `src-tauri/src/devocal/model/verify.rs`（新） | SHA-256、哈希缓存、已安装判定、目录、旧文件迁移 | 2 |
| `src-tauri/src/devocal/model/fetch.rs`（新） | `Fetcher`/`Body` trait、`ReqwestFetcher`、错误分类 | 3 |
| `src-tauri/src/devocal/model/download.rs`（新） | `.part` 续传、换源、校验、安装、`ModelError` | 4 |
| `src-tauri/src/devocal/model/state.rs`（新） | `ModelState`：任务、状态汇总、取消、删除、导入 | 5 |
| `src-tauri/src/devocal/model/fake.rs`（新，`cfg(test)`） | 脚本化假抓取器 | 4（任务 5 复用） |
| `src-tauri/src/devocal/mod.rs` | `model_path` 改为按清单校验；`apply` 接收模型路径 | 2 |
| `src-tauri/src/lib.rs` | 注册插件、状态、命令；启动时迁移 | 2、6 |
| `src-tauri/src/settings.rs` | `open_settings` 增加 `section` | 10 |
| `src-tauri/Cargo.toml` | 新依赖 | 3、6 |
| `scripts/model-manifest.mjs`（新） | Node 读取清单 | 1 |
| `scripts/fetch-stemgenrt.mjs` | 改读清单，使用新目录 | 1 |
| `scripts/model-manifest.test.ts`（新） | 清单、脚本与许可文本一致 | 1 |
| `licenses/StemgenRT-5.8.txt` | 地址改成清单的 `origin` | 1 |
| `src/modelDownload.ts` + `.test.ts`（新） | 前端类型、解析、文案、格式、同意记录、前缀 | 7 |
| `src/useModelDownload.ts`（新） | 轮询与发送命令 | 8 |
| `src/components/ModelSection.tsx` + `.css`（新） | 模型区块：各状态的行、前缀输入框 | 8、9 |
| `src/components/ModelConsentDialog.tsx`（新） | 确认框 | 9 |
| `src/components/DevocalSettings.tsx` | 挂上模型区块，处理定位请求 | 8、10 |
| `src/settingsSection.ts`（新） | 设置页定位请求（URL 参数 + 事件） | 10 |
| `src/openSettings.ts`（新） | 打开设置（含非 Tauri 预览），供设置按钮和提示共用 | 10 |
| `src/components/SettingsButton.tsx`、`PlayerControls.tsx`、`PlayerControls.css`、`src/devocal.ts` | 主窗口提示 | 10 |
| `src/SettingsPage.tsx` | 把定位请求传给 `DevocalSettings` | 10 |
| `tests/model-download.spec.ts`（新） | 模型区块的 Playwright 测试 | 8、9、10 |
| `tests/devocal.spec.ts`、`auto-apply-key-scale.spec.ts`、`autotune-control.spec.ts`、`colors.spec.ts`、`display.spec.ts` | 模拟后端补 `get_model_status` 等命令 | 8、10 |
| `.github/release-notes.md`、`README.md`、`docs/release-checklist.md`（新）、`scripts/check-model-mirrors.mjs`（新） | 文档与发版检查 | 11 |

---

### Task 1: 清单、Node 脚本与许可文本一致

**Files:**
- Create: `src-tauri/models.json`、`src-tauri/src/devocal/model/mod.rs`、`src-tauri/src/devocal/model/manifest.rs`、`scripts/model-manifest.mjs`、`scripts/model-manifest.test.ts`
- Modify: `src-tauri/src/devocal/mod.rs:11-13`（加 `pub mod model;`）、`scripts/fetch-stemgenrt.mjs`、`licenses/StemgenRT-5.8.txt:6-7`

**Interfaces:**
- Produces（`devocal::model::manifest`）：
  - `pub const STEMGENRT_ID: &str = "stemgenrt-hop128";`
  - `#[derive(Debug, Clone, Deserialize)] #[serde(rename_all = "camelCase")] pub struct Manifest { pub version: u32, pub mirrors: Vec<String>, pub models: Vec<ModelSpec> }`
  - `pub struct ModelSpec { pub id: String, pub name: String, pub tier: Tier, pub files: Vec<FileSpec>, pub sample_rate: u32, pub latency_ms: f64, pub runtime: Runtime, pub license: LicenseInfo, pub source: String }`
  - `pub struct FileSpec { pub file: String, pub bytes: u64, pub sha256: String, pub origin: String, pub mirrorable: bool }`
  - `pub enum Tier { Realtime, Quality }`、`pub enum Runtime { Cpu, Cuda }`：都是 serde lowercase。
  - `pub struct LicenseInfo { pub code: String, pub weights: String, pub training_data: Vec<String> }`
  - `impl Manifest { pub fn parse(json: &str) -> Result<Manifest, String>; pub fn model(&self, id: &str) -> Option<&ModelSpec> }`
  - `pub fn bundled() -> &'static Manifest`：`OnceLock`，解析 `include_str!("../../../models.json")`，非法时 panic 并带出错误信息。
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum SourceKind { Origin, Custom, Mirror(String /*host*/) }`
  - `impl SourceKind { pub fn label(&self) -> String }`：分别返回 `"origin"`、`"custom"`、`"mirror:<host>"`。
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub struct Candidate { pub kind: SourceKind, pub url: String }`
  - `pub fn candidates(file: &FileSpec, custom_prefix: Option<&str>, mirrors: &[String]) -> Vec<Candidate>`
  - `pub fn valid_prefix(prefix: &str) -> bool`
- Produces（Node）：`scripts/model-manifest.mjs` 导出：
  - `loadManifest(): Manifest`
  - `modelFile(id: string, file?: string): FileSpec`：`file` 缺省时取该模型的第一个文件。
  - `MANIFEST_PATH`

- [ ] **Step 1: 写 `src-tauri/models.json`**

内容逐字照抄规格 §3 的 JSON（含 `name`、`tier`、`sampleRate`、`latencyMs`、`runtime`、`license`、`source`）。

- [ ] **Step 2: 写失败的 Rust 测试（`manifest.rs` 的 `#[cfg(test)]`）**

```rust
#[test] fn bundled_manifest_is_valid_and_lists_stemgenrt() {
    let m = bundled();
    assert_eq!(m.version, 1);
    let s = m.model(STEMGENRT_ID).unwrap();
    assert_eq!(s.files.len(), 1);
    let f = &s.files[0];
    assert_eq!((f.file.as_str(), f.bytes), ("model.onnx", 37_529_132));
    assert_eq!(f.sha256, "77164d6a581fafb2a31f53fd8ffde44c07cf618472952a4cdba14e68dda3b8b9");
    assert!(f.origin.starts_with("https://github.com/sweetspotsoundsystem/stemgen-rt/raw/61df8f4aa1555ef110308d01ea92b54ace770979/"));
    assert_eq!(m.mirrors, ["https://ghfast.top/", "https://ghproxy.net/", "https://ghproxy.vip/"]);
}
#[test] fn parse_rejects_unsafe_or_malformed_entries() {
    // 每种都从一个合法 JSON 改一处，断言 parse 返回 Err：
    // version 2；重复 id；id 含大写或 "/"；file 为 "../x.onnx"、"a/b"、"a\\b"、""；
    // sha256 长度不是 64 或含大写/非十六进制；origin 为 http://；
    // mirrorable=true 但 origin 不以 "https://github.com/" 开头；
    // 镜像不以 https:// 开头或不以 "/" 结尾；models 为空；某模型 files 为空。
}
#[test] fn candidates_follow_origin_custom_mirrors_order() {
    let f = /* mirrorable 的 FileSpec，origin = "https://github.com/a/b/raw/c/m.onnx" */;
    let c = candidates(&f, Some("https://my.proxy/"), &["https://ghfast.top/".into(), "https://ghproxy.net/".into()]);
    assert_eq!(c.iter().map(|c| (c.kind.label(), c.url.clone())).collect::<Vec<_>>(), vec![
        ("origin".into(), "https://github.com/a/b/raw/c/m.onnx".into()),
        ("custom".into(), "https://my.proxy/https://github.com/a/b/raw/c/m.onnx".into()),
        ("mirror:ghfast.top".into(), "https://ghfast.top/https://github.com/a/b/raw/c/m.onnx".into()),
        ("mirror:ghproxy.net".into(), "https://ghproxy.net/https://github.com/a/b/raw/c/m.onnx".into()),
    ]);
}
#[test] fn non_mirrorable_files_only_use_origin() { /* mirrorable=false 时，即使有自定义前缀和镜像，也只返回 origin 一项 */ }
#[test] fn valid_prefix_rules() {
    for ok in ["https://ghfast.top/", "https://a.b/c/"] { assert!(valid_prefix(ok), "{ok}"); }
    for bad in ["", "http://a.b/", "https://a.b", "https:///", "https://a b/", " https://a.b/", "javascript:alert(1)/", "https://a.b/\n"] {
        assert!(!valid_prefix(bad), "{bad:?}");
    }
}
```

`valid_prefix` 的规则：
- 以 `https://` 开头，以 `/` 结尾；
- 主机部分非空；
- 不含空白和控制字符；
- 总长不超过 200。

- [ ] **Step 3: 运行，确认失败**

运行：`cargo test --manifest-path src-tauri/Cargo.toml devocal::model::manifest`
预期：编译失败（类型未定义）。

- [ ] **Step 4: 实现 `manifest.rs`，并在 `model/mod.rs` 中加 `pub mod manifest;`**

- 校验写在 `parse` 里。
- `id` 只允许 `[a-z0-9-]+`。
- `file` 必须是单个路径段：不含 `/`、`\`、`..`，不能为空，也不能是 `.`。
- 镜像的 host 取 `https://` 之后到下一个 `/` 之间的部分。

- [ ] **Step 5: 写失败的 vitest 测试 `scripts/model-manifest.test.ts`**

```ts
import { readFileSync } from "node:fs";
import { loadManifest, modelFile } from "./model-manifest.mjs";
it("the StemgenRT licence text names the manifest's origin, size and SHA-256", () => {
  const f = modelFile("stemgenrt-hop128");
  const text = readFileSync(new URL("../licenses/StemgenRT-5.8.txt", import.meta.url), "utf8");
  expect(text).toContain(f.origin);
  expect(text).toContain(f.bytes.toLocaleString("en-US") + " bytes");
  expect(text).toContain(`SHA-256 ${f.sha256}`);
});
it("fetch-stemgenrt.mjs reads the manifest and hard-codes no hash or URL", () => {
  const src = readFileSync(new URL("./fetch-stemgenrt.mjs", import.meta.url), "utf8");
  expect(src).toContain("model-manifest.mjs");
  expect(src).not.toMatch(/[0-9a-f]{64}/);
  expect(src).not.toContain("githubusercontent");
});
it("the manifest lists the three measured mirrors", () => {
  expect(loadManifest().mirrors).toEqual(["https://ghfast.top/", "https://ghproxy.net/", "https://ghproxy.vip/"]);
});
```

- [ ] **Step 6: 运行 `npx vitest run scripts/model-manifest.test.ts`，确认失败**

- [ ] **Step 7: 实现 Node 侧**

- **`scripts/model-manifest.mjs`**：`readFileSync` 读 `new URL("../src-tauri/models.json", import.meta.url)`，再 `JSON.parse`。
- **`scripts/fetch-stemgenrt.mjs`**：
  - 改为 `const MODEL = modelFile("stemgenrt-hop128")`，用 `MODEL.origin` 作为下载地址；
  - 默认目标改为 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\stemgenrt-hop128\model.onnx`；文件头注释同步修改；
  - 其余逻辑不变（`--accept`、校验、改名）。
- **`licenses/StemgenRT-5.8.txt` 第 7 行**：地址换成清单的 `origin`。第 8 行（大小与哈希）保持原样，它已经与测试期望的格式一致。

- [ ] **Step 8: 运行两组测试，确认通过**

运行：
- `npx vitest run scripts/model-manifest.test.ts`
- `cargo test --manifest-path src-tauri/Cargo.toml devocal::model::manifest`

预期：全部 PASS。

- [ ] **Step 9: 提交**

先确认 `git -C "E:\autotune helper\now-playing" branch --show-current` 输出 `model-download`，然后：

```bash
git add src-tauri/models.json src-tauri/src/devocal/model src-tauri/src/devocal/mod.rs scripts/model-manifest.mjs scripts/model-manifest.test.ts scripts/fetch-stemgenrt.mjs licenses/StemgenRT-5.8.txt
git commit -m "feat(model): single multi-model manifest shared by app, script and licence"
```

---

### Task 2: 校验、哈希缓存、目录与旧文件迁移；去人声改用已校验的模型

**Files:**
- Create: `src-tauri/src/devocal/model/verify.rs`
- Modify: `src-tauri/src/devocal/model/mod.rs`（加 `pub mod verify;`）、`src-tauri/src/devocal/mod.rs:30-53,178-217,370-495`、`src-tauri/src/lib.rs:36-42`
- Test: 写在 `verify.rs` 和 `devocal/mod.rs` 的测试模块里

**Interfaces:**
- Consumes：任务 1 的 `Manifest`、`FileSpec`、`STEMGENRT_ID`、`bundled()`。
- Produces（`devocal::model::verify`）：
  - `pub fn sha256_file(path: &Path) -> std::io::Result<String>`：返回小写十六进制，用 `format!("{:x}", hasher.finalize())`。
  - `pub fn file_verified(path: &Path, spec: &FileSpec) -> bool`：先比大小，再查缓存或计算哈希。
  - `pub fn note_verified(path: &Path, sha256: &str)`：安装后写入缓存，避免再算一次。
  - `pub fn model_dir(models_dir: &Path, id: &str) -> PathBuf`
  - `pub fn installed_files(manifest: &Manifest, models_dir: &Path, id: &str) -> Option<Vec<PathBuf>>`：所有文件都通过校验才返回 `Some`，顺序与清单一致。
  - `pub const LEGACY_FLAT: &[(&str, &str)] = &[("stemgenrt-hop128", "stemgenrt-hop128.onnx")];`
  - `pub fn migrate_legacy(manifest: &Manifest, models_dir: &Path) -> Vec<String>`：返回迁移成功的 id。
- Produces（`devocal`）：
  - `pub fn model_path_from(env: Option<OsString>, data_dir: &Path, manifest: &Manifest) -> Option<PathBuf>`
  - `pub fn model_path(data_dir: &Path) -> Option<PathBuf>`：内部使用 `bundled()`。
  - `fn apply<L>(sup: &mut Supervisor<L>, action: &str, model: Option<PathBuf>) -> Result<(), String>`
  - 删除 `MODEL_FILE`。

- [ ] **Step 1: 写失败的测试（`verify.rs`）**

测试用一个只有几 KB 内容的小清单（用 `Manifest::parse` 构造，哈希用 `sha2` 现算），目录用 `crate::devocal::tests::temp_dir`。

```rust
#[test] fn installed_requires_size_and_hash() {
    // 文件缺失 → None；大小不对 → None；大小对、内容错一字节 → None；正确 → Some(vec![dir/<id>/<file>])
}
#[test] fn cache_rehashes_when_mtime_changes() {
    // 写正确内容 → installed_files 为 Some；
    // 改写成同样长度的错误内容，并用 File::set_modified 把修改时间 +2 s → 变为 None
}
#[test] fn migrate_moves_a_verified_flat_file_into_the_model_dir() {
    // models/stemgenrt-hop128.onnx（内容匹配假清单中该 id 的唯一文件）
    // → 返回 vec!["stemgenrt-hop128"]，旧文件消失，models/stemgenrt-hop128/model.onnx 存在
}
#[test] fn migrate_leaves_bad_or_conflicting_files_alone() {
    // 内容不符：旧文件原地保留，新目录下没有文件；
    // 目标已存在：两个文件都不动，返回空
}
```

- [ ] **Step 2: 修改 `devocal/mod.rs` 的现有测试，使其先失败**

`model_path_prefers_an_existing_env_file_then_the_data_dir` 改为：
- 传入上面的假清单；
- 数据目录里只放 `b"x"` 时返回 `None`（校验不过）；
- 放入正确内容的 `models/stemgenrt-hop128/model.onnx` 后返回它；
- 环境变量指向的文件存在时优先返回它，且不校验；
- 环境变量为空字符串时被忽略。

`actions_map_to_supervisor_calls` 改为调用 `apply(&mut sup, "enable", None)`，之后再调用 `apply(&mut sup, "enable", Some(path))`。断言保持不变。

- [ ] **Step 3: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::`，确认失败**

- [ ] **Step 4: 实现**

- **`verify.rs`**：
  - 缓存为 `static CACHE: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, u64, String)>>>`；
  - `file_verified` 在大小不符时直接返回 false，不计算哈希。
- **`devocal/mod.rs`**：
  - `DevocalState::command` 在锁住 supervisor **之前**算出 `model`：只在 `action == "enable"` 时调用 `model_path`；
  - 原因：首次计算哈希约 0.1～0.2 s，不能卡住 100 ms 一次的 tick。
- **`lib.rs` 的 setup**：在 `restore_at_startup` 之后、`devocal.start` 之前调用 `migrate_legacy(bundled(), &dir.join("models"))`；只在旧扁平文件存在时才会计算哈希；迁移结果打一行 `eprintln!` 日志。

- [ ] **Step 5: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::`，确认全部 PASS**

- [ ] **Step 6: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/src/devocal src-tauri/src/lib.rs
git commit -m "feat(model): verified per-model layout with legacy flat-file migration"
```

---

### Task 3: HTTP 抓取层（reqwest）与本机假服务器测试

**Files:**
- Create: `src-tauri/src/devocal/model/fetch.rs`
- Modify: `src-tauri/Cargo.toml`（`[dependencies]` 段，紧接在 `tauri-plugin-single-instance` 一行之后）、`src-tauri/src/devocal/model/mod.rs`

**Interfaces:**
- Produces：
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum FetchError { Connect, Timeout, Status(u16), BadRange, Network(String) }`
  - `impl FetchError { pub fn retryable(&self) -> bool }`：`Connect`、`Timeout`、`Network`、`Status(429)`、`Status(500..=599)` 返回 true。
  - `pub struct Opened<B> { pub status: u16 /*200|206*/, pub total: Option<u64>, pub html: bool, pub body: B }`
    - `status` 为 206 时，`total` 取 `Content-Range` 中的总大小；为 200 时取 `Content-Length`；
    - `html` 取 `content-type` 是否以 `text/html` 开头。
  - `pub trait Body: Send { fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, FetchError>> + Send; }`
  - `pub trait Fetcher: Send + Sync + 'static { type Body: Body; fn open(&self, url: &str, offset: u64) -> impl Future<Output = Result<Opened<Self::Body>, FetchError>> + Send; }`
  - `pub struct ReqwestFetcher`
    - `ReqwestFetcher::new(connect_timeout: Duration, read_timeout: Duration) -> Result<Self, String>`
    - `ReqwestFetcher::standard()`：10 s / 30 s。
  - 约定：
    - `open` 在 `offset > 0` 时带 `Range: bytes=<offset>-`；
    - 200 和 206 以外的状态返回 `Err(Status(n))`；
    - 206 的起点 ≠ `offset` 时返回 `Err(BadRange)`。

- [ ] **Step 1: 添加依赖**

在 `src-tauri/Cargo.toml` 的 `[dependencies]` 中，`tauri-plugin-single-instance = "2.5"` 之后加：

```toml
# In-app model download: Windows schannel TLS and the system proxy (no rustls/aws-lc toolchain).
reqwest = { version = "0.13", default-features = false, features = ["native-tls", "system-proxy"] }
sha2 = "0.10"
```

已对照本机 `reqwest-0.13.5` 的 `Cargo.toml` 核实：
- `default` 里包含 `default-tls`（即 rustls + aws-lc）、`charset`、`http2`、`system-proxy`；
- 关掉默认 feature 后，必须显式写上 `system-proxy`，才会跟随 Windows 系统代理；
- `native-tls` 是正确的 feature 名；
- `Response::chunk()` 不需要 `stream` feature，所以不加 `stream`（与规格字面不同，行为一样，见报告中的待决问题）。

`sha2 0.10.9` 已经在 `Cargo.lock` 中。

- [ ] **Step 2: 写失败的测试**

测试用 `std::net::TcpListener` 在 `127.0.0.1:0` 上起一个线程做假服务器，按脚本返回原始 HTTP/1.1 响应，并记录每个请求的头。用 `#[tokio::test]`。

```rust
#[tokio::test] async fn full_get_reports_200_total_and_streams_all_bytes() { /* Content-Length=N → status 200, total Some(N)，拼起来的分块等于原数据 */ }
#[tokio::test] async fn range_get_parses_content_range_total() { /* open(url, 10) → 请求带 "Range: bytes=10-"；206 + "Content-Range: bytes 10-99/100" → total Some(100) */ }
#[tokio::test] async fn range_start_mismatch_is_bad_range() { /* 回 "bytes 0-99/100" 给 offset 10 → Err(BadRange) */ }
#[tokio::test] async fn redirect_keeps_range_header() { /* /a 302 → /b；/b 收到的请求仍带 Range: bytes=10- */ }
#[tokio::test] async fn html_content_type_is_flagged() { /* text/html; charset=utf-8 → html == true */ }
#[tokio::test] async fn error_statuses_map_to_status() { /* 404 → Err(Status(404))；503 → Err(Status(503))，且 retryable() 为 true；403 的 retryable() 为 false */ }
#[tokio::test] async fn stalled_body_times_out() { /* new(1 s, 300 ms)；服务器发完头后停住 → next_chunk 返回 Err(Timeout) */ }
#[tokio::test] async fn refused_connection_is_connect() { /* 先 bind 再 drop 得到一个关闭的端口 → Err(Connect) */ }
```

- [ ] **Step 3: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::model::fetch`，确认失败**

- [ ] **Step 4: 实现 `fetch.rs`**

- `reqwest::Client::builder().tls_backend_native().connect_timeout(..).read_timeout(..).user_agent(concat!("TuneLove/", env!("CARGO_PKG_VERSION")))`，重定向用默认策略。
  - 必须显式调用 `.tls_backend_native()`（0.13.5 的方法名）：`tauri` 也传递依赖 reqwest，如果 feature 合并后带进了 rustls，默认 TLS 后端可能不是 schannel。
- 错误分类：
  - `is_connect()` → `Connect`；
  - `is_timeout()` → `Timeout`；
  - 其余 → `Network(e.to_string())`。
- body 用 `Response::chunk()` 读取。

- [ ] **Step 5: 运行，确认 PASS；再运行 `cargo build --manifest-path src-tauri/Cargo.toml`，确认没有引入 rustls 或 aws-lc**

用 `cargo tree --manifest-path src-tauri/Cargo.toml -i aws-lc-rs` 检查：预期找不到这个包，或者它只经由 `tauri` 传递引入（与本次改动之前的结果一致）。

- [ ] **Step 6: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/devocal/model
git commit -m "feat(model): reqwest fetcher with range, redirect and timeout handling"
```

---

### Task 4: 下载核心——续传、换源、校验、原子安装

**Files:**
- Create: `src-tauri/src/devocal/model/download.rs`、`src-tauri/src/devocal/model/fake.rs`（`#[cfg(test)]`）
- Modify: `src-tauri/src/devocal/model/mod.rs`

**Interfaces:**
- Consumes：任务 1 的 `FileSpec`、`Candidate`、`SourceKind`；任务 2 的 `note_verified`；任务 3 的 `Fetcher`、`Body`、`Opened`、`FetchError`。
- Produces：
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum ModelError { Network, Timeout, HttpStatus(u16), SourceHtml, SizeMismatch, Sha256Mismatch, DiskFull, WriteFailed, InstallDenied, Cancelled, AlreadyRunning, AllSourcesFailed, InvalidPrefix, UnknownModel, DeleteDenied, ImportMismatch, ImportUnsupported }`
  - `impl ModelError { pub fn code(&self) -> String }`：输出 Global Constraints 中的机器码。`Network` 对应 `network_unreachable`，`HttpStatus(n)` 对应 `http_status:<n>`。
  - `pub struct RetryPolicy { pub retries: u32, pub backoff: Duration }`；`RetryPolicy::standard()` = `{ retries: 2, backoff: 1 s }`。第 k 次重试前等待 `backoff * k`。
  - `#[derive(Debug, Clone, PartialEq)] pub enum Update { Verifying, Downloading { received: u64, source: SourceKind } }`：`received` 指当前文件已写入的字节数。
  - `pub fn part_path(final_path: &Path) -> PathBuf`（`<file>.part`）、`pub fn meta_path(final_path: &Path) -> PathBuf`（`<file>.part.json`）
  - `#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")] pub struct PartMeta { pub source: String, pub url: String, pub written: u64, pub total: u64, pub sha256: String }`
  - `pub async fn download_file<F: Fetcher>(fetcher: &F, file: &FileSpec, final_path: &Path, candidates: &[Candidate], policy: &RetryPolicy, cancel: &AtomicBool, progress: &(dyn Fn(Update) + Send + Sync)) -> Result<(), ModelError>`：成功时正式文件已经就位，`.part` 和 `.part.json` 都已删除。
  - `pub fn install(part: &Path, final_path: &Path, sha256: &str) -> Result<(), ModelError>`：依次做 `sync_all`、`rename`、`note_verified`。`rename` 出现 `PermissionDenied` 时返回 `InstallDenied`。
  - `pub fn io_error(e: &std::io::Error) -> ModelError`：`StorageFull` 返回 `DiskFull`，其余返回 `WriteFailed`。
- Produces（`fake.rs`，供任务 5 复用）：
  - `pub enum FakeReply { Fail(FetchError), Serve { status: u16, total: Option<u64>, html: bool, chunks: Vec<Vec<u8>>, then: Option<FetchError> } }`
  - `pub struct FakeFetcher`
    - `FakeFetcher::new()`
    - `.script(url, Vec<FakeReply>)`：同一 URL 按顺序消费，耗尽后返回 `Fail(Connect)`；
    - `.calls() -> Vec<(String, u64)>`：记录每次 open 的 URL 和起点；
    - `.gate()`：返回一个 `tokio::sync::Notify`，设置后，下一次 `open` 会等它被 `notify_one`。供任务 5 制造“正在下载”的状态。
  - 辅助函数 `pub fn spec_for(bytes: &[u8], origin: &str, mirrorable: bool) -> FileSpec`。

**算法要点**（测试决定不了的部分）：
1. **开始**：
   - 读取 `.part.json` 和 `.part`。只有 `meta.total == file.bytes`、`meta.sha256 == file.sha256`、并且 `.part` 长度 ≤ `file.bytes` 时才续传，起点取 `min(meta.written, .part 长度)`；
   - 否则删除两个文件，从 0 开始；
   - 续传时先报 `Update::Verifying`，把已有部分读一遍，喂给哈希器。
   - 起点等于 `file.bytes` 时不联网，直接进入最后的校验。
2. **对每个来源**：最多尝试 `1 + retries` 次。
   - `open` 返回错误：
     - 可重试的错误：等待后重试；
     - 不可重试的错误（`Status` 4xx 非 429、`BadRange`）：换下一个来源。
   - `open` 成功之后：
     - `html` → 换下一个来源，记下 `SourceHtml`；
     - `206` 且 `total != Some(bytes)` → 换下一个来源；
     - `200`：如果 `total` 是 `Some` 且不等于 `bytes`，换下一个来源；否则截断 `.part`、重置哈希器、起点归 0。
   - 逐块写入、更新哈希、报 `Downloading`：
     - 每块写完检查取消标志；
     - 每累计 1 MiB 先 `flush`，再重写 `.part.json`；
     - 写入后累计超过 `bytes` → 截断到 0，换下一个来源。
   - body 中途出错，或提前结束且不足 `bytes`：按可重试错误处理，下一次从已写入的位置续传。
3. **写满 `bytes` 后**：报 `Verifying`，比较哈希。
   - 不符：删除 `.part` 和 `.part.json`；如果是第一次不符，回到第一个来源从 0 重新完整下载；第二次不符返回 `Sha256Mismatch`。
   - 相符：调用 `install`，再删除 `.part.json`。
4. **所有来源都失败**：返回 `AllSourcesFailed`。
5. **取消**：`flush`、写好 `.part.json`，然后返回 `Cancelled`。
6. **本地写入错误**（`.part` 的 open/write/flush）：立即通过 `io_error` 返回，不再换来源。

- [ ] **Step 1: 写失败的测试（`download.rs` 的测试模块）**

测试数据为 64 KiB 伪随机字节，每块 4 KiB；`RetryPolicy { retries: 2, backoff: Duration::ZERO }`。

```rust
#[tokio::test] async fn downloads_from_origin_and_installs_atomically()      // 正式文件内容正确；没有 .part/.part.json；calls == [(origin, 0)]
#[tokio::test] async fn retryable_errors_retry_twice_per_source_then_move_on() // origin 三次 Fail(Connect) → calls 依次为 origin×3，再到 mirror
#[tokio::test] async fn non_retryable_status_moves_on_without_retry()       // origin Fail(Status(404)) → origin 只出现 1 次
#[tokio::test] async fn html_source_is_dropped_immediately()                // mirror1 Serve{html:true} 只调用 1 次，接着 mirror2 成功
#[tokio::test] async fn wrong_total_drops_source()                          // 200 且 total = bytes+1 → 换源，不重试
#[tokio::test] async fn body_longer_than_spec_drops_source()                // total None，分块超出 bytes → 换源；正式文件正确，来自下一个来源
#[tokio::test] async fn mid_body_failure_resumes_from_written_offset()      // 先 Serve 一半 + then Some(Network)，再 206 → calls[1] == (origin, 一半)
#[tokio::test] async fn resume_appends_on_206_with_matching_total()        // 预置一半 .part 和 meta → calls == [(origin, 一半)]，正式文件正确
#[tokio::test] async fn resume_restarts_on_200()                           // 预置一半，服务器回 200 和整份 → 内容正确（不是一半+整份），长度为 bytes
#[tokio::test] async fn resume_206_with_wrong_total_drops_source()
#[tokio::test] async fn meta_ahead_of_part_uses_part_length()              // meta.written = 40 KiB，.part 只有 32 KiB → 起点 32 KiB
#[tokio::test] async fn part_without_meta_restarts_from_zero()
#[tokio::test] async fn part_longer_than_spec_restarts()
#[tokio::test] async fn meta_for_other_sha_restarts()
#[tokio::test] async fn complete_part_is_installed_without_network()        // .part 已满且 meta 有效 → calls 为空，安装成功
#[tokio::test] async fn sha_mismatch_redownloads_once_from_the_first_source() // 第一次内容错一字节、第二次正确 → Ok；calls == [(origin,0),(origin,0)]
#[tokio::test] async fn second_sha_mismatch_fails_and_leaves_nothing()      // 两次都错 → Err(Sha256Mismatch)；三个文件都不存在
#[tokio::test] async fn all_sources_failed_when_every_source_fails()        // → Err(AllSourcesFailed)
#[tokio::test] async fn cancel_keeps_part_and_meta()                        // progress 回调在首个 Downloading 时置位取消 → Err(Cancelled)；.part 长度 == meta.written > 0
#[tokio::test] async fn progress_reports_source_kind()                      // 镜像下载时 Update::Downloading 的 source == Mirror("ghfast.top")
#[cfg(windows)] #[test] fn install_reports_install_denied_when_target_locked() // 用 share_mode(0) 打开正式文件后调用 install → Err(InstallDenied)；.part 仍在
#[test] fn codes_match_the_contract()                                       // 逐个断言 code()，例如 HttpStatus(429) → "http_status:429"
```

- [ ] **Step 2: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::model::download`，确认失败**

- [ ] **Step 3: 实现 `fake.rs` 和 `download.rs`**

按上面的算法要点实现。文件 I/O 用 `std::fs`（单次阻塞最多是一个分块写入，或一次不超过 40 MB 的续传前重算哈希，可以接受）；等待用 `tokio::time::sleep`。

- [ ] **Step 4: 运行，确认全部 PASS**

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/src/devocal/model
git commit -m "feat(model): resumable multi-source download with SHA-256 gate and atomic install"
```

---

### Task 5: `ModelState`——任务、状态汇总、取消、删除、导入

**Files:**
- Create: `src-tauri/src/devocal/model/state.rs`
- Modify: `src-tauri/src/devocal/model/mod.rs`

**Interfaces:**
- Consumes：任务 1～4 的全部公开项。
- Produces：
  - `#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)] #[serde(rename_all = "lowercase")] pub enum ModelPhase { Missing, Downloading, Verifying, Installed, Failed }`
  - `#[derive(Serialize, Clone, Debug, PartialEq)] #[serde(rename_all = "camelCase")] pub struct ModelStatus { pub id: String, pub phase: ModelPhase, pub received_bytes: u64, pub total_bytes: u64, pub source: Option<String>, pub path: Option<String>, pub error: Option<String> }`
  - `#[derive(Clone)] pub struct ModelState`
    - `ModelState::new(manifest: &'static Manifest, env_override: Option<PathBuf>) -> Self`：`env_override` 只用于 `STEMGENRT_ID`。
    - `pub fn set_models_dir(&self, dir: PathBuf)`
    - `pub fn statuses(&self) -> Vec<ModelStatus>`
    - `pub fn begin_download<F: Fetcher>(&self, fetcher: Arc<F>, id: &str, custom_prefix: Option<&str>, policy: RetryPolicy, break_origin: bool, on_installed: Box<dyn FnOnce() + Send>) -> Result<impl Future<Output = ()> + Send + 'static, ModelError>`
      - `break_origin` 为 true 时，把候选列表中 `Origin` 的 URL 换成 `https://127.0.0.1:9/blocked`。它只供任务 6 的调试开关使用；测试中除 `break_origin_falls_through_to_mirror` 外一律传 `false`。
    - `pub fn begin_import(&self, id: &str, source_file: PathBuf, on_installed: Box<dyn FnOnce() + Send>) -> Result<impl Future<Output = ()> + Send + 'static, ModelError>`
    - `pub fn cancel(&self, id: &str)`：没有在运行就什么也不做。
    - `pub fn delete(&self, id: &str) -> Result<(), ModelError>`
  - 测试用的 `manifest` 参数是 `&'static`：测试里用 `Box::leak` 得到。

**规则**：
- **登记任务**：`begin_*` 在返回 future 之前就同步登记“运行中”。
  - 已有任务（任何 id）在运行 → `AlreadyRunning`；
  - id 不在清单 → `UnknownModel`；
  - 没有设置 `models_dir` → `WriteFailed`；
  - 已安装 → 返回一个立即完成的 future。
- **future 的执行**：按清单顺序逐个文件调用 `download_file`，候选来源由 `candidates()` 生成。
  - 结束时清除运行标记；
  - 成功：记下 `source`（最后一个文件所用来源的 label），清除错误，调用 `on_installed`；
  - `Cancelled`：不记录错误；
  - 其他失败：记录 `error = code()`，不调用 `on_installed`。
- **`statuses()`**：每个清单模型一项，按下面的顺序判断，取第一个成立的：
  1. 正在运行 → 实时的 phase（`Downloading`/`Verifying`）；`received` = 已完成文件的字节数 + 当前文件已写入的字节数；`source` 为当前来源的 label；
  2. `id == STEMGENRT_ID` 且 `env_override` 指向的文件存在 → `Installed`，`source: "env"`，`path` 为该文件；
  3. `installed_files` 为 `Some` → `Installed`，`path` 为第一个文件，`source` 为本次会话记录的来源（没有就是 `None`）；
  4. 有记录的错误 → `Failed`，`error` 为机器码，`received` 为各 `.part` 长度之和；
  5. 其他 → `Missing`，`received` 同上。

  各种情况下 `total_bytes` 都是清单中各文件大小之和。
- **导入**（只支持单文件模型；多文件返回 `ImportUnsupported`）：
  - 先比较源文件大小，不符 → `ImportMismatch`；
  - 再边复制到 `.part` 边算哈希，状态为 `Verifying`；
  - 哈希不符：删除 `.part`，记 `ImportMismatch`；
  - 相符：调用 `install`，`source` 记为 `"import"`。
  - 导入会覆盖同一模型已有的 `.part` 和 `.part.json`，先删除它们。
- **删除**：
  - 该 id 正在运行 → `AlreadyRunning`；
  - 只删除该模型目录下清单列出的文件名，以及对应的 `.part`、`.part.json`；其他文件不动；目录为空时删除目录；
  - `PermissionDenied` → `DeleteDenied`；
  - 同时清除记录的错误和来源。

- [ ] **Step 1: 写失败的测试（`state.rs`）**

使用 `fake::FakeFetcher` 和 `Box::leak` 得到的两模型假清单：`a` 有 1 个文件，`b` 有 2 个文件且 `mirrorable=false`。

```rust
#[tokio::test] async fn statuses_list_every_manifest_model_in_order()       // ids == ["a","b"]；都为 Missing；total_bytes 为各文件之和
#[tokio::test] async fn download_installs_and_calls_on_installed_once()    // 等待 future → a 为 Installed，source Some("origin")，path 以 "a/<file>" 结尾；回调计数 1
#[tokio::test] async fn failure_records_code_and_skips_on_installed()      // 所有来源失败 → Failed，error Some("all_sources_failed")；回调计数 0
#[tokio::test] async fn retry_clears_the_previous_error()                  // 失败后再次 begin_download 成功 → error None
#[tokio::test] async fn second_task_is_already_running()                   // 用 gate 卡住 a；begin_download("b") 和 begin_import("a") 都返回 Err(AlreadyRunning)；此时 a 为 Downloading
#[tokio::test] async fn cancel_returns_to_missing_with_partial_bytes()     // 卡在第二块之后 cancel → Missing，received_bytes > 0，error None
#[tokio::test] async fn multi_file_model_installs_all_files()              // b：两个文件都在，状态为 Installed
#[tokio::test] async fn break_origin_falls_through_to_mirror()              // break_origin=true → FakeFetcher.calls 中第一项 URL 为 "https://127.0.0.1:9/blocked"，最终来源为镜像
#[test] fn delete_removes_only_manifest_files_and_parts()                  // 目录里另放一个 notes.txt → 删除后它仍在
#[tokio::test] async fn delete_while_running_is_already_running()
#[test] fn env_override_reports_installed_from_env_for_stemgenrt_only()    // 用包含 stemgenrt-hop128 的假清单：source "env"；其他 id 不受影响
#[tokio::test] async fn import_verifies_then_installs_with_import_source()
#[tokio::test] async fn import_mismatch_leaves_no_part_and_reports_import_mismatch()
#[test] fn import_of_multi_file_model_is_unsupported()
#[test] fn unknown_id_is_unknown_model()
#[test] fn status_serialises_camel_case() {
    let v = serde_json::to_value(ModelStatus { id: "a".into(), phase: ModelPhase::Downloading, received_bytes: 1, total_bytes: 2,
        source: Some("mirror:ghfast.top".into()), path: None, error: None }).unwrap();
    assert_eq!(v, serde_json::json!({ "id": "a", "phase": "downloading", "receivedBytes": 1, "totalBytes": 2,
        "source": "mirror:ghfast.top", "path": null, "error": null }));
}
```

- [ ] **Step 2: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::model::state`，确认失败**

- [ ] **Step 3: 实现 `state.rs`**

内部用 `Arc<Inner>`，`Inner` 包含：
- `Mutex<Option<PathBuf>>`：models_dir；
- `Mutex<Option<Running { id, cancel: Arc<AtomicBool>, phase, received, source }>>`；
- `Mutex<HashMap<String, String>>`：错误；
- `Mutex<HashMap<String, String>>`：来源。

`lock` 沿用 `devocal/mod.rs` 中“中毒仍取内部值”的写法。

- [ ] **Step 4: 运行，确认 PASS；再跑一遍 `cargo test --manifest-path src-tauri/Cargo.toml devocal::`，确认全部 PASS**

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/src/devocal/model
git commit -m "feat(model): app-level model state with per-model status, cancel, delete and import"
```

---

### Task 6: Tauri 接线——命令、文件对话框、自动开启

**Files:**
- Modify: `src-tauri/Cargo.toml`（在任务 3 加的 `sha2` 一行之后）、`src-tauri/src/devocal/model/mod.rs`、`src-tauri/src/lib.rs`

**Interfaces:**
- Consumes：任务 5 的 `ModelState`、`ModelStatus`；任务 3 的 `ReqwestFetcher::standard()`；`DevocalState::command`。
- Produces（`devocal::model`）：
  - `#[derive(Debug, Clone, Deserialize)] #[serde(rename_all = "camelCase")] pub struct ModelRequest { pub id: String, pub action: String, #[serde(default)] pub auto_enable: bool, #[serde(default)] pub mirror_prefix: Option<String> }`
  - `pub fn validate_request(req: &ModelRequest, manifest: &Manifest) -> Result<(), ModelError>`：
    - id 不在清单 → `UnknownModel`；
    - 未知 action → `UnknownModel`；
    - `mirror_prefix` 是 `Some` 且非空，但 `!valid_prefix` → `InvalidPrefix`。
  - `pub fn wants_auto_enable(req: &ModelRequest) -> bool`：`req.auto_enable && req.id == STEMGENRT_ID`。
  - `#[tauri::command] pub async fn get_model_status(state: State<'_, ModelState>) -> Result<Vec<ModelStatus>, String>`：在 `spawn_blocking` 中调用 `statuses()`，因为首次查询要算哈希。
  - `#[tauri::command] pub async fn model_command(app: AppHandle, request: ModelRequest, state: State<'_, ModelState>) -> Result<Vec<ModelStatus>, String>`：错误返回 `ModelError::code()`。
- 前端调用方式：`invoke("model_command", { request: { id, action, autoEnable, mirrorPrefix } })`。

- [ ] **Step 1: 添加依赖**

在 `src-tauri/Cargo.toml` 的 `[dependencies]` 中，`sha2 = "0.10"` 之后加：

```toml
# "Import a local model file": native file picker, called from Rust only (no JS permission needed).
tauri-plugin-dialog = "2"
```

本机缓存的是 2.7.3，实际版本以锁文件为准。它会顺带引入 `tauri-plugin-fs` 和 `rfd`。

- [ ] **Step 2: 写失败的测试（`model/mod.rs` 的测试模块）**

```rust
#[test] fn request_parses_camel_case_with_defaults() {
    let r: ModelRequest = serde_json::from_value(serde_json::json!({ "id": "stemgenrt-hop128", "action": "download" })).unwrap();
    assert!(!r.auto_enable && r.mirror_prefix.is_none());
    let r: ModelRequest = serde_json::from_value(serde_json::json!({ "id": "x", "action": "import", "autoEnable": true, "mirrorPrefix": "https://p/" })).unwrap();
    assert!(r.auto_enable); assert_eq!(r.mirror_prefix.as_deref(), Some("https://p/"));
}
#[test] fn validate_request_rejects_unknown_ids_actions_and_bad_prefixes() { /* 分别断言 UnknownModel、UnknownModel、InvalidPrefix；空字符串前缀视为未填 → Ok */ }
#[test] fn auto_enable_only_for_stemgenrt() { /* stemgenrt + true → true；其他 id + true → false；stemgenrt + false → false */ }
```

- [ ] **Step 3: 运行 `cargo test --manifest-path src-tauri/Cargo.toml devocal::model::tests`，确认失败**

- [ ] **Step 4: 实现命令并接到 `lib.rs`**

- **`model_command` 的分支**：
  - **`download`**：
    - 抓取器：`ReqwestFetcher::standard()`，存在 `OnceLock<Arc<_>>` 里；创建失败返回 `network_unreachable`。
    - 自定义前缀：空串视为 `None`。
    - `on_installed`：仅当 `wants_auto_enable` 为 true 时，才在 `spawn_blocking` 中调用 `app.state::<DevocalState>().command("enable")`，失败只打 `eprintln!` 日志；否则传一个空闭包。
    - 用 `tauri::async_runtime::spawn` 运行 future，立即返回 `statuses()`。
  - **`import`**：
    - 先确认没有任务在运行（避免弹出对话框后才发现冲突）；
    - 然后在 `spawn_blocking` 中执行 `app.dialog().file().add_filter("ONNX 模型", &["onnx"]).blocking_pick_file()`；
    - 用户取消就原样返回 `statuses()`；选了文件就用 `FilePath::into_path()` 转成路径，调用 `begin_import`，再 spawn。
  - **`cancel`** / **`delete`**：直接调用对应方法。
- **只在调试构建中生效的真机测试开关**：`#[cfg(debug_assertions)]` 下，如果设置了环境变量 `TUNE_LOVE_MODEL_BREAK_ORIGIN`，就把候选列表中 `Origin` 的 URL 换成 `https://127.0.0.1:9/blocked`，直连必然连接失败。供任务 12 使用；发布构建里不存在这段代码。

  实现方式：命令层在调试构建中读取这个环境变量，作为任务 5 的 `begin_download` 的 `break_origin` 参数传入；发布构建一律传 `false`。
- **`lib.rs`**：
  - 在 single-instance 插件之后加 `.plugin(tauri_plugin_dialog::init())`；
  - `.manage(devocal::model::state::ModelState::new(bundled(), std::env::var_os(devocal::MODEL_ENV).filter(|v| !v.is_empty()).map(PathBuf::from)))`；
  - 在 setup 中，任务 2 的迁移之后调用 `set_models_dir(dir.join("models"))`；
  - 在 `generate_handler!` 中加 `devocal::model::get_model_status`、`devocal::model::model_command`。
  - 不改 `capabilities/*.json`：自定义命令不需要单独授权；对话框只在 Rust 侧调用。

- [ ] **Step 5: 运行全部 Rust 测试，并构建**

运行：
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `cargo build --manifest-path src-tauri/Cargo.toml`

预期：全部 PASS，构建成功。

- [ ] **Step 6: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src
git commit -m "feat(model): get_model_status/model_command, file import dialog and auto-enable"
```

---

### Task 7: 前端纯逻辑 `modelDownload.ts`

**Files:**
- Create: `src/modelDownload.ts`、`src/modelDownload.test.ts`

**Interfaces:**
- Produces：
  - `import manifestJson from "../src-tauri/models.json";`：`tsconfig` 已开 `resolveJsonModule`。
  - `export interface ModelFile { file: string; bytes: number; sha256: string; origin: string; mirrorable: boolean }`
  - `export interface ModelInfo { id: string; name: string; tier: "realtime" | "quality"; files: ModelFile[]; sampleRate: number; latencyMs: number; runtime: "cpu" | "cuda"; license: { code: string; weights: string; trainingData: string[] }; source: string }`
  - `export const MODEL_MANIFEST: { version: number; mirrors: string[]; models: ModelInfo[] }`
  - `export const STEMGENRT_ID = "stemgenrt-hop128"`
  - `export type ModelPhase = "missing" | "downloading" | "verifying" | "installed" | "failed"`
  - `export interface ModelStatus { id: string; phase: ModelPhase; receivedBytes: number; totalBytes: number; source: string | null; path: string | null; error: string | null }`
  - `export function parseModelStatuses(value: unknown): ModelStatus[] | null`：不是数组 → `null`；逐项解析，`id` 不是字符串或 `phase` 不认识的项丢弃；数字不是有限值时取 0。
  - `export const modelTotalBytes = (m: ModelInfo) => number`
  - `export function formatMiB(bytes: number): string`：`(bytes / 1048576).toFixed(1)`。
  - `export function approxSize(bytes: number): string`：`` `约 ${Math.round(bytes / 1048576)} MB` ``。
  - `export function sourceLabel(source: string | null): string`
  - `export function progressText(s: ModelStatus): string`
  - `export function progressPercent(s: ModelStatus): number`
  - `export function modelErrorText(code: string): string`
  - `export function isValidMirrorPrefix(s: string): boolean`
  - `export function pinnedCommit(origin: string): string | null`
  - `export function consentKey(m: ModelInfo): string`
  - `export function hasModelConsent(m: ModelInfo): boolean` / `export function rememberModelConsent(m: ModelInfo): void`：localStorage 键 `helper-model-consent-v1`，内容为 `{"keys": string[]}`，读写异常都吞掉。
  - `export function readMirrorPrefix(): string` / `export function saveMirrorPrefix(prefix: string): void`：键 `helper-model-mirror-v1`，内容为 `{"prefix": string}`。

**固定文案**（`modelErrorText`）：

| 机器码 | 文案 |
|---|---|
| `network_unreachable` | 无法连接下载地址。请检查网络，或设置系统代理后重试。 |
| `timeout` | 下载超时，请重试。 |
| `http_status:429`、`http_status:5xx` | 下载服务繁忙（HTTP <n>），请稍后再试。 |
| 其余 `http_status:<n>` | 下载地址暂时无法访问（HTTP <n>）。稍后再试，或从本地文件导入。 |
| `source_html` | 加速服务返回了网页，已换下一个来源 |
| `size_mismatch`、`sha256_mismatch` | 下载的文件校验不通过，已删除。请重试。 |
| `import_mismatch` | 所选文件与模型不符（大小或 SHA-256 不一致），未导入。 |
| `disk_full`、`write_failed` | 无法写入模型文件夹（空间不足或没有权限）。 |
| `install_denied` | 模型文件正被使用，请先关闭去人声再试。 |
| `delete_denied` | 模型文件正被使用，请先关闭去人声再删除。 |
| `cancelled` | 已取消下载。 |
| `already_running` | 正在下载中。 |
| `all_sources_failed` | 所有下载来源都失败了。可设置系统代理、填写加速前缀，或从本地文件导入。 |
| `invalid_prefix` | 下载加速前缀无效：须以 https:// 开头、以 / 结尾。 |
| `unknown_model`、`import_unsupported` | 这个模型暂不支持此操作。 |
| 其他 | 下载失败（<code>），请重试。 |

- [ ] **Step 1: 写失败的测试**

`src/modelDownload.test.ts` 在 node 环境运行，需要在 `beforeEach` 里给 `globalThis.localStorage` 装一个内存实现。

```ts
it("parses a status array and drops unknown entries", () => {
  expect(parseModelStatuses(null)).toBeNull();
  expect(parseModelStatuses({ phase: "missing" })).toBeNull();
  expect(parseModelStatuses([{ id: "a", phase: "downloading", receivedBytes: 5, totalBytes: 10, source: "origin", path: null, error: null },
    { id: "b", phase: "exploded" }, { phase: "missing" }])).toEqual([{ id: "a", phase: "downloading", receivedBytes: 5, totalBytes: 10, source: "origin", path: null, error: null }]);
});
it("formats sizes in MiB the way the spec quotes them", () => {
  expect(formatMiB(37529132)).toBe("35.8"); expect(approxSize(37529132)).toBe("约 36 MB");
});
it("labels sources", () => {
  expect(sourceLabel("origin")).toBe("直连"); expect(sourceLabel("mirror:ghfast.top")).toBe("镜像 ghfast.top");
  expect(sourceLabel("custom")).toBe("自定义加速"); expect(sourceLabel("import")).toBe("本地导入");
  expect(sourceLabel("env")).toBe("环境变量"); expect(sourceLabel(null)).toBe("");
});
it("writes the progress line with source", () => {
  expect(progressText({ id: "a", phase: "downloading", receivedBytes: 12_897_485, totalBytes: 37_529_132, source: "mirror:ghfast.top", path: null, error: null }))
    .toBe("已下载 12.3 / 35.8 MB · 来源：镜像 ghfast.top");
  // 没有 source 时不带“ · 来源：”；progressPercent 的结果为整数并截在 0..100，totalBytes 为 0 时返回 0
});
it("maps every machine code to Chinese", () => { /* 逐条断言上表，包括 http_status:404、http_status:503 和未知码 */ });
it("validates mirror prefixes like the backend", () => {
  for (const ok of ["https://ghfast.top/", "https://a.b/c/"]) expect(isValidMirrorPrefix(ok)).toBe(true);
  for (const bad of ["", "http://a.b/", "https://a.b", "https:///", "https://a b/", " https://a.b/", "javascript:alert(1)/"]) expect(isValidMirrorPrefix(bad)).toBe(false);
});
it("remembers consent per model SHA-256 set", () => {
  const m = MODEL_MANIFEST.models[0];
  expect(hasModelConsent(m)).toBe(false); rememberModelConsent(m); expect(hasModelConsent(m)).toBe(true);
  expect(hasModelConsent({ ...m, files: [{ ...m.files[0], sha256: "0".repeat(64) }] })).toBe(false);
});
it("reads the pinned commit from the origin", () => {
  expect(pinnedCommit(MODEL_MANIFEST.models[0].files[0].origin)).toBe("61df8f4aa1555ef110308d01ea92b54ace770979");
});
it("survives a throwing localStorage", () => { /* getItem/setItem 都抛异常：hasModelConsent → false，readMirrorPrefix → ""，两个写函数都不抛 */ });
```

- [ ] **Step 2: 运行 `npx vitest run src/modelDownload.test.ts`，确认失败**

- [ ] **Step 3: 实现 `src/modelDownload.ts`**

- `consentKey` = `` `${id}:${files.map(f => f.sha256).join(",")}` ``；
- `isValidMirrorPrefix` 的规则与 Rust 的 `valid_prefix` 逐条一致；
- `pinnedCommit` 取 origin 中第一个 40 位十六进制的路径段。

- [ ] **Step 4: 运行 vitest 和类型检查，确认 PASS**

运行：
- `npx vitest run src/modelDownload.test.ts`
- `npx tsc --noEmit`

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src/modelDownload.ts src/modelDownload.test.ts
git commit -m "feat(model): frontend status parsing, copy, formats and consent storage"
```

---

### Task 8: 轮询 hook 与设置页模型区块（各状态）

**Files:**
- Create: `src/useModelDownload.ts`、`src/components/ModelSection.tsx`、`src/components/ModelSection.css`、`tests/model-download.spec.ts`
- Modify: `src/components/DevocalSettings.tsx`；以及 `tests/devocal.spec.ts`、`tests/auto-apply-key-scale.spec.ts`、`tests/autotune-control.spec.ts`、`tests/colors.spec.ts`、`tests/display.spec.ts` 中的 `invoke` 模拟

**Interfaces:**
- Consumes：任务 7 的全部导出；后端命令 `get_model_status`、`model_command`。
- Produces：
  - `export type ModelAction = "download" | "cancel" | "import" | "delete"`
  - `export function useModelDownload(): { statuses: ModelStatus[] | null; send(id: string, action: ModelAction, options?: { autoEnable?: boolean; mirrorPrefix?: string }): Promise<void> }`
    - 只在 `isTauri()` 时轮询，间隔 500 ms，写法仿照 `useDevocal`（带 epoch 防止旧结果覆盖新结果，卸载后停止）；
    - `send` 调用 `invoke("model_command", { request: { id, action, autoEnable: options?.autoEnable ?? false, mirrorPrefix: options?.mirrorPrefix || null } })`；
    - 被拒绝时抛出 `new Error(modelErrorText(String(reason)))`。
  - `export function ModelSection(props: { autoEnablePending: boolean; onAutoEnableConsumed(): void; focusSeq: number }): JSX.Element`
    - 下载、继续下载、重试都由 `ModelSection` 内部的 `startDownload(model)` 处理：本任务里直接调用 `send(id, "download")`；任务 9 在它前面加确认框和前缀；
    - 三个 props 由任务 10 使用；本任务里 `DevocalSettings` 传 `false`、空函数、`0`。

**每个清单模型一行**（`<div role="group" aria-label={model.name}>`），状态取自 `statuses` 中 id 相同的那一项：
- **没有状态**：显示“正在读取模型状态…”，不显示按钮。
- **`missing` 且 `receivedBytes == 0`**：
  - 按钮“下载模型（约 36 MB）”，大小用 `approxSize`；
  - 旁边一行小字：“许可待作者确认 · ”后接一个文字按钮“来源与许可”（本任务中先不响应点击，任务 9 接上）。`weights` 不是 `pending` 时，小字改为 `权重许可：${weights}`。
- **`missing`（含有部分下载时）**：在下载按钮旁再显示一个文字按钮“从本地文件导入…”，调用 `import`（控制者裁定 2026-10-03：缺失时也提供导入，便于国内用户用其他方式取得文件；Playwright 补一条“缺失状态下导入”的用例）。
- **`missing` 且 `receivedBytes > 0`**：
  - 按钮“继续下载”；
  - 小字“已下载 12.3 / 35.8 MB”；
  - 按钮“删除已下载部分”，调用 `delete`。
- **`downloading`**：
  - 进度条，`role="progressbar"`，`aria-valuenow` 取 `progressPercent`。可用 Radix Themes 的 `Progress`；如果它不输出 `aria-valuenow`，就手写一个 div。
  - 一行 `progressText`；
  - 按钮“取消”。
- **`verifying`**：“正在校验…”。
- **`installed`**：
  - “模型已就绪”，`source == "env"` 时为“模型已就绪（开发者环境变量）”；
  - 用 `<details><summary>详细信息</summary>` 展开路径、`37,529,132 字节` 和完整 SHA-256；
  - 按钮“删除模型”；`source == "env"` 时不显示该按钮。
- **`failed`**：
  - `<p role="alert" className="color-error">`，内容为 `modelErrorText(error)`；
  - 按钮“重试”（走 `startDownload`）；
  - 按钮“从本地文件导入…”，调用 `import`。
- **`send` 被拒绝**：在该行显示 `role="alert"` 的错误；下一次操作时清空。

**Playwright 模拟**：`tests/model-download.spec.ts` 自己写一个 `prepare(page, view, { models, devocal })`：
- 在 `tests/devocal.spec.ts` 的模拟基础上，加 `get_model_status`（返回 `structuredClone(window.models)`）和 `model_command`；
- `model_command` 把 `args.request` 记进 `window.modelRequests`，并按动作推进状态：
  - `download`：变为 `downloading`，`receivedBytes: 12_897_485`，`source: "mirror:ghfast.top"`；
  - `cancel`：变为 `missing`，`receivedBytes` 保持不变；
  - `import`：变为 `installed`，`source: "import"`；
  - `delete`：变为 `missing`，`receivedBytes: 0`；
  - `window.modelFailure` 被设置时，抛出该字符串。
- 另外 5 个 spec 的模拟各加一行：`if (command === "get_model_status") return [];`。

- [ ] **Step 1: 写失败的 Playwright 测试**

视口 760×600，页面 `/?view=settings`。

```ts
test("settings: a missing model offers download with size and licence note", ...)
  // getByRole("group", { name: "StemgenRT（低延迟）" }) 内：按钮 "下载模型（约 36 MB）"；文字 "许可待作者确认"；按钮 "来源与许可"
test("settings: download progress shows bytes, source and a cancel button", ...)
  // 初始状态 downloading → progressbar 的 aria-valuenow 为 "34"；文字 "已下载 12.3 / 35.8 MB · 来源：镜像 ghfast.top"；
  // 点“取消” → modelRequests 为 [{ id: "stemgenrt-hop128", action: "cancel", autoEnable: false, mirrorPrefix: null }]
test("settings: a partial download offers resume and delete-partial", ...)
  // missing 且 receivedBytes>0 → 显示“继续下载”和“已下载 12.3 / 35.8 MB”；点“删除已下载部分” → 请求的 action 为 "delete"
test("settings: verifying and ready states", ...)
  // verifying → "正在校验…"；installed → "模型已就绪"；展开“详细信息” → 显示 path、"37,529,132 字节"、完整 SHA-256；“删除模型” → 请求 delete
test("settings: an env-provided model has no delete button", ...)
test("settings: a failure shows the Chinese error with retry and import", ...)
  // failed + all_sources_failed → alert 显示对应文案；“从本地文件导入…” → 请求 import；
  // 设置 modelFailure = "already_running" 后点“重试” → alert 显示“正在下载中。”
```

- [ ] **Step 2: 运行 `npx playwright test tests/model-download.spec.ts`，确认失败**

- [ ] **Step 3: 实现 `useModelDownload.ts`、`ModelSection.tsx/.css`，并挂进 `DevocalSettings`**

`ModelSection` 放在说明文字与“释放播放器”按钮之间。样式沿用 `devocal-settings-*` 的间距；错误使用已有的 `.color-error`。

- [ ] **Step 4: 运行测试，确认 PASS**

运行：
- `npx playwright test tests/model-download.spec.ts tests/devocal.spec.ts tests/auto-apply-key-scale.spec.ts tests/autotune-control.spec.ts tests/colors.spec.ts tests/display.spec.ts`
- `npx tsc --noEmit`

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src/useModelDownload.ts src/components/ModelSection.tsx src/components/ModelSection.css src/components/DevocalSettings.tsx tests
git commit -m "feat(model): settings model row with progress, cancel, resume, ready and failure states"
```

---

### Task 9: 确认框与“下载加速前缀”

**Files:**
- Create: `src/components/ModelConsentDialog.tsx`
- Modify: `src/components/ModelSection.tsx`、`src/components/DevocalSettings.tsx`、`tests/model-download.spec.ts`

**Interfaces:**
- Consumes：任务 7 的 `hasModelConsent`、`rememberModelConsent`、`pinnedCommit`、`readMirrorPrefix`、`saveMirrorPrefix`、`isValidMirrorPrefix`、`MODEL_MANIFEST`；任务 8 的 `ModelSection` 和 `useModelDownload`。
- Produces：
  - `export function ModelConsentDialog(props: { model: ModelInfo | null; mirrorPrefix: string; onAccept(): void; onCancel(): void }): JSX.Element`：`model` 为 `null` 时关闭。
  - 下载流程（`ModelSection` 内部的 `startDownload`）：点击下载、继续下载或重试时，如果 `hasModelConsent(model)` 为 true，直接发送请求；否则打开确认框，点“同意并下载”后先 `rememberModelConsent`，再发送请求。
  - 小字里的“来源与许可”按钮总是打开确认框。
  - 下载请求带上 `mirrorPrefix`：仅当 `readMirrorPrefix()` 非空且合法时才带。

**确认框**：用 Radix Themes 的 `AlertDialog`，标题“下载去人声模型”。正文逐段如下，取值来自清单；`<…>` 表示代入的值，SHA-256 和提交号用等宽字体完整显示：
1. `模型：<name>。用于实时去人声；模型不随安装包发布，需要下载到本机。`
2. `来源：<source>，固定提交 <pinnedCommit>。`
3. `大小：<formatMiB> MB（<bytes 千分位> 字节）`；`SHA-256：<sha256>`。多文件模型每个文件各列一行。
4. `许可：仓库代码采用 <license.code>；模型权重的许可作者未单独声明，待确认。`（`weights == "pending"` 时）
5. `训练数据：<trainingData 用“、”连接>。本应用免费、非商业，请自行判断使用场景。`
6. `加速：直连失败时，可能经由第三方 GitHub 加速服务下载（<mirrors 的 host 用“、”连接>[、你填写的 <自定义前缀的 host>]），它们能看到这次下载请求；文件一律按上面的 SHA-256 校验，不符即删除。`
7. `写入位置：%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\<id>\`
8. 按钮：“同意并下载”和“取消”。

**高级设置**：放在 `DevocalSettings` 底部的 `<details><summary>高级</summary>` 中：
- `TextField`，标签为“下载加速前缀（可选）”，占位文字 `https://example.com/`；
- 输入非空且不合法时，显示 `role="alert"` 的“须以 https:// 开头、以 / 结尾”，并且不保存；
- 合法或为空时，在失焦或回车时调用 `saveMirrorPrefix`。

- [ ] **Step 1: 写失败的 Playwright 测试**（追加到 `tests/model-download.spec.ts`）

```ts
test("consent dialog lists source, commit, size, sha256, licence, training data, mirrors and location", ...)
  // 点“下载模型（约 36 MB）” → alertdialog 中包含：
  // "https://github.com/sweetspotsoundsystem/stemgen-rt"、"61df8f4aa1555ef110308d01ea92b54ace770979"、"37,529,132 字节"、
  // 完整 SHA-256、"待确认"、"MUSDB18-HQ（仅教育用途）"、"MoisesDB（CC BY-NC-SA 4.0）"、"非商业"、
  // "ghfast.top、ghproxy.net、ghproxy.vip"、"models\\stemgenrt-hop128\\"；不包含 "权重采用 MIT"
test("cancelling consent sends no model_command", ...)        // 点“取消” → modelRequests 为空，localStorage 中没有同意记录
test("consent is remembered per model sha256", ...)            // 同意一次后，再点“下载” → 不弹框直接发出请求；
                                                                // 预置 localStorage 中的 key 为别的 sha → 仍然弹框
test("mirror prefix: invalid input is flagged and not sent; a valid one is sent and listed", ...)
  // 输入 "http://x/" 后失焦 → 出现 alert，下载请求中 mirrorPrefix 为 null；
  // 改为 "https://my.proxy/" → 确认框第 6 段含 "my.proxy"，请求中 mirrorPrefix 为 "https://my.proxy/"
```

- [ ] **Step 2: 运行 `npx playwright test tests/model-download.spec.ts`，确认新增用例失败**

- [ ] **Step 3: 实现 `ModelConsentDialog`，接入 `ModelSection`，并加上前缀输入框**

- [ ] **Step 4: 运行，确认 PASS**

运行：
- `npx playwright test tests/model-download.spec.ts`
- `npx tsc --noEmit`

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src/components tests/model-download.spec.ts
git commit -m "feat(model): consent dialog with licence and mirror disclosure; optional mirror prefix"
```

---

### Task 10: 主窗口提示 → 打开设置并定位；从“未找到模型”入口下载后自动开启

**Files:**
- Create: `src/settingsSection.ts`、`src/openSettings.ts`
- Modify:
  - `src-tauri/src/settings.rs`
  - `src/devocal.ts:77`、`src/devocal.test.ts`
  - `src/components/PlayerControls.tsx:41-45,81`、`src/components/PlayerControls.css:4`
  - `src/components/SettingsButton.tsx`
  - `src/SettingsPage.tsx`、`src/components/DevocalSettings.tsx`、`src/components/ModelSection.tsx`
  - `tests/devocal.spec.ts`、`tests/model-download.spec.ts`

**Interfaces:**
- Produces（Rust，`settings.rs`）：
  - `pub fn settings_url(section: Option<&str>) -> Result<String, String>`：
    - `None` → `"index.html?view=settings"`；
    - `Some("devocal-model")` → `"index.html?view=settings&section=devocal-model"`；
    - 其他 → `Err`。
  - `open_settings(app, window, state, section: Option<String>)`：
    - 先校验 `section`；
    - 窗口不存在时，克隆窗口配置，把 `url` 设为 `WebviewUrl::App(settings_url(..).into())` 后再创建；
    - 窗口已存在且 `section` 是 `Some` 时，调用 `app.emit_to("settings", "settings-section", section)`（需要 `use tauri::Emitter`）；
    - 然后照旧执行 show 和 focus。
- Produces（TS）：
  - `src/openSettings.ts`：`export async function openSettings(section?: "devocal-model"): Promise<void>`。
    - Tauri 环境：调用 `invoke("open_settings", section ? { section } : {})`；
    - 非 Tauri 环境：打开 `/?view=settings` 预览窗口，有 `section` 时加 `&section=…`。
    - `SettingsButton` 改为调用它（行为不变）。
  - `src/settingsSection.ts`：
    - `export const SETTINGS_SECTION_EVENT = "settings-section"`
    - `export type SettingsSection = "devocal-model"`
    - `export function parseSettingsSection(v: unknown): SettingsSection | null`
    - `export function useSettingsSectionRequest(): { section: SettingsSection; seq: number } | null`：
      - 初始值来自 `location.search` 中的 `section`；
      - `isTauri()` 时调用 `listen(SETTINGS_SECTION_EVENT, …)`，每收到一次合法的值，`seq` 加 1；
      - `listen` 被拒绝时忽略。
  - `devocal.ts`：
    - `DevocalNotice` 增加可选字段 `action?: "open-model-settings"`；
    - `unavailable` 且不是 `engine_unavailable` 的那一行改为 `{ text: "未找到去人声模型，请在设置中下载", kind: "warning", action: "open-model-settings" }`。
  - `PlayerControls`：
    - 当 `devocalLine?.action` 存在时，`.devocal-warning` 中渲染 `<button type="button" className="devocal-warning-action" onClick={() => openSettings("devocal-model").catch(() => onNotice("未能打开设置，请重试"))}>{text}</button>`；
    - 外层 span 仍然是唯一的 polite live region；
    - CSS：`.devocal-warning-action` 要有 `pointer-events: auto`，样式不变（继承颜色和字号），加下划线和 `cursor: pointer`。
  - `SettingsPage` → `DevocalSettings`：传入 `request = useSettingsSectionRequest()`。
    - `request` 的 `seq` 变化时，`DevocalSettings` 把去人声区块 `scrollIntoView({ block: "start" })`，设 `autoEnablePending = true`，并把 `focusSeq` 设为 `request.seq`。
    - `ModelSection` 在 `focusSeq` 变化（且非 0）时，聚焦 StemgenRT 行的第一个可用按钮。
    - `autoEnablePending` 为 true 时，在 StemgenRT 行下方显示“下载完成后将自动开启去人声”；该行下一次发出的 `download` 或 `import` 带 `autoEnable: true`，发出后调用 `onAutoEnableConsumed()`，由 `DevocalSettings` 把标志清掉。
    - 用户从设置按钮直接打开设置时没有 `request`，请求一律带 `autoEnable: false`。
  - 后端已经在任务 6 中根据 `autoEnable` 自动开启，本任务不改后端的这部分逻辑。

- [ ] **Step 1: 写失败的测试**

- **Rust（`settings.rs` 的测试模块）**：`settings_url_accepts_only_known_sections`，覆盖上述三种输入。
- **vitest（`src/devocal.test.ts`）**：
  - `unavailable` + `model_not_found` 时，`devocalNotice` 为 `{ text: "未找到去人声模型，请在设置中下载", kind: "warning", action: "open-model-settings" }`；
  - `engine_unavailable` 时没有 `action`；
  - 原来断言“未找到去人声模型”的用例同步修改。
- **Playwright**：
  - `tests/devocal.spec.ts`：
    - 模拟中加 `open_settings`，把 `args` 记进 `window.openSettingsCalls`；
    - 用例 `only one polite live region carries text at a time` 中的文案改为新文案。
  - `tests/model-download.spec.ts` 新增：

```ts
test("main: the missing-model warning opens settings at the model row", ...)
  // prepare("/", devocal: { phase: "unavailable", error: "model_not_found" }) →
  // getByRole("button", { name: "未找到去人声模型，请在设置中下载" }) 不需要 hover 就可见、可点击；
  // 点击后 openSettingsCalls 为 [{ section: "devocal-model" }]
test("settings opened from the missing-model hint downloads with autoEnable", ...)
  // goto "/?view=settings&section=devocal-model" → 去人声区块在视口内（boundingBox.y < 600），
  // 有“下载完成后将自动开启去人声”；同意后，请求中 autoEnable 为 true；
  // 第二次下载（先 cancel 再“继续下载”）时 autoEnable 为 false
test("settings opened normally downloads without autoEnable", ...)
test("a settings-section event in an open settings window focuses the model row", ...)
  // 模拟中实现 __TAURI_INTERNALS__.transformCallback（存回调并返回 id），以及 "plugin:event|listen"（记下 handler id）；
  // 调用记下的回调，参数为 { event: "settings-section", id: 1, payload: "devocal-model" } → 出现提示文字，焦点落在“下载模型（约 36 MB）”上。
  // 实现前先对照已安装的 @tauri-apps/api 中 listen/transformCallback 的源码，核对参数形状。
```

- [ ] **Step 2: 运行，确认失败**

运行：
- `cargo test --manifest-path src-tauri/Cargo.toml settings`
- `npx vitest run src/devocal.test.ts`
- `npx playwright test tests/devocal.spec.ts tests/model-download.spec.ts`

- [ ] **Step 3: 实现上述 Rust 与 TS 改动**

- [ ] **Step 4: 运行同一组命令和 `npx tsc --noEmit`，确认全部 PASS**

- [ ] **Step 5: 提交**

先确认分支是 `model-download`，然后：

```bash
git add src-tauri/src/settings.rs src tests
git commit -m "feat(model): missing-model hint opens settings at the model row; auto-enable after that download"
```

---

### Task 11: 文档与发版检查

**Files:**
- Modify: `.github/release-notes.md:6`、`README.md:105`
- Create: `docs/release-checklist.md`、`scripts/check-model-mirrors.mjs`

**Interfaces:**
- Consumes：任务 1 的 `scripts/model-manifest.mjs`。
- Produces：`node scripts/check-model-mirrors.mjs [--id stemgenrt-hop128]`。该脚本会访问真实网络，只在发版时手动运行，不得放进 `npm test`、CI 或任何 `*.test.ts`。

- [ ] **Step 1: 更新发布说明第 4 条**

> **去人声需要 StemgenRT 模型，安装包不附带。** 在“设置 → 去人声”里点“下载模型（约 36 MB）”。阅读来源与许可说明并同意后，程序从作者仓库的固定提交下载；直连失败时，会自动改用第三方 GitHub 加速服务。文件一律按固定的 SHA-256 校验，不符即删除。也可以从本地文件导入。模型保存在 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\stemgenrt-hop128\model.onnx`。权重的许可尚待作者确认；训练数据仅限非商业用途。

- [ ] **Step 2: 更新 README 第 105 行**

> StemgenRT 权重不随包发布：在设置里下载（需同意来源与许可说明），或者开发时用 `npm run fetch:stemgenrt -- --accept`。两种方式都放到 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\stemgenrt-hop128\model.onnx`，并校验 SHA-256；旧位置 `models\stemgenrt-hop128.onnx` 会在启动时自动迁移。

- [ ] **Step 3: 写 `scripts/check-model-mirrors.mjs`**

对清单中每个 `mirrorable` 文件的每个来源（直连 + 各镜像），发出 `GET` 请求，带 `Range: bytes=0-65535`，并检查：
- 状态为 206；
- `Content-Range` 的总大小等于 `bytes`；
- `content-type` 不是 `text/html`；
- 前 64 KiB 的 SHA-256 与直连的结果一致。直连不通时，改为与第一个通过的镜像比较，并在输出中注明。

每个来源输出一行“通过/失败 + 原因”。只要有来源失败，退出码就是 1。超时 15 s。

- [ ] **Step 4: 新建 `docs/release-checklist.md`**

本仓库目前没有发版清单，所以新建；只写这一条，其余发版步骤沿用现有流程。

```markdown
# 发版检查清单

- [ ] 实测模型下载来源：运行 `node scripts/check-model-mirrors.mjs`。
  - 不合格的镜像从 `src-tauri/models.json` 的 `mirrors` 中删除；
  - 新增的镜像必须先通过这一检查，并且不得收录会返回网页或被安全软件拦截的服务（2026-10-03 已排除 gh-proxy.com 与 gh-proxy.net）；
  - 结果（日期、各来源结论）记入该版本的发布记录。
```

- [ ] **Step 5: 校验**

运行：`npx vitest run scripts/model-manifest.test.ts`
预期：PASS。不要运行 `check-model-mirrors.mjs`，它会访问网络。

- [ ] **Step 6: 提交**

先确认分支是 `model-download`，然后：

```bash
git add .github/release-notes.md README.md docs/release-checklist.md scripts/check-model-mirrors.mjs
git commit -m "docs(model): in-app download in release notes and README; mirror check before release"
```

---

### Task 12: 全量回归与真机验证（需用户授权）

**Files:**
- Create（仅在真机测试完成后）：`docs/2026-10-03_model-download-real-machine.md`，记录真机测试结果。

- [ ] **Step 1: 全量自动化测试（不访问网络）**

运行：
- `cargo test --manifest-path src-tauri/Cargo.toml`
- `npm test`
- `npx playwright test`
- `npx tsc --noEmit`

预期：全部通过。任何失败都先修复，再继续下一步。

- [ ] **Step 2: 征得用户授权**

在对话中列出将要做的事，用户明确同意后才执行：
1. 从作者仓库和三个镜像真实下载约 36 MB；
2. 运行 `npm run tauri dev`，前提是已有 `fetch:cmake`，并且已执行 `build:engine`；
3. 读取和改动 `%LOCALAPPDATA%\io.github.ylzhe.tunelove\models\`：如果本机已有模型，先移到备份目录，测完恢复；
4. 第 (3) 项测试需要用户自己断开、再恢复网络；我们不改系统网络设置；
5. 第 (4) 项测试需要接管播放器，每次执行前都要核验播放器进程身份。

没有得到同意就停在这里，并报告“真机测试待授权”。

- [ ] **Step 3: 执行真机测试**（记录日期、版本、结果）

1. **直连**：`models\` 为空时，从设置页下载 → 确认框内容正确；状态的来源为“直连”；完成后，文件的大小和 SHA-256 与清单一致；没有残留的 `.part` 文件。
2. **切换镜像**：设置 `TUNE_LOVE_MODEL_BREAK_ORIGIN=1` 后启动调试构建，再下载 → 直连失败后，状态的来源变为“镜像 ghfast.top”（或下一个可用的镜像）；文件校验通过。
3. **续传**：
   - 下载中途由用户断网，几秒后恢复 → 从断点继续；`.part.json` 记录的字节数没有回到 0；
   - 再测一次：下载中途结束应用，重启后点“继续下载” → 从断点续传。
4. **自动开启**：删除模型后，在主窗口打开去人声开关 → 提示“未找到去人声模型，请在设置中下载”；点击提示 → 设置页定位到模型行，并显示“下载完成后将自动开启去人声”；下载完成后，去人声自动进入“去人声中”。
5. **旧位置迁移**：把模型放在 `models\stemgenrt-hop128.onnx` 后启动 → 文件被移到 `models\stemgenrt-hop128\model.onnx`。
6. 恢复用户原来的模型文件，恢复环境变量。

- [ ] **Step 4: 写真机记录，并提交**

把结果写进 `docs/2026-10-03_model-download-real-machine.md`，体例参照 `docs/2026-10-02_installer-local-check.md`，与预期不符的地方列在“偏差”一节。先确认分支是 `model-download`，然后：

```bash
git add docs/2026-10-03_model-download-real-machine.md
git commit -m "docs(model): real-machine download, mirror fallback, resume and auto-enable check"
```

推送须另行征得用户同意。
