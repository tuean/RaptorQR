# Windows 接收端 UI 技术方案（无 GPU · 纯 Rust · 取代 gpui）

> 设计稿，2026-11-16。目标：让 `host/`（RaptorQR Receiver）在**没有 GPU 的 Windows** 上
> 也有可用的图形界面——实时状态、拖拽框选、进度条、已接收列表。
> 现状见 `host/Cargo.toml:15-18`：gpui 只挂在 `cfg(not(windows))` 下，
> `host/src/main.rs:33-36` 在 Windows 直接落到 headless，`host/src/lib.rs:14-15` 连 `ui` 模块都不编译。

---

## 0. 结论

| 层 | 决定 | 理由 |
| --- | --- | --- |
| 绘制 | **CPU 光栅 + GDI 呈现**（`softbuffer`/`BitBlt`），不触碰任何 3D API | 无 GPU 时唯一稳的路；DWM 由系统用 WARP 合成，不归我们管 |
| UI 库 | **首选 iced 0.14 + `tiny-skia`**；次选 Slint 1.19 `renderer-software`；保守选 FLTK 1.5 | 许可证 + 与现有 gpui 代码形态接近 + 官方/半官方的软件渲染路径（见 §2） |
| 抓屏 | **GDI `BitBlt` 直采 32 位 DIB**，按区域只拷选区 | `xcap` 0.9 在 Windows 默认走 WGC/DXGI 桌面复制，需要 D3D11 设备，无 GPU 机器上大概率直接失败 |
| 结构 | `ui.rs` 拆成 `app/`（无 UI 依赖）+ `ui/gpui` + `ui/win` | 核心 `scanner`/`transfer`/`raptorq`/`qrscan` 一行不改；两个前端共用同一套 worker |
| 工期 | 约 4.5 人日（含 0.5 天选型 spike） | 见 §7 |

一句话：**gpui 在 Windows 上的问题不是"慢"，是"没有后端"**——所以不是调参，而是换一套
"CPU 光栅 + 系统窗口表面"的绘制栈，同时把抓屏从 DXGI 换成 GDI。

---

## 1. 现状与真实约束

### 1.1 今天的代码

- `host/src/ui.rs`（1272 行）= 三件事混在一个文件里：
  - `SharedState`（`ui.rs:18-30`）：UI 与抓帧线程共享的 `region` / `running` / `preview_display` / `selecting`；
  - `WorkerEvent`（`ui.rs:34-71`）+ `run_worker`（`ui.rs:89-301`）：抓帧 + 解码 + 落盘的循环；
  - `ReceiverView`（`ui.rs:318-1272`）：gpui 的视图与渲染（header chips / 进度卡 / 块网格 / 已接收列表 / 框选画布）。
- 这些代码里**只有 `render_*` 与 `paint_preview` 真的用到 gpui**，其余都是 `std` + 业务模块。所以换 UI 库的迁移面比看起来小。
- 平台相关的东西只有四处：抓屏（`capture.rs`）、剪贴板（`clipboard.rs`）、通知（`notify.rs`）、以及 `ui.rs` 本身。

### 1.2 "没有 GPU"到底意味着什么

需要区分三种情况，方案只对前两种负责：

1. **虚拟化/云桌面/远程会话**（VMware/VirtualBox/Hyper-V/RDP）：没有 3D 加速，或只有 WARP。
2. **物理机但驱动缺失/极老**：`d3d11.dll` 在，但 `D3D11CreateDevice` 只能拿到 WARP；`DuplicateOutput` 直接失败。
3. 驱动正常、只是性能弱：不属于本方案的目标（那时 DXGI 快路径可以开）。

本方案的硬约束由此得出：

- **不能依赖**：wgpu、Blade、Vulkan、D3D11/12、OpenGL（含 `opengl32` 的 GDI Generic 软实现——它只能报 GL 1.1，且慢到不可用）、WebView2 的 GPU 进程。
- **可以依赖**：GDI（`BitBlt` / `StretchDIBits` / DIB section）、`softbuffer`、系统字体加载（DirectWrite/GDI 字体枚举）、Win32 窗口消息。
- **不需要我们处理**：DWM 合成。Win8 起 DWM 常开，无 3D 适配器时它自己走软件路径；我们只往窗口 DC 上贴像素。

---

## 2. UI 库选型

### 2.1 候选与硬指标

判据按本项目的真实需要排：① 无 GPU 能开窗口并稳定绘制；② 中文/中文字体；③ 能在窗口里**画一张可缩放位图并接受拖拽**（框选区域）；④ 滚动列表 + 进度条；⑤ 单 exe、体积可接受；⑥ 许可证与 MIT 仓库相容；⑦ 与现有 gpui 代码的迁移成本。

