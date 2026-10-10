# iRec（macOS 屏幕录制工具 · Rust）

macOS 菜单栏常驻的屏幕录制工具：全局快捷键一键唤起，支持全屏 / 选区录制，
带系统声音（可关闭），输出 HEVC + AAC 的 `.mp4`，默认保存到桌面。

## 功能

| 能力 | 说明 |
| --- | --- |
| 一键唤起 | `⇧⌘R` 全屏录制 / 停止；`⇧⌘E` 框选区域录制 / 停止 |
| 全屏 or 局部 | 点菜单栏图标可手动选择；区域为拖拽框选，按任意键取消 |
| 选区框常驻 | 框选后选区框一直留在屏幕上，可拖动 / 改大小，框上控制条直接开始或停止录制 |
| 声音 | 默认录制系统声音（AAC 128 kbps），可在菜单里关闭 |
| 格式 | MP4（HEVC + AAC），文件名 `iRec-年月日-时分秒.mp4` |
| 保存位置 | 默认 `~/Desktop` |
| 体积 | HEVC 硬件编码 + 30 fps + YUV420 采样，可调画质/帧率/分辨率 |

实测（MacBook 内建屏 1728×1117，30 fps，默认“均衡”画质，录制 10 秒）：

| 设置 | 分辨率 | 10 秒体积 | 折合每分钟 |
| --- | --- | --- | --- |
| 2x（默认高清） | 3456×2234 | 3.49 MB | ≈ 21 MB |
| 1x（省空间） | 1728×1116 | 0.96 MB | ≈ 5.8 MB |

## 使用

1. 从「应用程序」或 Launchpad 启动 iRec（`cp -R outputs/iRec.app /Applications/`）。
   启动后会出现：
   - **Dock 图标**（`resources/icon.svg` → `resources/AppIcon.icns`，可以随时
     退出/重启，⌘Q 也可以，关闭窗口即退出）；
   - **控制窗口**：开始录制（全屏）/ 选择区域录制 / 停止录制、系统声音开关、
     画质 / 帧率 / 分辨率下拉框、实时状态（“● 录制中 00:12”“已保存 …”）、
     在访达中显示、退出。没在录制时「停止录制」是灰色的，录制中「开始录制（全屏）」
     和「选择区域录制」变灰，只有「停止录制」可点；
   - **菜单栏图标**（可选）：空心圆点 + `REC`，录制中变红点。
   首次使用需要“屏幕录制”权限：
   系统设置 → 隐私与安全性 → 屏幕录制 → 勾选 iRec，然后**重启 iRec**。
   应用不在启动时自动申请权限（macOS 15 的 `CGPreflightScreenCaptureAccess()`
   会误报未授权，自动申请会导致系统反复弹窗），只有在真正采集失败时才申请一次。
   想确认状态就点窗口里的「检查屏幕录制权限」。
   应用用稳定的开发者证书签名（`scripts/build_app.sh`），重新编译后权限不会失效；
   诊断日志在 `~/Library/Logs/irec.log`。

   排错：如果一直提示没权限，① 完全退出 iRec；② 系统设置 → 隐私与安全性 →
   屏幕录制，把 iRec 关掉再打开（列表里若有多条就都打开，没有就点 "+" 添加
   `/Applications/iRec.app`）；③ 重新打开 iRec，点窗口里的
   「申请 / 检查屏幕录制权限」看结论。临时兜底：在终端里跑
   `/Applications/iRec.app/Contents/MacOS/iRec &`
   （终端已有权限时可直接录制，窗口与快捷键同样可用）。
2. 菜单栏出现 `REC` 图标：
   - `● 00:12` 表示正在录制；结束后短暂显示 `✓ 12.3 MB`。
3. 快捷键：
   - `⇧⌘R`：开始 / 停止全屏录制
   - `⇧⌘E`：打开区域框选（框选时按任意键取消）；录制中再按一次停止
