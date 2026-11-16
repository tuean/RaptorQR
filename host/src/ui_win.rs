//! iced receiver shell for Windows.
//!
//! Why iced: gpui has no Windows backend at all, and the target machine has no
//! usable GPU either. iced draws with `tiny-skia` on the CPU and presents the
//! finished bitmap through `softbuffer` (a GDI blit), so it needs neither a GPU
//! nor a 3D driver. Window composition itself stays with DWM, which Windows
//! already runs on WARP — no part of this path asks for a hardware adapter.
//!
//! `Cargo.toml` therefore builds iced with `default-features = false` and only
//! `tiny-skia`: iced's default features include `wgpu`, which would try to claim
//! a real adapter and leave a black window on this class of machine.
//!
//! The capture/decode loop is not here — it lives in [`crate::app`] and is shared
//! with the gpui shell (`ui.rs`). This file only owns presentation, the region
//! picker, and the two lines of clipboard plumbing iced can do natively.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use iced::widget::canvas::{self, Canvas};
use iced::widget::{
    button, column, container, image, progress_bar, row, scrollable, text, Space,
};
use iced::{
    mouse, Alignment, Background, Border, Color, Element, Length, Padding, Point, Rectangle, Size,
    Subscription, Task, Theme,
};

use crate::app::{
    run_worker, SharedState, TransferItem, WorkerEvent, LAST_REGION, TEXT_CARD_LINES,
};
use crate::capture::{self, ScreenRect};
use crate::notify::human_bytes;

/// How often the UI drains worker events.
///
/// iced repaints the whole window for every message, and this is a CPU
/// rasterizer, so the tick is a cost/feel tradeoff: the worker emits status at
/// 500 ms and preview frames at 350 ms, and 200 ms keeps both within about half
/// a second of the truth at five repaints per second.
const POLL: Duration = Duration::from_millis(200);

/// Transfers kept in the list; the window shows the newest dozen, but a copy is
/// kept so an entry does not vanish the moment the next one lands.
const LIST_CAP: usize = 50;
const LIST_SHOWN: usize = 12;

// ─── Palette (same values as the gpui shell) ────────────────────────────────

const BG: u32 = 0x0d11_17;
const PANEL: u32 = 0x16_1b_22;
const BORDER: u32 = 0x21_26_2d;
const TEXT: u32 = 0xd6_d6_d6;
const MUTED: u32 = 0x8b_94_9e;
const DIM: u32 = 0x6a_6a_6e;
const BLUE: u32 = 0x58_a6_ff;
const GREEN: u32 = 0x3f_b9_50;
const RED: u32 = 0xff_7b_72;
const BUTTON: u32 = 0x21_26_2d;
const BUTTON_HOVER: u32 = 0x30_36_3d;
const PRIMARY: u32 = 0x1f_6f_eb;
const PRIMARY_HOVER: u32 = 0x38_8b_fd;
const PICKER_BG: u32 = 0x01_04_09;
const SELECTION: u32 = 0x4f_9d_ff;

fn c(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// ─── Messages ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Message {
    /// Drain whatever the capture worker produced since the last look.
    Tick,
    ToggleRunning,
    OpenPicker,
    ClosePicker,
    ClearRegion,
    PrevDisplay,
    NextDisplay,
    /// A finished drag, in fractions of the previewed display image.
    Region((f32, f32), (f32, f32)),
    /// Copy the text of the transfer at this index to the clipboard.
    Copy(usize),
}

// ─── State ──────────────────────────────────────────────────────────────────

struct Receiver {
    shared: Arc<SharedState>,
    events: mpsc::Receiver<WorkerEvent>,

    running: bool,
    capture_black: bool,
    fps: f64,
    qr_rate: f64,
    progress: f64,
    blocks: Vec<(u8, u32, u32)>,
    note: String,
    last_error: Option<String>,