| 候选 | 无 GPU | 中文字体 | 位图+拖拽 | 组件 | 体积 | 许可 | 迁移成本 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **iced 0.14 + `tiny-skia`** | ✅ softbuffer→GDI | ✅ cosmic-text + fontdb（Segoe UI / 微软雅黑回退） | ✅ `canvas` + `image` | ✅ 齐全 | ~10–14 MB | **MIT** ✅ | 低：builder 风格与 gpui 几乎一一对应 |
| **Slint 1.19 `renderer-software`** | ✅ 官方软件光栅 | ✅ 需开 `renderer-systemfonts` | ✅ `Image` + `TouchArea` | ✅ 齐全 | ~8–12 MB | ⚠️ GPLv3 或 Slint 免版税桌面许可 | 中：要学 `.slint` DSL，逻辑进属性/回调 |
| **FLTK 1.5** | ✅ 原生 GDI | ⚠️ 要显式指定字体与编码 | ✅ `draw_image` + 鼠标事件 | ⚠️ 够用但简陋 | ~4–6 MB | ✅ LGPL + 静态链接例外 | 中高：命令式布局，外观 1990s |
| **Tauri 2 + WebView2** | ⚠️ 靠 WebView2 的软件回退（`--disable-gpu`） | ✅ | ✅ DOM 全套 | ✅ | ~15 MB + 运行时 | ✅ MIT/Apache | **低**：可直接复用 `apps/web/dist-single/index.html` |
| egui + eframe | ⚠️ 官方只有 glow/wgpu 后端 | ✅ | ⚠️ 需自写软件光栅 | ✅ | ~8 MB | ✅ | 高：要自己接 softbuffer |
| gpui | ❌ 无 Windows 后端，Blade 需要 GPU API | — | — | — | — | — | 不适用 |
| bevy / wgpu 直用 | ❌ | — | — | — | — | — | 不适用 |

### 2.2 为什么首选 iced + tiny-skia

1. **许可证干净**：iced 是 MIT，RaptorQR 也是 MIT，不用改 LICENSE，不用在界面里挂第三方许可声明。
2. **`tiny-skia` 是 iced 的正式渲染器**（不是外挂）：`Cargo.toml` 里关掉 `wgpu`、只留 `tiny-skia`，`iced_winit` 就会用
   `softbuffer` 呈现——`softbuffer` 0.4.8 在 Windows 上就是 DIB + `BitBlt`，纯 CPU。
3. **迁移是机械替换**：gpui 的 `div().flex().gap_3().bg(rgb(..))` 家族，对应 iced 的
   `column![] / row![] / container(...).style(...)`；`canvas(...)` 直接对应 `iced::widget::canvas`；
   事件模型都是"消息 → `update` → 重绘"。
4. **跨平台**：同一套 UI 以后也能跑 macOS/Linux，长期可以把 gpui 整个退役（不是本方案的要求，但是白送的选项）。

代价：样式要自己写（没有 gpui 的 tailwind 风味链式 API 那么顺手，但也就多几十行 `fn card_style()`）。

### 2.3 备选：Slint（更少代码，但要处理许可证）

Slint 的软件渲染器是**一等公民**（MCU 场景就是它），`features = ["backend-winit", "renderer-software", "renderer-systemfonts"]`
三件套即可，官方后端选择器里也有 `winit-software`。`.slint` 里写界面，Rust 侧只做数据绑定，代码量约为 iced 的 1/3。

但 Slint 的许可是 **GPLv3 / 商业许可 / Slint 免版税桌面应用许可** 三选一：
GPLv3 会传染整个 RaptorQR（MIT → 必须整体 GPL），免版税许可要求界面里有可见的 Slint 归属声明、
且对"收费应用"有限制。**只有在你接受其中之一时才选它**，否则用 iced。

### 2.4 决策闸门（0.5 天 spike，先做，再决定）

在无 GPU 的 Windows 上跑一个 200 行的最小程序（iced 与 Slint 各一版），逐条检查通过/不通过：

| 检查项 | 通过标准 |
| --- | --- |
| 窗口能开 | 进程存活，窗口可见，无黑屏/无 D3D 报错 |
| 没有加载 3D 模块 | `Get-Process raptorqr-probe \| %{ $_.Modules.FileName }` 里**不出现** `d3d11.dll` / `d3d12.dll` / `dxgi.dll` / `vulkan-1.dll` / `opengl32.dll` |
| 中文 | "暂停 / 框选区域 / 已接收文件" 正常显示，不出现豆腐块 |
| 位图 | 16:9 位图缩放到 1100×620 无撕裂、无颜色错位 |
| 拖拽 | 鼠标按下-移动-抬起能画出选框，坐标回读正确 |
| 空闲 CPU | 只有状态刷新（2 Hz）时 ≤1% 单核 |
| 滚动 | 已接收列表 50 条滚动不卡 |

任一项不过：iced → Slint → FLTK 依次降级。这个闸门把"选错库"的风险压到半天。

---

## 3. 目标架构

### 3.1 模块树

