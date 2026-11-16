#!/usr/bin/env bash
# Build the Windows receiver and zip it for handing out:
#   release/RaptorQR-Receiver-windows-x64.zip
#     ├─ raptorqr-host.exe     (64-bit, iced window + --headless console receiver)
#     └─ 使用说明.txt
#
# Cross-compiles from macOS with mingw-w64 (Homebrew's x86_64-w64-mingw32-gcc) —
# no MSVC, no Windows machine needed. The GNU target links the C runtime
# statically enough to be a single self-contained file: only system DLLs.
#
# Usage: host/scripts/package_windows.sh [OUT_DIR]     (default: <repo>/release)
#        CARGO_TARGET_DIR=... host/scripts/package_windows.sh
set -euo pipefail

HOST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REPO_DIR="$(cd "$HOST_DIR/.." && pwd)"
OUT="${1:-$REPO_DIR/release}"
TARGET_DIR="${CARGO_TARGET_DIR:-$HOST_DIR/target}"
TRIPLE=x86_64-pc-windows-gnu
STRIP="$(command -v x86_64-w64-mingw32-strip || true)"

mkdir -p "$OUT"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

echo "==> [1/3] cross-compile for $TRIPLE"
cargo build --manifest-path "$HOST_DIR/Cargo.toml" --release --target "$TRIPLE"

BIN="$TARGET_DIR/$TRIPLE/release/raptorqr-host.exe"
if [ ! -f "$BIN" ]; then
  echo "missing $BIN (is CARGO_TARGET_DIR the one the build just wrote to?)" >&2
  exit 1
fi

echo "==> [2/3] stage the exe"
if [ -n "$STRIP" ]; then
  "$STRIP" --strip-all -o "$STAGE/raptorqr-host.exe" "$BIN"
else
  echo "    warning: x86_64-w64-mingw32-strip not found — shipping unstripped" >&2
  cp "$BIN" "$STAGE/raptorqr-host.exe"
fi
EXE_SHA="$(shasum -a 256 "$STAGE/raptorqr-host.exe" | cut -d' ' -f1)"
EXE_SIZE="$(du -h "$STAGE/raptorqr-host.exe" | cut -f1)"

cat > "$STAGE/使用说明.txt" <<TXT
RaptorQR 接收端 — Windows 版（无需显卡 / 无需 GPU）
==================================================

这是什么
--------
配合 RaptorQR 发送端使用：发送端把文件（或一段文本）编码成满屏滚动的二维码，
这个程序逐帧抓屏解码，把文件还原到「下载」文件夹、把文本直接放进剪贴板。
全程不联网、不需要 GPU：绘制是纯 CPU 光栅，抓屏走 GDI。

快速开始
--------
1. 把 raptorqr-host.exe 拷到任意目录（不需要安装，不需要运行库）
2. 双击运行 —— 会出现一个接收窗口
3. 在另一台机器/虚拟机里用浏览器打开 RaptorQR 发送端，选中要传的文件，
   点 “Start Live QR”，把二维码窗口拖到**本机屏幕可见**的位置
4. 窗口上方出现进度条和 QR/s 计数就说明在收；完成后文件落在「下载」文件夹

首次运行 Windows 可能提示“Windows 已保护你的电脑”（未签名程序）：
点「更多信息」→「仍要运行」。程序不带联网功能，也不写注册表。

界面
----
- 顶部胶囊：捕获中/已暂停、QR/s、fps、当前扫描范围
- 进度卡：当前传输的百分比、子块进度（block x/y）、提示文字
- 下方列表：已接收的文件（点「复制」可把文本再复制一次；文本不落盘）
- 右上角「框选区域」：点开后显示屏幕预览，用 ◀ ▶ 切显示器，
  在画面上**拖拽**框出二维码所在的矩形 → 只扫这一块，CPU 更省
  （选区只作用于那一块屏，其他屏仍全屏扫；点「清除区域」恢复全屏）
- 「暂停/继续」：暂停抓屏解码，窗口本身不关

命令行（可选）
--------------
raptorqr-host.exe                                    窗口版（默认）
raptorqr-host.exe --check                            体检：逐屏尺寸/亮度/解到几个二维码
raptorqr-host.exe --region 300,200,900,700           窗口版，开局就用这个选区
raptorqr-host.exe --display 2                        窗口版，预览默认第 2 块屏
raptorqr-host.exe --headless                         无界面版（控制台里看状态）
raptorqr-host.exe --headless --once                  收到第一个传输就退出（脚本用）
raptorqr-host.exe --headless --region ... --out D:\传入   无界面 + 指定保存目录

  说明：--out / --once 只属于无界面版，带了这两个参数一定走控制台。
  从 cmd/PowerShell 启动时不会隐藏控制台窗口；双击启动时控制台会被自动隐藏。

常见问题
--------
- 窗口全黑 / 一直显示「抓屏全黑」：锁屏、独占全屏的游戏、DRM 视频都会这样；
  普通窗口不会。先 --check 看一眼每块屏的亮度。
- 收不到：确认发送端窗口在本机屏幕上是**可见**的（被别的窗口完全挡住时，
  系统会冻结它的动画）；二维码太大/太小都不利于解码，发送端别缩放窗口。
- 同一个文件只会保存一次（发送端是循环播放的），不会堆出 “文件 (1).exe”。
- 本版本**没有系统通知**：收到东西看窗口里的卡片和列表。
- 文本传输不落盘：直接进剪贴板，窗口里也能看到全文。

校验
----
raptorqr-host.exe
  sha256 $EXE_SHA
  大小   $EXE_SIZE
TXT

echo "==> [3/3] zip"
ZIP="$OUT/RaptorQR-Receiver-windows-x64.zip"
rm -f "$ZIP"
(cd "$STAGE" && zip -q -X "$ZIP" raptorqr-host.exe "使用说明.txt")
(cd "$OUT" && shasum -a 256 "$(basename "$ZIP")" > "$(basename "$ZIP").sha256")

echo
ls -lh "$ZIP" "$STAGE/raptorqr-host.exe" | sed 's/^/    /'
echo "    exe sha256: $EXE_SHA"
cat "$ZIP.sha256" | sed 's/^/    zip sha256: /'