    selecting: bool,
    preview_display: usize,
    display_count: usize,
    preview: Option<image::Handle>,
    preview_dims: (u32, u32),
    preview_full: Option<ScreenRect>,
    region: Option<(usize, ScreenRect)>,

    transfers: Vec<TransferItem>,
}

impl Receiver {
    fn new() -> Self {
        // The region survives closing the window (iced quits the process on
        // close, but this also covers a rebuild of the state in development).
        let display_count = capture::display_list().len().max(1);
        let mut region = *LAST_REGION.lock().unwrap();
        let mut preview_display = region.map(|(display, _)| display).unwrap_or(0);

        // `--region`/`--display` seed the window exactly like the picker would,
        // so a shortcut with a pre-set scope needs no clicking. (`--out` and
        // `--once` have no window meaning and stay on the console path.)
        let args = crate::console::parse_args();
        if let Some(rect) = args.region {
            region = Some((preview_display.min(display_count - 1), rect));
        }
        if let Some(display) = args.display {
            preview_display = display.min(display_count - 1);
        }
        let preview_display = preview_display.min(display_count - 1);

        let shared = Arc::new(SharedState {
            region: Mutex::new(region),
            running: AtomicBool::new(true),
            preview_display: AtomicUsize::new(preview_display),
            display_count: AtomicUsize::new(display_count),
            preview_request: AtomicBool::new(false),
            selecting: AtomicBool::new(false),
        });

        let (tx, events) = mpsc::channel();
        let worker_shared = shared.clone();
        std::thread::spawn(move || run_worker(worker_shared, tx));

        Self {
            shared,
            events,
            running: true,
            capture_black: false,
            fps: 0.0,
            qr_rate: 0.0,
            progress: 0.0,
            blocks: Vec::new(),
            note: String::new(),
            last_error: None,
            selecting: false,
            preview_display,
            display_count,
            preview: None,
            preview_dims: (0, 0),
            preview_full: None,
            region,
            transfers: Vec::new(),
        }
    }

    /// Apply everything the worker queued since the last tick.
    ///
    /// Only the newest preview is kept: the frames are a few megabytes each and
    /// a UI hitch would otherwise queue them up behind each other.
    fn drain(&mut self) -> Task<Message> {
        let mut preview = None;
        let mut clipboard = None;

        for event in self.events.try_iter() {
            match event {
                WorkerEvent::Preview {
                    display,
                    display_count,
                    rgba,
                    w,
                    h,
                    full_w,
                    full_h,
                } => {
                    preview = Some((display, display_count, rgba, w, h, full_w, full_h));
                }
                WorkerEvent::Status {
                    fps,
                    packets: _,
                    qr_per_second,
                    scanning: _,
                    capture_black,
                    progress,
                    blocks,
                    note,
                } => {
                    self.fps = fps;
                    self.qr_rate = qr_per_second;
                    self.capture_black = capture_black;
                    self.progress = progress;
                    self.blocks = blocks;
                    self.note = note;
                    if !capture_black {
                        self.last_error = None;
                    }
                }
                WorkerEvent::Transfer {
                    filename,
                    path,
                    size,
                    written,
                    text,
                } => {
                    // Text never touches the disk; it goes straight to the
                    // clipboard, and the card keeps a copy for a retry.
                    let copied = text.as_ref().is_some_and(|body| !body.is_empty());
                    if copied {
                        clipboard = text.clone();
                    }
                    self.transfers.push(TransferItem {
                        filename,
                        path,
                        size,
                        written,
                        text,
                        copied,
                    });
                    while self.transfers.len() > LIST_CAP {
                        self.transfers.remove(0);
                    }
                }
                WorkerEvent::CaptureError(message) => self.last_error = Some(message),
            }
        }

        // A frame that was already queued when the picker closed has no
        // consumer; dropping it here keeps the next picker from flashing it.
        if let Some((display, count, rgba, w, h, full_w, full_h)) = preview.filter(|_| self.selecting) {
            self.preview_display = display;
            self.display_count = count.max(1);
            self.preview = Some(image::Handle::from_rgba(w, h, rgba));
            self.preview_dims = (w, h);
            // The preview is the whole display, so the coordinate space regions
            // live in is the frame itself.
            self.preview_full = Some(ScreenRect {
                x: 0,
                y: 0,
                w: full_w,
                h: full_h,
            });
        }

        match clipboard {
            Some(body) => iced::clipboard::write(body),
            None => Task::none(),
        }
    }