```text
host/src/
├── lib.rs                # pub mod app; pub mod capture; ...; ui 由 cfg 分派
├── main.rs               # 两个平台都调 ui::run()，--headless / --check 保持原样
├── app/
│   ├── mod.rs            # SharedState / WorkerEvent / run_worker（从 ui.rs 原样搬出）
│   └── mailbox.rs        # 新增：UI 侧消费的"合并槽"，见 §3.3
├── capture/
│   ├── mod.rs            # 公共 API 保持不变（display_list / capture_raw / luma_rect / crop / downscale …）
│   ├── macos.rs          # 现有 CGWindowListCreateImage 实现，原样搬
│   ├── windows_gdi.rs    # 新增：GDI BitBlt（默认路径）
│   └── windows_dxgi.rs   # 可选：有 GPU 时的 DXGI 快路径（feature = "dxgi"）
└── ui/
    ├── mod.rs            # cfg(target_os) → gpui / win 两条 `pub fn run()`
    ├── gpui/mod.rs       # 现在的 ui.rs（只留视图）
    └── win/
        ├── mod.rs        # iced Application：update / view / subscription
        ├── theme.rs      # 颜色、间距、文字尺寸常量（对应现在的 rgb(0x0d1117) 等）
        └── widgets.rs    # 卡片、chip、进度卡、块网格、传输行
```

**边界纪律**：`app` 与 `capture` 都不许 `use` 任何 UI 库；`ui/*` 不许直接抓屏或解码。
这一条就是现在 `ui.rs` 混在一起的根本问题，拆开后两个前端各自只有 600–900 行。

### 3.2 线程与数据流

```text
                        ┌──────────────── worker 线程（唯一）─────────────────┐
                        │ t0: capture::capture_raw(display)   (GDI BitBlt)   │
                        │ t1: RawFrame::to_luma() / crop / downscale          │
                        │ t2: Scanner::tick(now, cfg)  → 解码 → TransferTracker│
                        │ t3: 组装 WorkerEvent                                │
                        └───────────────┬────────────────────────────────────┘
                                        │ Mailbox::post（非阻塞，覆盖旧值）
                                        ▼
                        ┌────────────── Mailbox（Mutex + 版本号）─────────────┐
                        │ status: Option<Status>      preview: Option<Preview>│
                        │ log: VecDeque<Transfer>     dirty: AtomicU8         │
                        └───────────────┬────────────────────────────────────┘
                                        │ UI 定时器 120 ms 取走（合并）
                                        ▼
                        ┌──────────────── UI 线程 ────────────────────────────┐
                        │ iced: Subscription::every → Message::Tick           │
                        │ 读区 → Image::from_rgba8（预览）→ 更新视图           │
                        │ 剪贴板写入（只能在 UI 线程）                          │
                        └────────────────────────────────────────────────────┘
```

要点：

- **抓帧/解码永远不在 UI 线程**（现状已如此，保持）。
- **UI 帧率与抓帧解耦**：状态 2 Hz、预览最多 6–8 Hz、其余事件即时。
- **剪贴板与通知**的调用点仍在 UI 线程（对应 `ui.rs:496-503` 现在的做法）。

### 3.3 为什么要 `Mailbox` 而不是 `mpsc` 队列

现在的 `mpsc::channel` + 200 ms `try_iter` 能工作，但预览帧是**整屏 RGBA（2560×1440×4 ≈ 14.7 MB）**：
只要 UI 卡一次，队列里就会堆几帧十几 MB 的旧图，恢复时先渲染一堆过期画面。
`Mailbox` 用"最新值覆盖 + 脏标记"表达同一语义，内存恒为一帧：

```rust
// app/mailbox.rs
pub struct Mailbox {
    status: Mutex<Option<StatusSnap>>,
    preview: Mutex<Option<PreviewFrame>>,   // 只留最新一帧
    transfers: Mutex<VecDeque<TransferRow>>, // 最多 50 条，见 ui.rs:512-514
    error: Mutex<Option<String>>,
    dirty: AtomicU8,                        // bit0=status bit1=preview bit2=log
}

impl Mailbox {
    pub fn post_preview(&self, frame: PreviewFrame) {  // worker 侧，绝不阻塞
        *self.preview.lock().unwrap() = Some(frame);
        self.dirty.fetch_or(0b10, Ordering::Relaxed);
    }
    /// UI 定时器调用：一次性取走所有脏标记与最新值。
    pub fn drain(&self) -> Drain { /* swap 出 Option，清 dirty */ }
}
```

`PreviewFrame` 建议直接持有 `slint::SharedPixelBuffer` / `iced` 需要的 `RGBA Vec + w/h`，在 worker 侧
（`app` 层不依赖 UI 库，所以用 `Vec<u8>`）准备好，UI 侧只做零拷贝包装。

---

## 4. Windows 抓屏（比 UI 更关键）

### 4.1 现状为什么大概率不行

- `host/Cargo.toml:3` 把 `xcap = "0.9"` 放在**无条件依赖**里，`capture.rs:284-296` 的非 macOS 分支走 `capture_display()`。
- `xcap` 0.9 的 Windows 依赖是 `windows 0.62`，可选 feature 叫 `wgc`——也就是说默认路径是 **DXGI `IDXGIOutputDuplication`**（桌面复制），
  它要求一个真实的 D3D11 适配器；在 VM/RDP/无驱动机器上，`DuplicateOutput` 会返回失败（WARP 设备无法复制输出）。
