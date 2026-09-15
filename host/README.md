# RaptorQR Host Receiver (Rust + gpui)

屏幕捕获版 RaptorQR 接收端:不再用摄像头,直接从屏幕指定区域读取动画 QR
码流,解码 RaptorQR 传输协议,把文件重建到 `~/Downloads`。

配合 `apps/web` 的发送端(或单文件 HTML 发送端)使用。典型场景:虚拟机
(隔离内网)里的文件 → 屏幕动画 QR → 宿主机捕获还原。

## 构建与运行

```bash
cd host
cargo run --release
```

首次运行会要求 **屏幕录制权限**(macOS: 系统设置 → 隐私与安全性 → 屏幕
录制,勾选你的终端/应用),否则捕获到的画面是黑的。

## 使用

界面就三块:**顶部状态 → 中间当前传输进度 → 下方已接收文件列表**。

1. 打开 App:**所有显示器自动全屏扫描**,不需要任何操作。
2. 发送端播放二维码后,进度条 / 百分比实时更新;多 source block 时下方会多出
   每条 block 的进度条。
3. 传输完成:进度卡变成绿色 `✓ 已接收 <文件>` + 大小 + 路径,同时弹出系统通知,
   文件已在 `~/Downloads`。
4. 想缩小扫描范围时点右上角 **「框选区域」**(这时才会显示屏幕预览,平时不显示):
   用 ◀ ▶ 切到二维码所在的那块屏,拖拽框选,松手即生效;已设区域时右上角多出
   **「清除区域」**。

- **◀ Display ▶**: 切换预览的显示器(框选就在当前预览的那块屏上进行)。
- **Pause / Resume**: 暂停/恢复捕获。
- **Reset region**: 恢复为扫描整块屏(所有显示器全屏扫)。

发送端是**循环播放**的,所以接收端会反复解出同一个传输。接收端按
「同一传输 + 同样内容」去重:只在第一次真正写盘,后续轮次不再重复保存
(`file (1)`、`file (2)` 之类不会堆积),UI 也只记录一次。若 `~/Downloads`
里已存在同名且内容完全相同的文件,接收端会直接认为已完成,不重复写。
相反,内容不同的同名文件仍会另存为 `file (1)`。

状态栏里的 `QR/s` 表示每秒解到的二维码数量:显示 `no QR visible` 就说明
屏幕上那块区域没有可见的二维码(常见原因:发送端窗口被别的窗口挡住,
macOS 会冻结被遮挡窗口的动画)。

## 多显示器

- **默认扫描所有显示器**(每块屏全屏找码),所以 QR 在哪块屏都能收到。
- 每块屏各自节流解码(100 ms/屏),互不抢占。
- 想缩小范围:点 **◀ Display ▶** 把预览切到目标屏,再在预览上拖拽框选;
  选区记录在**哪块显示器 + 该屏像素坐标**上,只有那块屏会用选区裁剪,
  其他屏仍全屏扫。
- 终端自检可以直接看出 QR 在哪块屏:

  ```bash
  "RaptorQR Receiver.app/Contents/MacOS/raptorqr-host" --check
  # displays: 3
  #   [1] Display #41061 — 1710×1107 logical (primary)
  #   [2] Display #65535 — 3440×1440 logical
  # [display 2] ...
  #   QR codes decoded: 4
  ```

  `--display N` 只看第 N 块(1-based),`--region x,y,w,h` 只在该屏内裁一块。

默认捕获 8 fps、每块屏解码间隔 100 ms;QR 解码支持一帧多码(发送端
4 码并行播放也 OK)。

## 架构

```
src/protocol.rs   8 字节传输头 + packed word + CRC32C(移植自 packets/raptorqr-core/src/protocol)
src/raptorq.rs    RaptorQ FEC 解码会话:RFC 6330 block 几何 + raptorq crate + inflate + 文件名前置块
src/transfer.rs   会话跟踪:包头→会话路由→重组→保存
src/qrscan.rs     rxing(ZXing 纯 Rust 移植)QR 解码,输出二进制载荷
src/capture.rs    xcap 屏幕捕获(monitor-local 坐标)
src/ui.rs         gpui UI:预览 + 拖拽选区 + 进度 + 传输记录;后台线程做捕获+解码
```

关键协议事实(与 TS 端一一对应):

- 传输包 = 8 字节头 + 载荷 + CRC32C(小端);`symbolIndex == 31` 表示 RaptorQ 包。
- RaptorQ 载荷 = 4 字节 Payload ID(1 字节 source block 号 + 24 位 ESI,大端)+ `T` 字节符号;
  `T = 传输载荷长度 - 4`,由收到的包长度推断。