    /// Turn a drag on the preview into a region on the previewed display.
    fn commit_region(&mut self, from: (f32, f32), to: (f32, f32)) -> Task<Message> {
        let Some(full) = self.preview_full else {
            return Task::none();
        };
        if full.w == 0 || full.h == 0 {
            return Task::none();
        }

        let x0 = from.0.min(to.0).clamp(0.0, 1.0);
        let x1 = from.0.max(to.0).clamp(0.0, 1.0);
        let y0 = from.1.min(to.1).clamp(0.0, 1.0);
        let y1 = from.1.max(to.1).clamp(0.0, 1.0);

        let rect = ScreenRect {
            x: (x0 * full.w as f32).round() as u32,
            y: (y0 * full.h as f32).round() as u32,
            w: ((x1 - x0) * full.w as f32).round() as u32,
            h: ((y1 - y0) * full.h as f32).round() as u32,
        }
        .intersect(&full);

        // A sliver this small is a mis-click, not a selection.
        if rect.area() * 500 < full.area() {
            return Task::none();
        }

        self.region = Some((self.preview_display, rect));
        *self.shared.region.lock().unwrap() = self.region;
        self.selecting = false;
        self.shared.selecting.store(false, Ordering::Relaxed);
        // The picker is gone, so the preview has no consumer left.
        self.preview = None;
        self.preview_full = None;
        self.preview_dims = (0, 0);
        Task::none()
    }

    fn switch_display(&mut self, delta: i64) -> Task<Message> {
        let count = self.display_count.max(1) as i64;
        let next = (self.preview_display as i64 + delta).rem_euclid(count) as usize;
        self.preview_display = next;
        self.shared.preview_display.store(next, Ordering::Relaxed);
        // Drop the old display's frame right away; a stale preview under a new
        // display number is worse than a blank one.
        self.preview = None;
        self.preview_dims = (0, 0);
        self.preview_full = None;
        self.shared.preview_request.store(true, Ordering::Relaxed);
        Task::none()
    }

    /// Chips in the header: state, decode rate, frame rate, capture scope.
    fn chips(&self) -> Vec<(String, u32)> {
        let mut chips = Vec::with_capacity(4);
        chips.push((
            if self.running { "● 捕获中" } else { "○ 已暂停" }.to_string(),
            if self.running { GREEN } else { MUTED },
        ));
        if self.capture_black {
            chips.push(("抓屏全黑".to_string(), RED));
        } else if self.qr_rate > 0.0 {
            chips.push((format!("{:.1} QR/s", self.qr_rate), BLUE));
        } else {
            chips.push(("未发现二维码".to_string(), MUTED));
        }
        chips.push((format!("{:.1} fps", self.fps), DIM));
        chips.push(match self.region {
            Some((display, rect)) => (
                format!("显示 {} · {}×{} 区域", display + 1, rect.w, rect.h),
                DIM,
            ),
            None => (format!("全部 {} 块屏", self.display_count.max(1)), DIM),
        });
        chips
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // Stop the worker and remember the region; the process is on its way out,
        // but this is also what a future "reopen the window" would rely on.
        self.shared.running.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = LAST_REGION.lock() {
            *slot = self.region;
        }
    }
}

fn subscription(_receiver: &Receiver) -> Subscription<Message> {
    iced::time::every(POLL).map(|_| Message::Tick)
}

