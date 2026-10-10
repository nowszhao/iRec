# iRec

macOS 菜单栏常驻的屏幕录制工具，Rust 编写。全屏或选区录制，可带系统声音，
输出 HEVC + AAC 的 `.mp4`，默认保存到桌面。

[English](./README.en.md) | 简体中文

## 目录

- [特性](#特性)
- [系统要求](#系统要求)
- [安装与构建](#安装与构建)
- [使用](#使用)
  - [控制窗口](#控制窗口)
  - [全局快捷键](#全局快捷键)
  - [选区录制](#选区录制)
  - [菜单栏与设置项](#菜单栏与设置项)
- [输出规格与实测体积](#输出规格与实测体积)
- [权限与排错](#权限与排错)
- [命令行自测](#命令行自测)
- [实现要点](#实现要点)
- [稳定性设计](#稳定性设计)
- [已知限制](#已知限制)
- [应用图标](#应用图标)

## 特性

| 能力 | 说明 |
| --- | --- |
| 一键唤起 | `⇧⌘R` 全屏录制 / 停止；`⇧⌘E` 进入区域框选，录制中再按一次停止 |
| 全屏 / 选区 | 区域为拖拽框选，框选过程中按任意键取消 |
| 选区框常驻 | 框选结束后选区框留在屏幕上，可拖动、可改大小，控制条上直接开始或停止录制 |
| 静默常驻 | 启动后只在菜单栏显示（无 Dock 图标、不弹窗口），全部操作在菜单栏菜单中完成 |
| 状态即按钮状态 | 未录制时「停止录制」置灰；录制中「开始录制（全屏）」「选择区域录制」置灰 |
| 自身不入镜 | 控制窗口与选区框设为 `NSWindowSharingType::None`，不会被录进视频 |
| 声音 | 四档可选：关闭 / 系统声音 / 麦克风 / 系统声音 + 麦克风（混合为一条音轨） |
| 编码 | HEVC 硬件编码 + YUV420 采样，画质 / 帧率 / 分辨率可调 |
| 输出 | MP4（HEVC + AAC），文件名 `iRec-年月日-时分秒.mp4`，默认 `~/Desktop` |

## 系统要求

- macOS 13.0 及以上（`LSMinimumSystemVersion = 13.0`）
- 屏幕录制权限（首次实际采集时申请，见[权限与排错](#权限与排错)）
- 构建：Rust 工具链（edition 2024），Xcode 命令行工具

## 安装与构建

```sh
git clone ssh://git@ssh.github.com:443/nowszhao/iRec.git
cd iRec

./scripts/build_app.sh              # 编译 release 并打包为 target/release/iRec.app
cargo build --release               # 仅编译二进制（target/release/iRec）
```

打包脚本默认做 ad-hoc 签名；如需固定证书（重新编译后仍保留系统已授予的录制权限），
指定签名身份：

```sh
SIGN_IDENTITY="Apple Development: Your Name (TEAMID)" ./scripts/build_app.sh
```

安装到应用程序目录：

```sh
cp -R target/release/iRec.app /Applications/
```

## 使用

iRec 是菜单栏应用：启动后**静默常驻**，不显示 Dock 图标，也不自动弹出窗口。
点击菜单栏图标（空心圆点 + `REC`，录制中变红点并显示 `● 00:12`，结束后短暂显示
`✓ 12.3 MB`）打开菜单，录制、画质、权限检查、退出等全部操作都在这里完成。
需要可视化操作时，点菜单里的「打开控制窗口…」唤出控制窗口；关闭窗口只是隐藏它，
不会退出应用（退出请用菜单里的「退出」）。

### 控制窗口

默认不显示，从菜单栏菜单的「打开控制窗口…」唤出；关闭窗口只隐藏不退出。


| 控件 | 行为 |
| --- | --- |
| 开始录制（全屏） | 开始录制；录制中置灰并显示「录制中…」 |
| 选择区域录制 | 进入全屏框选；录制中置灰 |
| 停止录制 | 仅在录制中可点；其余时间置灰 |
| 声音 | 关闭 / 系统声音 / 麦克风 / 系统声音 + 麦克风 |
| 画质 / 帧率 / 分辨率 | 更小体积 · 均衡 · 高清 · 极清 / 30 · 60 fps / 1x · 2x |
| 在访达中显示 | 在 Finder 中选中最近一次录制 |
| 申请 / 检查屏幕录制权限 | 主动触发系统授权并给出结论 |

窗口底部实时显示状态：`就绪`、`● 录制中 00:12`、`正在保存…`、`已保存 xxx（12.3 MB）`。

### 全局快捷键

| 快捷键 | 作用 |
| --- | --- |
| `⇧⌘R` | 开始 / 停止全屏录制 |
| `⇧⌘E` | 进入区域框选；录制中按一次停止当前录制 |
| `⌘Q` | 退出（控制窗口激活时可用；退出前会先写完当前文件） |

### 选区录制

1. `⇧⌘E` 或点「选择区域录制」，拖拽框出目标范围（框选时按任意键取消）。
2. 松开鼠标后**选区框留在原地**，上方出现控制条：`宽 × 高 · 计时`、`开始录制`、`关闭`。
3. 拖动控制条整体移动选区，拖动右下角圆点调整大小。
4. 点控制条上的「开始录制」开始：框变红、控制条显示计时、按钮变为「停止录制」。
5. 点「停止录制」结束；选区框保留在原处，便于对同一区域连续录制。

录制中移动或改大小不会改变当前正在录制的范围（避免视频跳变），新区域在**下一次**
录制时生效，控制条会提示「新区域下次生效」。

### 菜单栏与设置项

菜单依次提供：开始录制（全屏）、选择区域录制、停止录制、声音（四档子菜单）、
画质（更小体积 / 均衡 / 高清 / 极清）、帧率（30 / 60）、分辨率（1x / 2x）、
在访达中显示最新录制、打开控制窗口…、申请 / 检查屏幕录制权限、退出。
其中「停止录制」仅在录制中可点，其余时间置灰。菜单项与控制窗口共享同一份设置。

## 输出规格与实测体积

| 项目 | 值 |
| --- | --- |
| 容器 / 编码 | MP4，视频 HEVC（硬件编码），音频 AAC 128 kbps / 48 kHz 立体声 |
| 采样 | YUV420（`420v`，NV12） |
| 默认参数 | 均衡画质、30 fps、2x（Retina 像素）、开启系统声音 |
| 文件命名 | `iRec-年月日-时分秒.mp4`，重名自动加序号 |
| 保存位置 | `~/Desktop`（`Settings::out_dir`） |
| 日志 | `~/Library/Logs/irec.log` |

实测（MacBook 内建屏 1728×1117，30 fps，均衡画质，录制 10 秒）：

| 分辨率设置 | 实际分辨率 | 10 秒体积 | 折合每分钟 |
| --- | --- | --- | --- |
| 2x（默认高清） | 3456×2234 | 3.49 MB | ≈ 21 MB |
| 1x（省空间） | 1728×1116 | 0.96 MB | ≈ 5.8 MB |

## 声音

| 选项 | 说明 |
| --- | --- |
| 关闭 | 只录画面，不写音轨 |
| 系统声音 | 录制各应用播放的声音 |
| 麦克风 | 只录麦克风 |
| 系统声音 + 麦克风 | 两路 PCM 换算到 48 kHz 立体声后相加，混合成一条音轨 |

选择含麦克风的档位时会立即向系统申请麦克风权限（首次会弹系统授权框）。
麦克风采样由 ScreenCaptureKit 的 `SCStreamOutputType::Microphone` 提供，格式为设备
原生 PCM（实测 48 kHz / 单声道 / float32），在 `src/audio.rs` 里重采样、补声道并混入
系统声音的采样缓冲。仅录麦克风时，系统声音的采样会先被静音覆盖再混音。

## 权限与排错

首次实际采集时才会向系统申请「屏幕录制」权限——macOS 15 的
`CGPreflightScreenCaptureAccess()` 会误报未授权，启动时自动申请会导致系统反复弹窗。
可随时点控制窗口里的「申请 / 检查屏幕录制权限」确认状态。

若始终提示无权限：

1. 完全退出 iRec（⌘Q 或点「退出」）。
2. 系统设置 → 隐私与安全性 → 屏幕录制：关闭再打开 iRec（列表中有多条就全部打开；
   没有则点 `+` 添加 `/Applications/iRec.app`）。
3. 重新打开 iRec，点「申请 / 检查屏幕录制权限」查看结论。
4. 临时兜底：终端执行 `/Applications/iRec.app/Contents/MacOS/iRec &`
   （终端已获授权时可直接录制，窗口与快捷键同样可用）。

签名变更或 bundle id 变更（`com.nows.irec`）后，系统会把它当成新应用，需要重新授权一次。
麦克风同理：系统设置 → 隐私与安全性 → 麦克风，允许 iRec 后重启一次。

## 命令行自测

无需界面即可验证采集与封装：

```sh
cargo run --release -- --selftest --seconds 5                             # 全屏
cargo run --release -- --selftest --seconds 5 --region 200,300,900,600    # 区域（Cocoa 坐标，y 自屏幕底部起算）
cargo run --release -- --selftest --seconds 5 --no-audio --scale 1        # 关闭声音、1x
cargo run --release -- --selftest --seconds 5 --audio mic                 # 只录麦克风
cargo run --release -- --selftest --seconds 5 --audio both                # 系统声音 + 麦克风
```

可用参数：`--seconds`、`--region x,y,w,h`、`--fps`、`--scale`、`--out <dir>`、
`--audio off|system|mic|both`（`--no-audio` 等价于 `off`）。
设置 `SCREEN_REC_DEBUG=1` 可在 stderr 输出采集与编码的调试信息。

## 实现要点

| 模块 | 职责 |
| --- | --- |
| `src/recorder.rs` | ScreenCaptureKit 采集 + `AVAssetWriter`（HEVC/AAC）封装 MP4 |
| `src/frame.rs` | 帧拷贝到自有 `CVPixelBufferPool` 并按选区裁剪 |
| `src/hotkey.rs` | Carbon `RegisterEventHotKey`，无需辅助功能 / 输入监控权限 |
| `src/overlay.rs` | 全屏透明 `NSWindow` + 自绘 `NSView` 实现拖拽框选 |
| `src/hud.rs` | 常驻选区框：描边窗口、控制条、缩放把手三个窗口协同 |
| `src/audio.rs` | PCM 读取、重采样与混音（麦克风混入系统声音） |
| `src/state.rs` | 全局设置与命令队列（快捷键 / 菜单 / 框选动作统一在主线程定时器执行） |

区域录制采用「整屏采集 + 拷贝时裁剪」，而不是直接把 ScreenCaptureKit 的子区域画面
交给 `AVAssetWriter`——后者会失败（`AVFoundationErrorDomain -11800` / `OSStatus -16122`）。

选区框、控制窗口均设置 `NSWindowSharingType::None`，同时采集时用
`SCContentFilter` 排除本进程窗口，确保界面元素不会出现在录制结果中。

## 稳定性设计

- **分片写入**：每 5 秒写入一个 movie fragment。应用被强退或崩溃时，文件仍含 moov
  与已写入分片，可以正常打开，而不是留下无法播放的空壳。
- **收尾不阻塞主线程**：停止录制时在后台线程等待 `AVAssetWriter` 封口，主线程继续
  运行 run loop，避免界面卡住导致文件永远不封口。
- **退出即收尾**：退出或被终止前，先把正在录制的文件写完。
- **编码失败早停**：编码器报错立即结束录制并提示，而不是继续写出损坏文件。

## 已知限制

- 仅录制主显示器。
- 录制的是系统声音（各应用播放的音频），不含麦克风输入。
- 快捷键固定为 `⇧⌘R` / `⇧⌘E`（见 `src/hotkey.rs` 的 `register_defaults`）。
- 录制中调整选区不会改变当前视频的取景范围。

## 应用图标

`resources/icon.svg` 是唯一源文件（深色圆角方块 + 屏幕面板 + 选区四角 + 红色录制点），
`resources/AppIcon.icns` 由它生成，并由 `scripts/build_app.sh` 打进 app。
重新生成时须用 Chrome 无头模式渲染，`qlmanage` 会垫一层白底：

```sh
mkdir -p /tmp/icon.iconset
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new \
  --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1024,1024 --default-background-color=00000000 \
  --screenshot=/tmp/icon1024.png "file://$PWD/resources/icon.svg"
# 再用 PIL / sips 生成 iconset 所需的 10 个尺寸，最后：
iconutil -c icns /tmp/icon.iconset -o resources/AppIcon.icns
```