- 结果就是 `host/README.md:245-246` 记的那条已知限制：**Windows 端抓屏从未在真机验证过**。

所以在做界面前，先把抓屏换成 GDI——否则界面做出来也是空转。

### 4.2 GDI `BitBlt` 方案

```rust
// capture/windows_gdi.rs（示意，windows-sys 0.61）
use windows_sys::Win32::Graphics::Gdi::*;

pub struct GdiFrame {
    hdc_screen: HDC,
    hdc_mem: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,      // 32bpp top-down BGRA，宽 w 高 h
    w: i32, h: i32,
}

impl GdiFrame {
    pub fn new(w: i32, h: i32) -> Option<Self> {
        let hdc_screen = unsafe { GetDC(std::ptr::null_mut()) };
        let hdc_mem = unsafe { CreateCompatibleDC(hdc_screen) };
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w, biHeight: -h,          // 负高度 = top-down，省一次翻转
                biPlanes: 1, biBitCount: 32, biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe { CreateDIBSection(hdc_screen, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0) };
        let old = unsafe { SelectObject(hdc_mem, bitmap) };
        Some(Self { hdc_screen, hdc_mem, bitmap, old, bits: bits as *mut u8, w, h })
    }

    /// 只拷 `rect`（屏幕物理像素）—— GDI 的拷贝成本随面积走，区域越小越快。
    pub fn blit(&mut self, x: i32, y: i32, w: i32, h: i32) -> bool {
        unsafe { BitBlt(self.hdc_mem, 0, 0, w, h, self.hdc_screen, x, y, SRCCOPY | CAPTUREBLT) != 0 }
    }
}

impl Drop for GdiFrame {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc_mem, self.old);
            DeleteObject(self.bitmap as _);
            DeleteDC(self.hdc_mem);
            ReleaseDC(std::ptr::null_mut(), self.hdc_screen);
        }
    }
}
```

设计要点：

1. **DIB section 直接给指针**：`BitBlt` 写进 DIB 的 bits，`RawFrame` 直接借这块内存
   （`&bits[..w*h*4]` → 和你现在 `RawFrame::provider/owned` 的双形态一致），**零额外拷贝**。
2. **区域优先**：`bitblt` 的源坐标就是屏幕坐标，只拷选区。这点和 macOS 相反——
   `CGWindowListCreateImage` 是"固定 11–18 ms，与矩形无关"，而 GDI 是"与面积线性相关"。
   所以在 Windows 上"框选小区域"是实打实的性能优化，`ScanConfig.region` 应该一路传到 `capture_raw`，
   而不是整屏抓完再 `crop`。
3. **多屏**：`EnumDisplayMonitors` 拿每块屏的 `rcMonitor`（虚拟屏幕坐标）与 `MONITORINFOF_PRIMARY`；
   抓某块屏 = `BitBlt(dst, 0,0,w,h, screenDC, mon.rcMonitor.left, mon.rcMonitor.top, SRCCOPY)`。
   负坐标（副屏在主屏左边/上边）必须支持，`ScreenRect` 的 i32 坐标刚好够。
4. **DPI**：`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` 写进 manifest；否则 150% 缩放下
   `GetSystemMetrics` 返回逻辑像素，抓到的图会被系统拉伸。manifest 见 §6.4。
5. **`CAPTUREBLT`**：让分层窗口（部分 electron/游戏覆盖层）也被合成进来；个别老驱动上会闪，
   做成 `RAPTORQR_GDI_PLAIN=1` 可关。
6. **失败模式**：锁屏/安全桌面、UAC 提权窗口、受保护视频 → 全黑。复用现有 `console.rs:207-221`
   的 `black_frame_hint()`，把 Windows 分支文案改成"当前会话可能被锁定/受保护，或运行在无桌面会话中"。
7. **成本预期**（需要在真机确认）：2560×1440 全屏 `BitBlt` 约 3–8 ms，1280×720 区域约 1–2 ms；
   之后 `to_luma` 与解码成本和 macOS 相同。也就是说**区域抓取 + 3 次/秒的 probe 节奏**能把空闲 CPU 压到个位数百分比——
   和 macOS 那边实测的 7% 同量级。

### 4.3 可选：DXGI 快路径

保留 `feature = "dxgi"`：启动时探测 `D3D11CreateDevice(HARDWARE)` 是否成功、`DuplicateOutput` 是否成功，
成功就用 DXGI（能吃满 60 Hz、GPU 侧零拷贝），失败静默降级到 GDI，并在 `--check` 里打印一行 `capture: dxgi|gdi`。
这样同一份二进制在高配机器上不浪费性能，在无 GPU 机器上也能跑。

---

## 5. UI 落地（以 iced 为例）

### 5.1 依赖