fn update(receiver: &mut Receiver, message: Message) -> Task<Message> {
    match message {
        Message::Tick => receiver.drain(),
        Message::ToggleRunning => {
            receiver.running = !receiver.running;
            receiver.shared.running.store(receiver.running, Ordering::Relaxed);
            Task::none()
        }
        Message::OpenPicker => {
            receiver.selecting = true;
            receiver.shared.selecting.store(true, Ordering::Relaxed);
            // Show something immediately instead of waiting for the throttle.
            receiver.shared.preview_request.store(true, Ordering::Relaxed);
            Task::none()
        }
        Message::ClosePicker => {
            receiver.selecting = false;
            receiver.shared.selecting.store(false, Ordering::Relaxed);
            receiver.preview = None;
            receiver.preview_dims = (0, 0);
            receiver.preview_full = None;
            Task::none()
        }
        Message::ClearRegion => {
            receiver.region = None;
            *receiver.shared.region.lock().unwrap() = None;
            *LAST_REGION.lock().unwrap() = None;
            Task::none()
        }
        Message::PrevDisplay => receiver.switch_display(-1),
        Message::NextDisplay => receiver.switch_display(1),
        Message::Region(from, to) => receiver.commit_region(from, to),
        Message::Copy(index) => {
            let Some(item) = receiver.transfers.get_mut(index) else {
                return Task::none();
            };
            let Some(body) = item.text.clone() else {
                return Task::none();
            };
            item.copied = true;
            iced::clipboard::write(body)
        }
    }
}

// ─── Region picker canvas ───────────────────────────────────────────────────

/// The picker's draw program: the previewed display plus the drag selection.
struct Preview {
    handle: Option<image::Handle>,
    dims: (u32, u32),
    /// Physical size of the whole display — the space regions are expressed in.
    full: Option<ScreenRect>,
    /// Already committed region, when it belongs to the previewed display.
    region: Option<ScreenRect>,
}

#[derive(Default)]
struct Drag {
    from: Option<(f32, f32)>,
    to: Option<(f32, f32)>,
}

impl Preview {
    /// Where the image sits inside the canvas: letterboxed, aspect preserved.
    fn fitted(&self, bounds: Rectangle) -> Option<Rectangle> {
        let (w, h) = self.dims;
        if w == 0 || h == 0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
            return None;
        }
        let (w, h) = (w as f32, h as f32);
        let scale = (bounds.width / w).min(bounds.height / h);
        let size = Size::new(w * scale, h * scale);
        Some(Rectangle::new(
            Point::new(
                (bounds.width - size.width) / 2.0,
                (bounds.height - size.height) / 2.0,
            ),
            size,
        ))
    }

    /// Canvas-local cursor position → fraction of the image, if it is on it.
    ///
    /// Off-image drags are ignored rather than clamped: a click in the letterbox
    /// is not a selection, and clamping would silently snap it to an edge.
    fn fraction_at(&self, bounds: Rectangle, position: Point) -> Option<(f32, f32)> {
        let fitted = self.fitted(bounds)?;
        let x = (position.x - fitted.x) / fitted.width;
        let y = (position.y - fitted.y) / fitted.height;
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
    }

    /// A region on the display → a rectangle inside the fitted image.
    fn region_rect(&self, fitted: Rectangle) -> Option<Rectangle> {
        let full = self.full?;
        let region = self.region?;
        if full.w == 0 || full.h == 0 {
            return None;
        }
        let x0 = region.x as f32 / full.w as f32;
        let y0 = region.y as f32 / full.h as f32;
        let x1 = (region.x + region.w) as f32 / full.w as f32;
        let y1 = (region.y + region.h) as f32 / full.h as f32;
        Some(rect_of(fitted, (x0, y0), (x1, y1)))
    }
}

fn rect_of(fitted: Rectangle, from: (f32, f32), to: (f32, f32)) -> Rectangle {
    let x0 = from.0.min(to.0);
    let x1 = from.0.max(to.0);
    let y0 = from.1.min(to.1);
    let y1 = from.1.max(to.1);
    Rectangle::new(
        Point::new(
            fitted.x + x0 * fitted.width,
            fitted.y + y0 * fitted.height,
        ),
        Size::new(
            (x1 - x0) * fitted.width,
            (y1 - y0) * fitted.height,
        ),
    )
}

