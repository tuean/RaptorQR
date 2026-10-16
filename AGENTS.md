# RaptorQR（个人 fork）— agent 约定

先读 [README.md](README.md)（本 fork 改了什么 + 用法）和 [host/README.md](host/README.md)
（协议映射、诊断开关、已知限制）。

## 铁律：改完 `host/` 就更新应用

用户双击运行的是 **`/Applications/RaptorQR Receiver.app`**，它不会自己跟着源码变。
任何对 `host/` 的代码改动做完之后，**必须**：

```bash
pnpm host:test      # 1. 测试全绿
pnpm host:install   # 2. 构建 release（universal + 签名）→ 覆盖 /Applications
```

- 只改不装 = 用户跑的还是旧二进制，等于没改。用户报「还是不行 / 还是收不到」时，
  第一件事就是看 `/Applications` 里那份的 `built at` / `Signed Time` 是不是刚刚。
- `pnpm host:install` 末尾会打印 `arch` / `built at` / `sha256`。`built at` 是「装的是不是
  刚编的」的判据；`sha256` 只用来核 `/Applications` 与 `release/` 是同一份文件（签名带
  时间戳，重签一次哈希就会变，不能拿它跟上一版比较）。
- 改完抓取/解码/UI，顺手在真机上验一次：`"/Applications/RaptorQR Receiver.app/Contents/MacOS/raptorqr-host" --check`
  （报告每块屏的尺寸/亮度/解到几个二维码；全黑 = 缺屏幕录制权限）。
- 签名用固定的本地证书（`RaptorQR Local Signing` / `TVA Local Signing`），重装不会重置
  屏幕录制授权。
- 只改 `apps/web`（发送端）时不需要重装接收端：产物走 `pnpm guest:build`
  （→ `apps/web/dist-single/index.html`）或 `pnpm release`（→ `release/`）。
- 顺序：要出完整产物就先 `pnpm release`，**再** `pnpm host:install`（install 会重打 zip），
  这样 `release/`、zip、`/Applications` 三份 sha256 一致；反了顺序 zip 会比已安装的那份
  少一次签名。

## 其它约定

- 提交前：`host/` 改动跑 `pnpm host:test`；TS 侧改动跑 `pnpm test`。
- **文本不落盘**：文本传输走剪贴板 + 界面显示，只有一个落盘入口
  `transfer::save_completed`（文本返回 `None`），不要绕过它直接写文件。
  剪贴板：UI 用 gpui，无界面用 `clipboard::copy_text`（macOS 必须带 `LC_CTYPE=UTF-8`，
  否则 `pbcopy` 在 `C` locale 下会静默丢掉中文）。
- 接收端看到的文件名、MIME、文本都来自发送端，属于**不可信输入**：落盘、拼路径、拼
  AppleScript 之前必须校验/转义（见 `host/src/transfer.rs` 的 `sanitize_filename`、
  `host/src/notify.rs`）。
- 后台解码线程没有 panic 兜底：热路径里不要用 `unwrap()/expect()`，一次 panic 会让接收端
  静默失去接收能力（历史上出过：文本被当成文件名 → ENAMETOOLONG → 线程死）。