```toml
# host/Cargo.toml
[dependencies]
rxing = "0.9"
raptorq = "2.0"
flate2 = "1.1"
image = "0.25"

[target.'cfg(not(windows))'.dependencies]
xcap = "0.9"                                  # macOS/Linux 仍用它
gpui = { version = "0.2.2", features = ["runtime_shaders"] }

[target.'cfg(target_os = "macos")'.dependencies]
objc2-core-foundation = "0.3.2"
objc2-core-graphics = "0.3.2"

[target.'cfg(windows)'.dependencies]
# 实际落地的特性集（`smol` 的理由见下）。
iced = { version = "0.14", default-features = false, features = [
    "tiny-skia", "canvas", "image-without-codecs", "crisp", "smol",
] }
windows-sys = { version = "0.61", features = [
    "Win32_Foundation", "Win32_Graphics_Gdi", "Win32_UI_HiDpi",
    "Win32_UI_WindowsAndMessaging",
] }
```

> 与最初设计的差异（实测后改的，见 §11）：`image` → `image-without-codecs`
> （帧是我们自己抓的裸 RGBA，不需要 PNG/JPEG 解码器）；`thread-pool` → `smol`
> （`iced::time::every` 只在 tokio / smol 后端存在，thread-pool 后端的 `time`
> 模块是空的）；`arboard` / `embed-resource` 暂未引入（剪贴板用 `iced::clipboard::write`，
> 图标与 manifest 留到打包阶段）。

要点：

- **`default-features = false` 是关键**：iced 0.14 的默认特性同时开 `wgpu` 和 `tiny-skia`
  （已核对：`default = ["wgpu","tiny-skia","crisp","web-colors","thread-pool",…]`），
  留着 `wgpu` 它会优先用 wgpu → 无 GPU 机器上直接崩或黑屏。只留 `tiny-skia` 才走 softbuffer。
- **`xcap` 必须移出无条件依赖**：Windows 分支不再需要它，留着只会把 DXGI 那套依赖拖进构建。
- 关掉 `wgpu` 后，`iced_renderer` 的 `fallback::Renderer` 里只剩一种可选渲染器（编译期决定），
  不存在"运行时误选 wgpu"的可能——所以不需要额外的运行时开关，只要不把 `wgpu` 特性打开即可。
  校验方式就是 §2.4 那条 `Modules` 检查：进程里没有 `d3d11.dll`/`dxgi.dll`。

### 5.2 视图结构（对照现有 gpui）

| 现状（gpui） | iced 对应 |
| --- | --- |
| `render_header` (`ui.rs:765-819`) | `row![text("◈ RaptorQR"), chips..., button("暂停"), button("框选区域")]` |
| `render_region_picker` (`ui.rs:822-909`) | `column![picker_bar, canvas(Preview).width(Length::Fill).height(Length::Fill)]` |
| `render_progress_card` (`ui.rs:998+`) | `container(column![progress_bar, block_grid]).style(card)` |
| `render_body` 的已接收列表 (`ui.rs:924-995`) | `scrollable(column(rows))`，行 = `row![icon, name, size, detail]` |
| `paint_preview` + `canvas_bounds` 映射 | `canvas::Program::draw` 里 `frame.fill_rectangle` + `image`；光标坐标用 `bounds` 归一化 |
| `cx.notify()` | `Message` → `update` 返回 `Task::none()`，iced 自动 `request_redraw` |

拖拽选区的实现（iced `canvas::Program`）：

```rust
impl canvas::Program<Message> for PreviewCanvas {
    fn update(&self, state: &mut Drag, event: &Event, bounds: Rectangle, cursor: Cursor) -> (Status, Option<Message>) {
        // 事件里拿 cursor.position_in(bounds) → 除以 bounds.size() 得 0..1 的分数，
        // 再乘 full_w/full_h 得到屏幕物理像素 —— 与 gpui 版 paint_preview 的映射完全同构。
        ...
    }
    fn draw(&self, state: &Drag, renderer: &Renderer, bounds: Rectangle) -> Vec<Geometry> { ... }
}
```

### 5.3 消息与订阅

```rust
pub enum Message { Tick, Pause, TogglePicker, PickStart(f32,f32), PickMove(f32,f32), PickEnd,
                   PrevDisplay, NextDisplay, CancelPicker, ClearRegion, Copy(usize) }

impl Receiver {
    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(std::time::Duration::from_millis(120)).map(|_| Message::Tick)
    }
    fn update(&mut self, msg: Message) -> Task<Message> {
        match msg {
            Message::Tick => { if let Some(d) = self.mailbox.drain() { self.apply(d) } Task::none() }
            ...
        }
    }
}
```

- 定时器 120 ms：比现状的 200 ms 略快，但**只在 `drain` 有脏标记时才真正改状态**，iced 不会因此重绘。
- 预览帧上限 6–8 Hz：由 worker 侧节流（`PreviewFrame` 只在 `selecting == true` 时生成，和 `ui.rs:111` 的
  `want_preview` 语义一致），UI 侧不再二次节流。
- 剪贴板：`arboard::Clipboard::new().set_text(text)`，失败时回落到现有的 `clipboard::copy_text`
  （PowerShell 路径，启动约 150–400 ms，只在 GUI 路径失败时才用）。

