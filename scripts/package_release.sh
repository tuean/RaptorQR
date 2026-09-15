#!/usr/bin/env bash
# Build both deliverables into ./release/:
#   1. RaptorQR-Sender.html     — single-file guest sender (copy into the VM)
#   2. RaptorQR Receiver.app    — macOS receiver (double-click to run)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/release"
mkdir -p "$OUT"

echo "==> [1/2] guest sender: single-file HTML"
pnpm --filter @raptorqr/web build:single
cp apps/web/dist-single/index.html "$OUT/RaptorQR-Sender.html"

echo "==> [2/2] host receiver: macOS app bundle"
"$ROOT/host/scripts/package_macos_app.sh" "$OUT" >/dev/null

SENDER_SIZE="$(du -h "$OUT/RaptorQR-Sender.html" | cut -f1)"
cat > "$OUT/使用说明.txt" <<TXT
RaptorQR — 屏幕二维码传文件（宿主机 ⇄ 隔离虚拟机）
=================================================

两个产物
--------
1. RaptorQR-Sender.html        发送端（放进虚拟机里用）
   - 单文件、离线可用，无需网络、无需安装任何东西
   - 用法：在虚拟机里双击/用浏览器打开 → 选择要传出的文件 → 点 “Start Live QR”
   - 建议把浏览器窗口调到最前，二维码区域完整可见（被其他窗口挡住时系统会冻结动画）
   - 文件大小：$SENDER_SIZE

2. RaptorQR Receiver.app       接收端（宿主机上用，macOS 13+）
   - 用法：双击打开 → 首次会要求「屏幕录制」权限（系统设置 → 隐私与安全性 →
     屏幕录制，勾选 RaptorQR Receiver；授权后需完全退出 App 再重开）
   - 权限只需给一次：App 用固定证书签名（RaptorQR Local Signing / TVA Local Singing），
     所以重新打包也不会再重复要权限
   - 界面三块：顶部状态芯片 → 中间当前传输进度 → 下方已接收文件列表
   - 默认自动扫描所有显示器（含外接屏），二维码在哪块屏都能收到
   - 想缩小范围：点右上角「框选区域」（这时才显示屏幕预览）→ 用 ◀ ▶ 切到目标屏
     → 拖拽框选；扫完点「清除区域」恢复正常全屏扫描
   - 进度条完成后文件在 ~/Downloads，同时弹系统通知
   - 传输完成后会自动保存到 ~/Downloads，并弹出系统通知 + 应用内绿色提示
   - 发送端是循环播放的：同一个文件只会保存一次，不会重复堆副本
   - 每次收到的文件名与发送端一致；重名但内容不同会另存为 “文件名 (1).扩展名”

排查
----
- 接收端状态栏若显示 “no QR visible”，说明屏幕上没有可见二维码：
  把发送端窗口置前，或先切到发送端所在的那块屏看看
- 想知道 QR 到底在哪块屏，用 --check 会逐屏报告解到几个码
- 终端里可以自检：
    "RaptorQR Receiver.app/Contents/MacOS/raptorqr-host" --check
  会逐块显示器打印逻辑尺寸、画面亮度、解到几个二维码
  （0 个且画面全黑 = 缺少屏幕录制权限）；--display N 只看第 N 块屏
- 关闭完成通知：环境变量 RAPTORQR_NO_NOTIFY=1
TXT

echo
echo "release/ contents:"
ls -lh "$OUT" | tail -n +2
