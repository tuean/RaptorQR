# RaptorQR（个人 fork）

本项目是 [infrost/RaptorQR](https://github.com/infrost/RaptorQR) 的 fork。上游是「用动画二维码在两台设备间传文件/文本」的工具（浏览器发送端 + 摄像头接收端 + 终端 CLI，RaptorQ 喷泉码），**上游的项目介绍、性能数据、npm 包、CLI 用法、架构说明请看[上游 README](https://github.com/infrost/RaptorQR#readme)**，本文件只记录本 fork 改了什么。

## 本 fork 的改动

1. **新增 `host/`：屏幕捕获接收端（Rust）**
   不用摄像头，直接扫屏幕上一块区域的二维码流，用来做「隔离虚拟机 ↔ 宿主机」这种没网络、没共享剪贴板的场景。**macOS/Linux 是 gpui 窗口版**（实时预览、拖拽框选、多显示器自动扫描、系统通知）；**Windows 是无界面控制台版**（`--display` / `--region` / `--out` / `--once` / `--check`），两者共用同一套捕获-扫描-解码-保存链路。协议映射、测试和已知限制见 [host/README.md](host/README.md)。

2. **新增单文件发送端构建 `apps/web/vite.config.single.ts`**
   `pnpm guest:build` → `apps/web/dist-single/index.html`，自包含、离线可用，拷进虚拟机双击就能当发送端。

3. **新增发布脚本 `scripts/package_release.sh`**
   `pnpm release` 一条命令产出 `release/`：单文件 HTML + macOS 接收端 `.app`/zip + 中文使用说明。

4. **macOS 端打 universal 二进制**（Apple Silicon + Intel，macOS 13+）。

5. **GitHub Actions 产出 Windows 接收端**（`.github/workflows/windows-host.yml`：在 `windows-latest` 上跑 `cargo test` 并上传 `raptorqr-host.exe` 产物）。

6. **重写 README**（即本文件）。

## 用法

```bash
pnpm install
pnpm guest:build   # 虚拟机端：单文件发送页面
pnpm host:run      # 宿主机端（macOS）：屏幕捕获接收，首次需授予「屏幕录制」权限
pnpm release       # 出全部发布产物 → release/

# Windows 接收端（在 Windows 上，或下载 Actions 产物）
cargo build --manifest-path host/Cargo.toml --release   # → raptorqr-host.exe（无界面，参数见 host/README.md）
```

## 链接

- 原项目：https://github.com/infrost/RaptorQR
- 本 fork：https://github.com/tuean/RaptorQR
- 预编译产物：[Releases](https://github.com/tuean/RaptorQR/releases/latest) —— 发送端单文件 HTML、macOS 接收端 `.app`、**Windows 接收端 `raptorqr-host.exe`**
- 许可证：MIT（© Haixiang，见 [LICENSE](LICENSE)）
