# RaptorQR（个人 fork）

本项目是 [infrost/RaptorQR](https://github.com/infrost/RaptorQR) 的 fork。上游是「用动画二维码在两台设备间传文件/文本」的工具（浏览器发送端 + 摄像头接收端 + 终端 CLI，RaptorQ 喷泉码），**上游的项目介绍、性能数据、npm 包、CLI 用法、架构说明请看[上游 README](https://github.com/infrost/RaptorQR#readme)**，本文件只记录本 fork 改了什么。

## 本 fork 的改动

1. **新增 `host/`：屏幕捕获接收端（Rust）**
   不用摄像头，直接扫屏幕上一块区域的二维码流，用来做「隔离虚拟机 ↔ 宿主机」这种没网络、没共享剪贴板的场景。**macOS/Linux 是 gpui 窗口版**（实时预览、拖拽框选、多显示器自动扫描、系统通知）；**Windows 是无界面控制台版**（`--display` / `--region` / `--out` / `--once` / `--check`），两者共用同一套捕获-扫描-解码-交付链路。**收到的文件写 `~/Downloads`；收到的文本不落盘，直接进系统剪贴板并在界面里显示全文**（发送端 Text 模式）。协议映射、测试和已知限制见 [host/README.md](host/README.md)。

2. **新增单文件发送端构建 `apps/web/vite.config.single.ts`**
   `pnpm guest:build` → `apps/web/dist-single/index.html`，自包含、离线可用，拷进虚拟机双击就能当发送端。

3. **新增发布脚本 `scripts/package_release.sh`**
   `pnpm release` 一条命令产出 `release/`：单文件 HTML + macOS 接收端 `.app`/zip + 中文使用说明。

4. **macOS 端打 universal 二进制**（Apple Silicon + Intel，macOS 13+）。

5. **GitHub Actions 产出 Windows 接收端**（`.github/workflows/windows-host.yml`：在 `windows-latest` 上跑 `cargo test` 并上传 `raptorqr-host.exe` 产物）。

6. **重写 README**（即本文件）。

7. **新增 `host/scripts/install_macos_app.sh`（`pnpm host:install`）**
   构建最新接收端并安装到 `/Applications`。**改完 `host/` 的代码必须跑一次**，否则双击打开的
   `/Applications/RaptorQR Receiver.app` 还是旧版（旧 bug 也一起留着）。

## ⚠️ 改完代码必做：更新应用

`/Applications/RaptorQR Receiver.app` 才是实际双击运行的那一份，它不会自己跟着源码变。
**每次改动 `host/` 之后，都要执行**：

```bash
pnpm host:test         # 1. 测试全绿
pnpm host:install      # 2. = host/scripts/install_macos_app.sh
# → 构建 release（universal + 本地证书签名）
# → 刷新 release/RaptorQR-Receiver-macos.zip
# → 退出正在运行的接收端 → 覆盖 /Applications/RaptorQR Receiver.app → 校验签名
# → 打印 arch / built at / sha256
```

要出完整产物（发送端单文件 HTML 等）就先 `pnpm release`，**再**跑 `pnpm host:install`：
install 会把 zip 一起重打，保证 `release/`、zip、`/Applications` 三份完全一致。

判断有没有生效，看输出的 **built at**（应为刚刚）和 **Signed Time**：

```bash
codesign -dvvv "/Applications/RaptorQR Receiver.app" 2>&1 | grep "Signed Time"
```

**sha256** 用来核对上面那三份是同一个文件；别拿它跟上一版比 —— 代码签名带时间戳，
同样的源码重新签一次哈希就会变（所以脚本用 built at 而不是哈希来判断新旧）。

签名用的是固定的本地证书（`RaptorQR Local Signing` / `TVA Local Signing`），所以重新安装
**不会**再要一次屏幕录制权限。

## 用法

```bash
pnpm install
pnpm guest:build   # 虚拟机端：单文件发送页面
pnpm host:run      # 宿主机端（macOS）：屏幕捕获接收，首次需授予「屏幕录制」权限
pnpm host:test     # 接收端 Rust 测试（host/ 改完先跑这个）
pnpm host:install  # 构建最新接收端并装进 /Applications（改完 host/ 必做）
pnpm release       # 出全部发布产物 → release/（单文件 HTML + .app/zip + 使用说明）

# Windows 接收端（在 Windows 上，或下载 Actions 产物）
cargo build --manifest-path host/Cargo.toml --release   # → raptorqr-host.exe（无界面，参数见 host/README.md）
```

## 链接

- 原项目：https://github.com/infrost/RaptorQR
- 本 fork：https://github.com/tuean/RaptorQR
- 预编译产物：[Releases](https://github.com/tuean/RaptorQR/releases/latest) —— 发送端单文件 HTML、macOS 接收端 `.app`、**Windows 接收端 `raptorqr-host.exe`**
- 许可证：MIT（© Haixiang，见 [LICENSE](LICENSE)）