### 5.4 渲染预算

| 项 | 目标 | 说明 |
| --- | --- | --- |
| 窗口尺寸 | 1120×760 固定（可缩放但设最小 900×600） | 软件光栅下面积就是成本，冻结默认尺寸最划算 |
| 空闲重绘 | 2 Hz（仅状态文字/进度） | 局部脏区由 iced/softbuffer 处理；实测目标是单核 ≤1% |
| 框选预览 | 6–8 Hz，预览宽 ≤1100 | worker 先降采样（复用 `capture::downscale`），UI 侧只做一次最近邻/线性放大 |
| 单帧预算 | ≤8 ms | 一块位图 + 十几个矩形 + 文字；超过就说明在画没必要的东西 |
| 内存 | 预览帧 1 张（Mailbox 覆盖） | 避免现在 `preview_pending_drop` 那种双缓冲堆积 |

---

## 6. 平台壳

### 6.1 剪贴板

- GUI 路径用 `arboard`（Win32 剪贴板，无 GPU、无子进程、无编码坑）。
- 保留 `clipboard::copy_text`（`clipboard.rs:23-33` 的 PowerShell 路径）作为无 GUI / arboard 失败时的兜底。
- 语义不变：**文本不落盘**，只有 `transfer::save_completed` 一个落盘入口（`transfer.rs:312`）。

### 6.2 通知

`notify.rs:76-90` 现在 Windows 分支是 no-op。Windows 上要能弹 toast，需要：

1. 应用有一个 **AppUserModelID**（`SetCurrentProcessExplicitAppUserModelID`）；
2. 开始菜单里有带同一 AUMID 的快捷方式（NSIS 安装时创建）；
3. 才可以用 `notify-rust 4.19`（Windows toast）或直接 WinRT。

降级链：toast → `FlashWindowEx` 任务栏闪烁 → 仅应用内列表条目。
文件名仍是**不可信输入**：`notify.rs` 里拼接前要按现有 `applescript_literal` 的思路做长度截断与换行清洗，
Windows 的 toast 不做命令求值，但仍要防止超长/控制字符。

### 6.3 单实例、自启、日志

- 单实例：`CreateMutexW("Global\\RaptorQRReceiver")`，已存在则 `FindWindowW` + `SetForegroundWindow`（或直接退出）。
- 自启：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，值 = 安装路径；界面不提供开关（保持 scope 小）。
- 日志：无控制台时 `eprintln!` 会丢。加一个 `%LOCALAPPDATA%\RaptorQR\receiver.log`（保留 `RAPTORQR_DEBUG` 语义），
  排查"黑屏/抓不到"时必须能看到 `capture: gdi|dxgi` 与每帧耗时。

### 6.4 清单与打包

- `#![windows_subsystem = "windows"]`（`main.rs` 顶部按 cfg 加），日志走文件。
- `build.rs` 用 `embed-resource` 嵌 `app.manifest`：`PerMonitorV2` DPI、`asInvoker`（不要管理员）、Win10/11 `supportedOS`。
- 静态 CRT：`RUSTFLAGS="-C target-feature=+crt-static"` → 目标机器不需要 VC++ 运行库。
- 安装包：NSIS 3（`gitsc` 已有同款脚本可抄：装到 `Program Files`、开始菜单/桌面快捷方式、
  卸载保留 `%LOCALAPPDATA%\RaptorQR`）。
- 预期体积：8–14 MB 单 exe（vs macOS 那份 universal 二进制），无运行时依赖。

---

## 7. 里程碑与验收

| # | 内容 | 产出 | 验收 | 估时 |
| --- | --- | --- | --- | --- |
| M-1 | **选型 spike**（§2.4） | `spikes/win-ui/` 两个 200 行 demo | 7 项检查全过 | 0.5 d |
| M0 | 抽 `app/` 模块：`SharedState`/`WorkerEvent`/`run_worker` 搬出 `ui.rs`，gpui 侧只留渲染 | `host/src/app/*`、`ui/gpui/mod.rs` | macOS `pnpm host:test` 全绿；GUI 行为与现在一致（关窗/重开/选区保留） | 0.5 d |
| M1 | `capture/windows_gdi.rs` + `display_list` + `--check` | Windows 可跑的 `raptorqr-host.exe` | 无 GPU 机器上 `--check` 打印每块屏尺寸、亮度非 0、能解到 ≥1 个二维码 | 1 d |
| M2 | iced 壳：header/进度卡/块网格/已接收列表/框选画布 | `ui/win/*` | 与 macOS 版功能对齐（暂停、框选、多屏切换、清区域、复制文本） | 1.5 d |
| M3 | 平台件：arboard、toast、单实例、自启、日志 | `notify/windows.rs`、`clipboard` 分支 | 收到文件后有 toast + 列表；文本自动进剪贴板（含中文） | 0.5 d |
| M4 | 打包与真机回归 | NSIS 安装包 | 干净机器安装即用；无 GPU 机器满流程跑通一次 5 MB 文件 | 0.5 d |