impl canvas::Program<Message> for Preview {
    type State = Drag;

    fn update(
        &self,
        drag: &mut Drag,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let at = cursor.position_in(bounds)?;
                let from = self.fraction_at(bounds, at)?;
                drag.from = Some(from);
                drag.to = Some(from);
                Some(canvas::Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                drag.from?;
                drag.to = cursor
                    .position_in(bounds)
                    .and_then(|at| self.fraction_at(bounds, at));
                Some(canvas::Action::request_redraw().and_capture())
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                let from = drag.from?;
                let to = drag.to?;
                drag.from = None;
                drag.to = None;
                Some(canvas::Action::publish(Message::Region(from, to)).and_capture())
            }
            _ => None,
        }
    }

    fn mouse_interaction(
        &self,
        _state: &Drag,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match cursor.position_in(bounds) {
            Some(at) if self.fraction_at(bounds, at).is_some() => mouse::Interaction::Crosshair,
            _ => mouse::Interaction::default(),
        }
    }

    fn draw(
        &self,
        drag: &Drag,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), c(PICKER_BG));

        if let Some(fitted) = self.fitted(bounds) {
            if let Some(handle) = &self.handle {
                frame.draw_image(fitted, canvas::Image::new(handle.clone()));
            }

            let selection = match (drag.from, drag.to) {
                (Some(from), Some(to)) => Some(rect_of(fitted, from, to)),
                _ => self.region_rect(fitted),
            };

            if let Some(rect) = selection {
                if rect.width >= 1.0 && rect.height >= 1.0 {
                    frame.fill_rectangle(
                        rect.position(),
                        rect.size(),
                        Color::from_rgba(0.31, 0.62, 1.0, 0.12),
                    );
                    frame.stroke_rectangle(
                        rect.position(),
                        rect.size(),
                        canvas::Stroke::default()
                            .with_color(c(SELECTION))
                            .with_width(2.0),
                    );
                }
            }
        }

        vec![frame.into_geometry()]
    }
}

// ─── View ───────────────────────────────────────────────────────────────────

fn view(receiver: &Receiver) -> Element<'_, Message> {
    let mut layout = column![header(receiver)];
    if receiver.selecting {
        layout = layout.push(picker(receiver));
    } else {
        layout = layout.push(container(body(receiver)).height(Length::Fill));
    }
    layout = layout.push(footer(receiver));

    container(layout)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(c(BG))),
            text_color: Some(c(TEXT)),
            ..container::Style::default()
        })
        .into()
}

fn header(receiver: &Receiver) -> Element<'_, Message> {
    let mut chips = row![].spacing(6).align_y(Alignment::Center);
    for (label, colour) in receiver.chips() {
        chips = chips.push(pill(label, colour));
    }

    let mut actions = row![
        button(text(if receiver.running { "暂停" } else { "继续" }).size(12.0))
            .style(secondary)
            .on_press(Message::ToggleRunning)
            .padding([4.0, 12.0]),
        button(text("框选区域").size(12.0))
            .style(primary)
            .on_press(Message::OpenPicker)
            .padding([4.0, 12.0]),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    if receiver.region.is_some() {
        actions = actions.push(
            button(text("清除区域").size(12.0))
                .style(secondary)
                .on_press(Message::ClearRegion)
                .padding([4.0, 12.0]),
        );
    }

    container(
        row![
            text("◈ RaptorQR").size(15.0).color(c(BLUE)),
            chips,
            Space::new().width(Length::Fill),
            actions,
        ]
        .spacing(12)
        .align_y(Alignment::Center),
    )
    .padding(Padding {
        top: 10.0,
        right: 16.0,
        bottom: 10.0,
        left: 16.0,
    })
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(c(PANEL))),
        ..container::Style::default()
    })
    .into()
}