- 接收端从包头 `dataLength`(预处理后长度)和 `T` 推出 source block 几何
  (`Z = ceil(Kt / 56403)`),与发送端 wasm wrapper 完全一致。
- 解出后若 `compressed` 标志置位则 raw-deflate 解压;文件模式再剥掉
  `[nameLen][name][mimeLen][mime][data]` 前置块。

## Guest 端:单文件 HTML 发送端

不需要 Node、不需要联网,把 web 发送端打成**一个自包含 HTML**(内联 JS/CSS/两个
WASM codec/全部 worker),拷进隔离虚拟机双击打开即可:

```bash
pnpm guest:build     # 仓库根目录
# 产物:apps/web/dist-single/index.html  (~5.4 MB, 唯一文件)
```

在该页面里选择要传出的文件 → 点 **Start Live QR**,二维码动画就会播放。
（单文件构建会跳过 PWA 预缓存与 Service Worker 注册,因为旁边没有 sw.js/manifest。）

## 完成提示

传输完成后(且只在真正写入新文件时):

- **系统通知**:`RaptorQR — 文件已接收` / `文件名 · 1.4 MB → ~/Downloads/...`,带提示音。
  用的是系统自带的 `osascript display notification`,没有额外依赖。
  想先确认权限是否放行,可以跑一次 `./target/release/raptorqr-host --notify-test`。
  用 `RAPTORQR_NO_NOTIFY=1` 可关闭通知。
- **应用内**:底部绿色横幅 `✓ 已接收 <文件名> · <大小> · <路径>`,历史记录在其下方。

文件名来自发送端,属于不可信输入,拼进 AppleScript 前会转义引号/反斜杠/换行
(`src/notify.rs` 有对应单测),避免脚本注入。

## 诊断开关

| 环境变量 / 参数 | 作用 |
| --- | --- |
| `--check [--region x,y,w,h]` | 抓一帧,报告尺寸/亮度/解到的二维码数量与包头信息;用来确认屏幕录制权限和框选区域 |
| `--notify-test` | 用真实代码路径发一条示例通知,验证系统通知权限 |
| `RAPTORQR_DEBUG=1` | 在 stderr 打印每次解码的区域、解到的二维码数、进度、耗时 |
| `RAPTORQR_FAKE_SCREEN=<文件或目录>` | 用 PNG 帧回放代替真实屏幕抓取(调试/演示用,配合 `scripts/capture_singlefile_frames.mjs` 的产物) |
| `RAPTORQR_NO_NOTIFY=1` | 关闭完成通知 |

## 测试

```bash
cd host
cargo test
```

`tests/roundtrip.rs` 是跨语言集成测试:先用真实发送端 RaptorQ WASM 编码器
生成传输包(`node scripts/encode_fixture.mjs`,需要先 `pnpm install`),
Rust 端解码并逐字节比对。覆盖三个场景:

- 文本 + deflate 压缩
- 文件(文件名/MIME 前置块)+ 不可压缩数据
- 12 MB 多 source block(RFC 6330 分块几何)

`tests/singlefile_e2e.rs` 是**真正的端到端测试**:用无头 Chromium 驱动上面那个
单文件 HTML,把它画到 canvas 上的 QR 帧抓成 PNG,再交给 Rust 侧完整解码:

```bash
pnpm guest:build
cd host && node scripts/capture_singlefile_frames.mjs \
  ../apps/web/dist-single/index.html tests/fixtures/singlefile
cargo test --test singlefile_e2e
```

覆盖链路:单文件 HTML 发送端 → encode worker → RaptorQ WASM → fast_qr canvas
→ PNG → 灰度 → rxing 多码解码 → 传输协议 → RaptorQ FEC → inflate/前置块 → 原始字节。
（唯一没被自动化覆盖的是最后的 OS 屏幕抓取:`xcap` + 屏幕录制权限。）

## 构建说明

- gpui 默认在编译期调用 `xcrun metal` 编译 Metal shader,而只装
  CommandLineTools(没有完整 Xcode)时会失败。因此 `Cargo.toml` 启用了 gpui 的
  `runtime_shaders` feature(shader 改为运行时编译),这样只装 CLT 也能 `cargo run`。
- 首次运行需要在 系统设置 → 隐私与安全性 → 屏幕录制 里授权,否则捕获全黑。

## 已知限制

- 坐标按物理像素;Retina 屏下若发送端按逻辑像素渲染,区域会偏小,可在预览上重新框选。
- `xcap` 走 macOS 旧版 `CGWindowListCreateImage` API;未来可换 ScreenCaptureKit。