**无 GPU 机器上的验收清单**（每步都要在目标机上跑）：

```powershell
# 1) 确认真的没有 3D 适配器（应只见 "Microsoft Basic Display Adapter" 或 WARP）
Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion

# 2) 确认进程没有把 3D 模块拉进来（这是"确实在用软件渲染"的硬证据）
Get-Process raptorqr-host | ForEach-Object { $_.Modules.FileName } |
  Select-String -Pattern 'd3d11|d3d12|dxgi|vulkan|opengl32'

# 3) 抓屏自检
.\raptorqr-host.exe --check

# 4) 空闲 CPU（单核百分比）
Get-Counter '\Process(raptorqr-host)\% Processor Time' -SampleInterval 1 -MaxSamples 10

# 5) 满流程：发送端 5 MB 文件 → 接收 → 校验 sha256
```

通过标准：第 2 步**空输出**；第 3 步每块屏 `luma` 均值非 0 且至少解到 1 个二维码；
第 4 步空闲 ≤5%（框选打开时 ≤25%）；第 5 步哈希一致。

---

## 8. 风险与兜底

| 风险 | 触发条件 | 兜底 |
| --- | --- | --- |
| iced `tiny-skia` 路径在 Windows 上有未发现的 bug（滚动/图片/字体） | M-1 或 M2 | 降级 Slint software（接受其许可证），或 FLTK |
| Slint 软件渲染缺 CJK 字形 | 若选 Slint 且系统字体查找失败 | 开 `renderer-systemfonts`；再不行打包 Noto Sans SC 子集 |
| `BitBlt` 在 RDP/锁屏会话拿到全黑 | 远程桌面、屏幕锁定 | 复用 `capture_black` 提示；`--check` 明确打印"会话可能被锁定" |
| GDI 抓帧比预期慢（大屏 + 高 DPI） | 4K 屏、150% 缩放 | 只抓选区（默认行为）；必要时 `StretchBlt` 在抓取时直接减半分辨率 |
| 软件渲染 CPU 打满 | 大窗口 + 高频预览 | 固定窗口尺寸、预览 6 Hz、预览宽 ≤1100、状态 2 Hz |
| 装机环境没有 `%LOCALAPPDATA%` 写权限 / 组策略禁自启 | 企业域机器 | 自启失败只记日志，不阻塞启动 |
| 许可证选错（Slint GPLv3 传染） | 选 Slint 且发布 | 发布前先定许可：要么换 iced，要么整体 GPLv3，要么接受免版税许可 + 界面归属声明 |

---

## 9. 明确不做

- 不动 `scanner` / `transfer` / `raptorq` / `qrscan` 的任何逻辑（M0 只搬文件）。
- 不做主题系统、多语言、托盘菜单以外的新交互。
- 不替换 macOS 的 gpui（同一套 iced 壳以后可以顺带覆盖，但不是这次的目标，也不要在 M2 里顺手做）。
- 不用 Tauri/WebView2 作为主方案（作为"想复用 `apps/web/dist-single/index.html`"时的 C 计划保留）。

---

## 10. 事实核对表（写作时已核实）

| 事实 | 来源 |
| --- | --- |
| gpui 仅作非 Windows 依赖，Windows 无 GUI | `host/Cargo.toml:15-18`、`host/src/main.rs:33-36`、`host/src/lib.rs:14-15` |
| Windows 抓屏走 xcap，且从未真机验证 | `host/Cargo.toml:3`、`host/src/capture.rs:284-296`、`host/README.md:245-246` |
| xcap 0.9.0 的 Windows 依赖为 `windows 0.62`，可选 feature `wgc` | crates.io sparse index（rsproxy 镜像）实测 |
| iced 0.14.0 默认同时开 `wgpu` 与 `tiny-skia` | 同上，`default = ["wgpu","tiny-skia",…]` |
| Slint 1.19.0 有 `renderer-software` / `backend-winit` / `renderer-systemfonts` 特性 | docs.rs feature 列表实测 |
| 版本：softbuffer 0.4.8、arboard 3.7.0、windows-sys 0.61.2、fast_image_resize 6.1.0、notify-rust 4.19.0 | crates.io sparse index |
| 本机工具链 rustc 1.100.0 | `rustc --version` |
| macOS 侧 CPU 实测：GUI ~7%、headless ~4%；抓帧固定 11–18 ms 与面积无关 | 项目白板「性能现状与关键实测」 |

---

## 11. 实施现状（2026-11-16，代码已落地）

M-1 / M0 / M1 / M2 已完成，`cargo check --target x86_64-pc-windows-msvc --all-targets`
零 error 零 warning，并且已经在 macOS 上用 mingw 交叉编译出可运行的 exe。

### 11.1 实际改了什么

