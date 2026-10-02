# 第三方组件与发布范围

此目录的自主代码采用 [MIT License](LICENSE)。构建仅复用下列公开项目。

| 组件 | 固定 revision | 来源及许可证 |
|---|---|---|
| Steinberg VST3 pluginterfaces | `4f547e8e102b47de4a8b8aaf343c73b700786372` | https://github.com/steinbergmedia/vst3_pluginterfaces ，MIT |
| MinHook | `c3fcafdc10146beb5919319d0683e44e3c30d537` | https://github.com/TsudaKageyu/minhook ，BSD 两条款，包括 HDE32/HDE64 notices |

[bootstrap.ps1](bootstrap.ps1) 获取固定 revision；已有 checkout 的 revision 不符或存在修改时停止并保留现场。
许可证原文分别位于下载后的 `vendor/pluginterfaces/LICENSE.txt` 和 `vendor/minhook/LICENSE.txt`。
发布二进制时须随包保留这些完整声明；[package.py](package.py) 会自动将它们加入源码及二进制包。

未包含 Auto-Tune、Auto-Key、卡卡助手、`em64.dll`、`em32.dll`、商业安装包、派生反汇编、账号、授权或本地取证材料。
产品名称用于标识互操作目标。本项目不是 Antares、Image-Line 或卡卡的官方产品。
MIT 许可证只覆盖这里的自主实现，不授予商业产品的分发权。