fn pill(label: String, colour: u32) -> Element<'static, Message> {
    container(text(label).size(11.0).color(c(colour)))
        .padding(Padding {
            top: 2.0,
            right: 8.0,
            bottom: 2.0,
            left: 8.0,
        })
        .style(move |_| container::Style {
            background: Some(Background::Color(c(PANEL))),
            border: Border {
                color: c(BORDER),
                width: 1.0,
                radius: 20.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

fn body(receiver: &Receiver) -> Element<'_, Message> {
    let mut list = column![].spacing(6).width(Length::Fill);
    if receiver.transfers.is_empty() {
        list = list.push(
            text("还没有收到内容。发送端开始播放二维码后，文件会保存到「下载」文件夹，文本会直接进剪贴板并显示在这里。")
                .size(12.0)
                .color(c(DIM)),
        );
    } else {
        for (index, item) in receiver.transfers.iter().enumerate().rev().take(LIST_SHOWN) {
            list = list.push(transfer_row(index, item));
        }
    }

    column![
        progress_card(receiver),
        text("已接收文件").size(11.0).color(c(MUTED)),
        scrollable(list).height(Length::Fill),
    ]
    .spacing(10)
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn progress_card(receiver: &Receiver) -> Element<'_, Message> {
    let content: Element<'_, Message> = if let Some(done) = receiver.transfers.last() {
        match &done.text {
            Some(text_body) => text_card(done, text_body, receiver.transfers.len() - 1),
            None => row![
                text(format!("✓ 已接收 {}", done.filename))
                    .size(14.0)
                    .color(c(GREEN)),
                text(human_bytes(done.size)).size(12.0).color(c(MUTED)),
                text(done.path.display().to_string())
                    .size(11.0)
                    .color(c(DIM)),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        }
    } else {
        let percent = (receiver.progress * 100.0).round().clamp(0.0, 100.0) as u32;
        let title = if receiver.progress > 0.0 && !receiver.note.is_empty() {
            receiver.note.clone()
        } else {
            "等待二维码…（把发送端窗口置于可见位置）".to_string()
        };

        let mut card = column![
            row![
                text(title).size(13.0).color(c(TEXT)),
                Space::new().width(Length::Fill),
                text(format!("{percent}%")).size(13.0).color(c(BLUE)),
            ]
            .align_y(Alignment::Center),
            progress_bar(0.0..=1.0, receiver.progress as f32).style(main_bar),
        ]
        .spacing(8)
        .width(Length::Fill);

        if receiver.blocks.len() > 1 {
            card = card.push(block_strip(receiver));
        }
        card.into()
    };

    container(content)
        .padding(14)
        .width(Length::Fill)
        .style(panel)
        .into()
}

fn block_strip(receiver: &Receiver) -> Element<'_, Message> {
    let mut strip = row![].spacing(8);
    for (block, have, total) in receiver.blocks.iter().take(LIST_SHOWN) {
        let ratio = if *total == 0 {
            0.0
        } else {
            *have as f32 / *total as f32
        };
        strip = strip.push(
            container(
                column![
                    text(format!("block {block} {have}/{total}"))
                        .size(10.0)
                        .color(c(DIM)),
                    progress_bar(0.0..=1.0, ratio).style(block_bar),
                ]
                .spacing(3),
            )
            .width(Length::Fill),
        );
    }
    strip.into()
}

/// The text of a received text: a preview of the first lines plus a copy button.
///
/// The clipboard already has the whole thing (that is what the worker does on
/// arrival); the button is there for when the user has copied something since.
fn text_card<'a>(item: &'a TransferItem, body: &str, index: usize) -> Element<'a, Message> {
    let lines: Vec<&str> = body.lines().collect();
    let mut preview = column![].spacing(2);
    for line in lines.iter().take(TEXT_CARD_LINES) {
        preview = preview.push(
            text(if line.is_empty() { " ".to_string() } else { line.to_string() })
                .size(12.0)
                .color(c(TEXT)),
        );
    }
    if lines.len() > TEXT_CARD_LINES {
        preview = preview.push(
            text(format!(
                "…共 {} 行；点「复制」拿到完整文本",
                lines.len()
            ))
            .size(11.0)
            .color(c(DIM)),
        );
    }

    column![
        row![
            text("✓ 已收到文本").size(14.0).color(c(GREEN)),
            text(human_bytes(item.size)).size(12.0).color(c(MUTED)),
            text(if item.copied {
                "已复制到剪贴板，可直接粘贴"
            } else {
                "复制失败，点右侧按钮重试"
            })
            .size(11.0)
            .color(if item.copied { c(MUTED) } else { c(RED) }),
            Space::new().width(Length::Fill),
            button(text("复制").size(12.0))
                .style(secondary)
                .on_press(Message::Copy(index))
                .padding([4.0, 12.0]),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
        container(preview)
            .padding(10)
            .width(Length::Fill)
            .style(inset),
    ]
    .spacing(8)
    .into()
}