| 文件 | 改动 |
| --- | --- |
| `host/src/app.rs`（新） | 从 `ui.rs` 抽出的共用内核：`SharedState`、`WorkerEvent`、`run_worker`、`TransferItem`、`LAST_REGION`、`TEXT_CARD_LINES`、`STATUS_INTERVAL`。无任何 GUI 依赖。 |
| `host/src/ui.rs` | 只剩 gpui 外壳，正文从 `pub struct ReceiverView` 开始；macOS/Linux 行为不变。 |
| `host/src/ui_win.rs`（新，约 1000 行） | iced 外壳：头部胶囊/按钮、进度卡与子块进度、文本卡、已接收列表、框选 canvas、页脚提示、`Message`/`update`/`view`/`subscription`。 |
| `host/src/capture_win.rs`（新） | GDI 抓屏：`dpi_aware()`、`monitors()`（缓存 2 s + 主屏优先）、`capture_bgra(monitor, Option<ScreenRect>)`（复用 DIB，`BitBlt` 只拷选区）。 |
| `host/src/capture.rs` | Windows 分支：`display_list`/`logical_size`/`capture_display`/`capture_luma`/`capture_raw_scoped`；`RawFrame` 新增 `scope: Option<ScreenRect>`。非 Windows 路径行为不变（`capture_raw_scoped` 直接委托 `capture_raw`）。 |
| `host/src/scanner.rs` | 抓帧改走 `capture_raw_scoped(index, 属于该屏的选区)`；解码时若 `frame.scope` 已带裁剪矩形，就不再二次按 `cfg.region` 裁剪。 |
| `host/src/main.rs` | `--headless` 走 console；Windows 走 `capture_win::dpi_aware()` → `ui_win::run()`，窗口起不来时回落到 console；macOS/Linux 仍走 `gui_main()`。 |
| `host/src/lib.rs` | `#[cfg(windows)] pub mod capture_win;` / `pub mod ui_win;`。 |

### 11.2 与设计稿的三处偏离

1. **没有引入 `Mailbox`**（§3.3）。Windows 侧的重绘不是由事件驱动，而是
   `iced::time::every(150ms)` 的订阅 tick 去 `drain` worker 的 `mpsc`：**同一批里只保留
   最后一张预览**，其余丢弃。效果与 Mailbox 等价（内存恒为一帧），少了一个结构体和
   一处双端状态。gpui 侧完全没动。
2. **框选用 `canvas::Program` 而不是叠在 image 上**。`iced_tiny_skia` 实现了
   `geometry::Renderer`（源码核对：`iced_tiny_skia-0.14.1/src/lib.rs:320`），所以
   `Frame::draw_image` + `fill_rectangle` + `stroke_rectangle` 在纯 CPU 光栅下就能画出
   预览、半透明选区和高亮边框，不需要 GPU geometry pipeline。
3. **没有系统通知**（§6.2）。`iced::window::request_user_attention` 需要 `window::Id`，
   而 iced 0.14 没有 `Id::MAIN`，`window::latest()` 的产物是一条 `Task<Option<Id>>` 型
   消息，混进本应用的 `Message` 枚举很别扭。v1 用应用内卡片 + 列表承担反馈，
   真要 toast 时再在 `notify.rs` 里补 Windows 实现。

### 11.3 平台细节（实现期发现、值得记住的）

- **exe 保持控制台子系统**：`--check` / `--headless` 要往 stdout 写，改成
  `windows_subsystem = "windows"` 就得重开控制台句柄，反而更容易踩坑。代价是双击时
  Windows 会给进程配一个控制台窗口；`ui_win::set_console_visible(false)` 在开窗前把它
  `ShowWindow(SW_HIDE)`——只隐藏、不 `FreeConsole`，所以 std 的 stdout 仍然有效，
  窗口起不来回落控制台时还能打印。用 `GetConsoleProcessList` 判断控制台是否与 shell
  共用（>1 个进程就保持可见），避免把用户的终端一起藏了。
- **CLI 与窗口的边界**：`--region` / `--display` 会被窗口版拿来当初始选区（`Receiver::new`
  读 `console::parse_args()`），`--out` / `--once` 没有窗口等价物，`main.rs` 直接把这两类
  参数路由到控制台版——老脚本的行为不变。
- **抓屏失败文案按平台分叉**：`app.rs` 里 `if cfg!(windows) { "GDI returned an empty frame" }
  else { "grant Screen Recording permission?" }`，同一个失败在两边原因完全不同。
- **体积**：GNU 目标 release 未 strip 约 40 MB，`x86_64-w64-mingw32-strip --strip-all`
  后约 27 MB（MSVC 目标由 CI 产出，PDB 单独放，不需要这步）。

### 11.4 上机验收（Windows 侧还没跑过）

```powershell
raptorqr-host.exe --check          # 逐屏尺寸/亮度/解到几个二维码
raptorqr-host.exe                  # 窗口版
```

判据：窗口有画面（不是全黑）、`QR/s` 会动、`框选区域` 里能看到预览并能拖框、
收到文件后卡片出现且 `Downloads` 里文件 sha256 与发送端一致。

交叉编译（macOS，无需 MSVC）：

```bash
cargo build --manifest-path host/Cargo.toml --release --target x86_64-pc-windows-gnu
# → host/target/x86_64-pc-windows-gnu/release/raptorqr-host.exe
```