4. 选区录制流程：
   - `⇧⌘E` 或点「选择区域录制」→ 拖拽框出要录的范围（按任意键取消）；
   - 松手后**选区框留在原地**，上方出现控制条：`宽 × 高 · 计时`、`开始录制`、
     `关闭`；
   - 拖动控制条可整体移动选区，拖右下角圆点可改大小；
   - 点控制条上的 `开始录制` 开始录制，框变红并显示计时，按钮变成 `停止录制`；
   - 点 `停止录制` 结束，选区框保留在原处，方便接着录同一块区域。
   录制中移动 / 改大小不会改变正在录的范围（视频不会跳），新区域在**下一次**
   录制时生效，控制条会提示“新区域下次生效”。

   说明：选区框和控制窗口都设了 `NSWindowSharingType::None`，不会被录进视频。
4. 菜单项：画质（更小体积 / 均衡 / 高清 / 极清）、帧率（30 / 60）、
   分辨率（1x / 2x）、录制声音开关、在访达中显示最新录制、退出。

## 编译

```sh
cd iRec
./scripts/build_app.sh        # 编译 release 并生成 ../outputs/iRec.app
cargo build --release         # 只编译二进制
```

自测（无需界面，录 N 秒后打印结果）：

```sh
cargo run --release -- --selftest --seconds 5                       # 全屏
cargo run --release -- --selftest --seconds 5 --region 200,300,900,600   # 区域（Cocoa 坐标，y 从屏幕底部算）
cargo run --release -- --selftest --seconds 5 --no-audio --scale 1
```

设置 `SCREEN_REC_DEBUG=1` 可在 stderr 看到采集/编码的调试信息。

## 实现要点

- `src/recorder.rs`：ScreenCaptureKit 采集 + `AVAssetWriter`（HEVC/AAC）写 MP4。
- `src/frame.rs`：把每帧拷贝到自己的 `CVPixelBufferPool` 缓冲并按区域裁剪。
  直接把 ScreenCaptureKit 的画面交给 `AVAssetWriter` 在“录子区域”时会失败
  （AVFoundationErrorDomain -11800 / OSStatus -16122），所以做法是：
  始终采集整块屏幕，再在拷贝时裁剪出选区。
- `src/hotkey.rs`：Carbon `RegisterEventHotKey`，不需要辅助功能/输入监控权限。
- `src/overlay.rs`：全屏透明 NSWindow + 自绘 NSView 做拖拽框选。
- `src/state.rs`：全局设置与命令队列（快捷键/菜单/框选产生的动作统一在
  主线程定时器里执行）。

## 稳定性设计

- **分片写入**：每 5 秒写一个 movie fragment，即使应用被强退/崩溃，
  文件仍带着 moov + 分片，可以正常打开（不会只剩一个打不开的空壳）。
- **收尾不阻塞主线程**：停止录制时在后台线程等待 AVAssetWriter 收尾，
  主线程继续跑 run loop，避免卡住导致文件永远不封口。
- **退出即收尾**：点退出或应用被终止时，会先把正在录的文件写完再退出。
- **编码失败早停**：编码器一旦报错就立刻结束录制并提示，而不是继续写坏文件。

## 已知限制

- 只录制主显示器；多屏场景下录主屏。
- 录制的是系统声音（应用播放的声音），不是麦克风。
- 快捷键写死为 `⇧⌘R` / `⇧⌘E`（改 `src/hotkey.rs` 里的 `register_defaults`）。

## 图标

`resources/icon.svg` 是唯一源文件（深色圆角方块 + 屏幕面板 + 选区四角 + 红色录制点），
`resources/AppIcon.icns` 由它生成并被 `scripts/build_app.sh` 打进 app。改完 SVG 重新生成：

用 Chrome 无头模式渲染才能保留透明背景（`qlmanage` 会垫一层白底）：

```sh
mkdir -p /tmp/icon.iconset
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" --headless=new \
  --disable-gpu --hide-scrollbars --force-device-scale-factor=1 \
  --window-size=1024,1024 --default-background-color=00000000 \
  --screenshot=/tmp/icon1024.png "file://$PWD/resources/icon.svg"
# 再用 PIL / sips 缩成 iconset 的 10 个尺寸，最后：
iconutil -c icns /tmp/icon.iconset -o resources/AppIcon.icns
```