fn transfer_row(index: usize, item: &TransferItem) -> Element<'_, Message> {
    let detail: Element<'_, Message> = match &item.text {
        Some(_) if item.copied => text("已复制到剪贴板").size(11.0).color(c(MUTED)).into(),
        Some(_) => text("复制失败（点「复制」重试）")
            .size(11.0)
            .color(c(RED))
            .into(),
        None if !item.written => text(format!(
            "{}（已存在，未重复保存）",
            item.path.display()
        ))
        .size(11.0)
        .color(c(DIM))
        .into(),
        None => text(item.path.display().to_string())
            .size(11.0)
            .color(c(DIM))
            .into(),
    };

    let mut row_content = row![
        column![
            text(format!("✓ {}", item.filename)).size(13.0).color(c(TEXT)),
            detail,
        ]
        .spacing(2)
        .width(Length::Fill),
        text(human_bytes(item.size)).size(11.0).color(c(MUTED)),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    if item.text.is_some() {
        row_content = row_content.push(
            button(text("复制").size(11.0))
                .style(secondary)
                .on_press(Message::Copy(index))
                .padding([3.0, 10.0]),
        );
    }

    container(row_content)
        .padding(Padding {
            top: 8.0,
            right: 10.0,
            bottom: 8.0,
            left: 10.0,
        })
        .width(Length::Fill)
        .style(inset)
        .into()
}

fn picker(receiver: &Receiver) -> Element<'_, Message> {
    let preview = Preview {
        handle: receiver.preview.clone(),
        dims: receiver.preview_dims,
        full: receiver.preview_full,
        region: receiver
            .region
            .and_then(|(display, rect)| (display == receiver.preview_display).then_some(rect)),
    };

    let bar = container(
        row![
            text(format!(
                "在画面上拖拽框选二维码区域 · 当前显示第 {}/{} 块屏",
                receiver.preview_display + 1,
                receiver.display_count.max(1)
            ))
            .size(12.0)
            .color(c(MUTED)),
            Space::new().width(Length::Fill),
            button(text("◀").size(12.0))
                .style(secondary)
                .on_press(Message::PrevDisplay)
                .padding([4.0, 10.0]),
            button(text("▶").size(12.0))
                .style(secondary)
                .on_press(Message::NextDisplay)
                .padding([4.0, 10.0]),
            button(text("取消").size(12.0))
                .style(secondary)
                .on_press(Message::ClosePicker)
                .padding([4.0, 12.0]),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding(Padding {
        top: 8.0,
        right: 16.0,
        bottom: 8.0,
        left: 16.0,
    })
    .width(Length::Fill)
    .style(|_| container::Style {
        background: Some(Background::Color(c(PANEL))),
        ..container::Style::default()
    });

    column![
        bar,
        container(
            Canvas::new(preview)
                .width(Length::Fill)
                .height(Length::Fill)
        )
        .padding(12)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(c(PICKER_BG))),
            ..container::Style::default()
        }),
    ]
    .height(Length::Fill)
    .into()
}

