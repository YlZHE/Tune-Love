# Tune Love

[![CI](https://github.com/YlZHE/Tune-Love/actions/workflows/ci.yml/badge.svg)](https://github.com/YlZHE/Tune-Love/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Windows](https://img.shields.io/badge/Windows-10%2F11%20x64-0078D6?logo=windows&logoColor=white)

我平时喜欢跟着歌唱，唱的时候会挂 Auto-Tune。每次都一样麻烦：先找伴奏，找不到就想办法把原唱去掉；再自己猜这首歌是什么调，去插件里一项一项选；选错了人声还会被拉歪，比不修音更难听。

所以我做了这个小工具，把这几步交给电脑。

## 它现在会做的

- 在桌面上放一个小窗，显示正在播放的歌名、歌手、封面和进度，也能切歌、暂停。
- 点一下按钮去掉原唱，你听到的是伴奏。歌词还是在原来的播放器里看，我没有让声音整体延后，所以歌词和声音基本对得上。
- 一边放歌一边判断这首歌用哪些音，把结果写进 Auto-Tune 的 Key 和 Scale。这个要在设置里自己打开，默认是关的。
- 判断不准的时候，宁可先不修音，也不乱拉。窗口标题会写当前用的调，比如 `F Minor`；拿不准时会是 `F Chromatic`。

## 我是怎么取舍的

我只在乎一件事：唱的时候不跑调，也不被误拉。至于调名认得对不对，没那么重要。大调和它对应的小调用的是同一组音，认成另一个也没关系，只要我要唱的音在里面就行。

判断调只听歌，不听我唱的声音。声音也不会为了多听一会儿而延后。

另外，它不能把显卡或处理器占满。如果某个办法会这样，我就换更轻的。

## 还没做好的

- 去人声现在只有一个模型。它的延迟大概 75 毫秒，我试听过，能接受，但原唱的残留还听得出来。我想做的"换更干净的模型"只写了设计，没开始写。
- 把整首歌升降调之后再输出，还没做。
- 我只在自己的电脑和几个播放器、一个 Auto-Tune 版本上试过。别的环境能不能用，我不知道。

## 怎么用

1. 去 [Releases](https://github.com/YlZHE/Tune-Love/releases) 下载 `Tune-Love_<版本>_x64-setup.exe`，直接安装，不用管理员权限。
2. 打开你平时的音乐软件放一首歌，小窗会自己出现。
3. 想要伴奏，点小窗上那个人物加声波的图标。
4. 想自动选调，去设置页连接 Auto-Tune，再打开"自动写入 Key/Scale"。

系统要求 Windows 10 2004 以上，或者 Windows 11，64 位。

几件事先说清楚：

- 安装包没有签名，Windows 会提示，点"更多信息"再"仍要运行"就行。
- 连接 Auto-Tune 的部分要往那个软件里放一小段程序，杀毒软件可能会报警。源码在 `reference/` 里，可以自己看。
- 去人声用的模型没放进安装包，第一次用时要在设置里点下载，会先让你确认来源和许可。
- 升级前要先关掉本程序和 Auto-Tune 所在的软件，不然文件被占用。
- 不读麦克风，不上传录音，也不记你听了什么歌。

## 想自己编译

```powershell
npm ci
npm run tauri dev
```

详细的构建步骤、测试方法，以及用到的开源项目和许可，都在 [README.dev.md](README.dev.md) 里，那份比较长，写得比较技术。

## 许可

代码用 GPL-3.0，见 [LICENSE](LICENSE)。这个项目只做免费、非商业用途。用到的第三方东西各有各的许可，其中 Rare UI 和 React Bits 带有 Commons Clause 限制，具体看 `licenses/` 目录。

可选的去人声模型 bytesep（字节跳动，Kong 等人，权重 CC BY 4.0，[Zenodo 5804160](https://doi.org/10.5281/zenodo.5804160)、[5513378](https://doi.org/10.5281/zenodo.5513378)）和 HTDemucs（Meta Demucs，MIT；训练数据来源不明，仅限非商业使用）已由本项目转换修改（转成 ONNX 并改写计算图，权重数值未改），许可全文见 `licenses/bytesep.txt`、`licenses/htdemucs.txt`。