fn footer(receiver: &Receiver) -> Element<'_, Message> {
    let line: Element<'_, Message> = if receiver.capture_black {
        text("⚠ 抓到的是全黑画面 —— 锁屏、独占全屏的游戏或 DRM 视频都会这样。切回普通桌面再试，或用「框选区域」只抓需要的那一块。")
            .size(11.0)
            .color(c(RED))
            .into()
    } else if let Some(error) = &receiver.last_error {
        text(error.clone()).size(11.0).color(c(RED)).into()
    } else {
        text("自动扫描所有显示器；需要缩小范围时点右上角「框选区域」。文件保存到「下载」文件夹，文本直接进剪贴板。")
            .size(11.0)
            .color(c(DIM))
            .into()
    };

    container(line)
        .padding(Padding {
            top: 8.0,
            right: 16.0,
            bottom: 8.0,
            left: 16.0,
        })
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(Background::Color(c(PANEL))),
            ..container::Style::default()
        })
        .into()
}

// ─── Style helpers ──────────────────────────────────────────────────────────

fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c(PANEL))),
        border: Border {
            color: c(BORDER),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..container::Style::default()
    }
}

fn inset(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c(BG))),
        border: Border {
            color: c(BORDER),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

fn secondary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(c(match status {
            button::Status::Hovered | button::Status::Pressed => BUTTON_HOVER,
            _ => BUTTON,
        }))),
        text_color: c(TEXT),
        border: Border {
            color: c(BORDER),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..button::Style::default()
    }
}

fn primary(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: Some(Background::Color(c(match status {
            button::Status::Hovered | button::Status::Pressed => PRIMARY_HOVER,
            _ => PRIMARY,
        }))),
        text_color: Color::WHITE,
        border: Border {
            color: c(PRIMARY),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..button::Style::default()
    }
}

fn main_bar(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(c(BG)),
        bar: Background::Color(c(BLUE)),
        border: Border {
            color: c(BORDER),
            width: 1.0,
            radius: 4.0.into(),
        },
    }
}

fn block_bar(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: Background::Color(c(BG)),
        bar: Background::Color(c(GREEN)),
        border: Border {
            color: c(BORDER),
            width: 1.0,
            radius: 3.0.into(),
        },
    }
}

// ─── Entry point ────────────────────────────────────────────────────────────

/// Show or hide the console window this process started with.
///
/// The exe keeps the default console subsystem so `--check` and `--headless`
/// still print; when it is *double-clicked* Windows gives it a console of its
/// own, which would otherwise sit behind the receiver window for the whole
/// session. Only the window is hidden — the console itself stays allocated, so
/// std's stdout keeps working and the fallback path below can still print.
///
/// A console shared with the launching shell (`cmd.exe` started us) is left
/// alone: hiding that would hide the user's terminal.
fn set_console_visible(visible: bool) {
    use windows_sys::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};

    // SAFETY: both calls take handles this process owns; GetConsoleWindow
    // returns null when there is no console at all, and the process list is
    // only read into a local array of the size we pass.
    unsafe {
        let window = GetConsoleWindow();
        if window.is_null() {
            return;
        }
        if !visible {
            let mut processes = [0u32; 8];
            let attached =
                GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32);
            if attached > 1 {
                return; // shared with a shell: leave it on screen
            }
        }
        ShowWindow(window, if visible { SW_SHOW } else { SW_HIDE });
    }
}

/// Run the receiver window. Returns when the user closes it.
pub fn run() -> iced::Result {
    set_console_visible(false);
    let result = iced::application(Receiver::new, update, view)
        .title("RaptorQR Receiver")
        .theme(|_: &Receiver| Theme::Dark)
        .window_size(Size::new(920.0, 640.0))
        .antialiasing(true)
        .subscription(subscription)
        .run();
    if result.is_err() {
        // main() falls back to the console receiver; give it a window again.
        set_console_visible(true);
    }
    result
}
