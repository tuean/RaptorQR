//! Windows screen capture through GDI (`BitBlt`), for machines without a GPU.
//!
//! xcap's Windows backend is DXGI desktop duplication: it needs a real D3D11
//! adapter, and on a GPU-less box `IDXGIOutputDuplication` is simply unavailable,
//! so the grab fails (that is why this repository never had working Windows
//! capture). GDI needs no device at all — the desktop is composited in software
//! by the system, we blit the pixels into a DIB section and read them straight
//! out of its bitmap bits.
//!
//! The cost model is the *opposite* of macOS: `BitBlt` is proportional to the
//! copied area, so grabbing only the user's selected region is a real win here
//! (on macOS `CGWindowListCreateImage` costs the same 11–18 ms for any rect, and
//! only the capture *rate* matters).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, MONITORINFOEXW,
    SRCCOPY,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::capture::ScreenRect;

/// `MONITORINFOF_PRIMARY` lives in `winuser.h`; windows-sys does not expose it
/// for the GDI module, and the value is stable ABI.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// How long a monitor enumeration is reused — `EnumDisplayMonitors` is a
/// user32 round-trip and the capture loop asks for the list every tick.
const MONITOR_CACHE_TTL: Duration = Duration::from_secs(2);

thread_local! {
    static MONITORS: RefCell<Option<(Instant, Vec<Monitor>)>> = const { RefCell::new(None) };
    static TARGET: RefCell<Option<Target>> = const { RefCell::new(None) };
}

/// One monitor, positioned in **virtual-screen** coordinates.
///
/// The virtual screen is the desktop DC's coordinate space: the primary monitor
/// sits at (0,0) and a secondary one to its left has negative x. Blitting uses
/// these coordinates; regions (which the UI picks from a single monitor's
/// preview) are monitor-local and get translated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub primary: bool,
    pub name: String,
}

/// Ask Windows for physical pixels instead of logical ones.
///
/// Without this a 125 % display reports (and blits) a scaled 1536×864 desktop,
/// which would misplace every region the user selects. winit already does this
/// for the GUI, but `--headless` and `--check` never create a window, and the
/// first call wins: a second call fails harmlessly with ERROR_ACCESS_DENIED.
pub fn dpi_aware() {
    unsafe {
        // SAFETY: no arguments, and a failure only means someone beat us to it.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Connected monitors, primary first — the same order as [`crate::capture`].
pub fn monitors() -> Vec<Monitor> {
    dpi_aware();
    MONITORS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let stale = match slot.as_ref() {
            None => true,
            Some((at, list)) => at.elapsed() > MONITOR_CACHE_TTL || list.is_empty(),
        };
        if stale {
            let mut list = enum_monitors();
            // Stable sort: multiple adapters keep their enumeration order.
            list.sort_by_key(|m| !m.primary);
            *slot = Some((Instant::now(), list));
        }
        slot.as_ref().map(|(_, l)| l.clone()).unwrap_or_default()
    })
}

fn enum_monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _clip: *mut RECT,
        data: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY: `data` is the `Vec<Monitor>` pointer passed in below, and the
        // callback only runs for the duration of that `EnumDisplayMonitors` call.
        let out = &mut *(data as *mut Vec<Monitor>);

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let ok = GetMonitorInfoW(
            hmon,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        );
        if ok != 0 {
            let r = info.monitorInfo.rcMonitor;
            out.push(Monitor {
                x: r.left,
                y: r.top,
                w: (r.right - r.left).max(0) as u32,
                h: (r.bottom - r.top).max(0) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                name: device_name(&info.szDevice),
            });
        }
        TRUE
    }

    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        // SAFETY: the pointer stays valid for the call, and `collect` writes
        // through it exactly as modelled above.
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            &mut list as *mut Vec<Monitor> as LPARAM,
        );
    }
    list
}

/// `\\.\DISPLAY1` → `DISPLAY1`; the device path prefix is noise in a UI.
fn device_name(device: &[u16; 32]) -> String {
    let end = device.iter().position(|c| *c == 0).unwrap_or(device.len());
    let raw = String::from_utf16_lossy(&device[..end]);
    raw.rsplit('\\').next().unwrap_or(&raw).to_string()
}

/// Grab `rect` (monitor-local, `None` = the whole monitor) as **BGRA**.
///
/// Rows are top-down: the DIB is created with a negative height so row 0 is the
/// top of the screen and no vertical flip is needed anywhere downstream.
pub fn capture_bgra(m: &Monitor, rect: Option<&ScreenRect>) -> Option<(Vec<u8>, u32, u32)> {
    let full = ScreenRect {
        x: 0,
        y: 0,
        w: m.w,
        h: m.h,
    };
    let r = match rect {
        Some(r) => r.intersect(&full),
        None => full,
    };
    if r.w == 0 || r.h == 0 {
        return None;
    }

    TARGET.with(|slot| {
        let mut slot = slot.borrow_mut();
        let target = slot.get_or_insert_with(Target::empty);
        if !target.ensure(r.w as i32, r.h as i32) {
            return None;
        }

        // SAFETY: plain GDI calls on handles this module owns; the desktop DC is
        // released again on every path, and the DIB's bits stay valid for as long
        // as `target` is alive (we copy out before returning).
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            if screen.is_null() {
                return None;
            }
            let ok = BitBlt(
                target.hdc,
                0,
                0,
                r.w as i32,
                r.h as i32,
                screen,
                m.x + r.x as i32,
                m.y + r.y as i32,
                SRCCOPY | CAPTUREBLT,
            );
            ReleaseDC(std::ptr::null_mut(), screen);
            if ok == 0 {
                return None;
            }

            let len = r.w as usize * r.h as usize * 4;
            let mut out = vec![0u8; len];
            std::ptr::copy_nonoverlapping(target.bits, out.as_mut_ptr(), len);
            // A DIB section has no alpha channel, and `BitBlt` leaves the fourth
            // byte at 0 — which an image widget would read as fully transparent.
            for pixel in out.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            Some((out, r.w, r.h))
        }
    })
}

/// A memory DC with a 32-bpp DIB selected into it, reused across frames.
///
/// Allocating this per grab was measurable (a 1920×1080 DIB is 8 MB): the
/// display is recreated only when the requested size changes.
struct Target {
    hdc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Target {
    fn empty() -> Self {
        Self {
            hdc: std::ptr::null_mut(),
            bmp: std::ptr::null_mut(),
            old: std::ptr::null_mut(),
            bits: std::ptr::null_mut(),
            w: 0,
            h: 0,
        }
    }

    fn ensure(&mut self, w: i32, h: i32) -> bool {
        if w == self.w && h == self.h && !self.bits.is_null() {
            return true;
        }
        self.release();
        if w <= 0 || h <= 0 {
            return false;
        }

        // SAFETY: fresh handles, checked before use; `bmi` is a plain POD struct.
        unsafe {
            let hdc = CreateCompatibleDC(std::ptr::null_mut());
            if hdc.is_null() {
                return false;
            }

            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                // Negative height = top-down rows. Without this every frame would
                // arrive upside down and no QR code would decode.
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(
                hdc,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if bmp.is_null() || bits.is_null() {
                DeleteDC(hdc);
                return false;
            }
            let old = SelectObject(hdc, bmp as HGDIOBJ);
            self.hdc = hdc;
            self.bmp = bmp;
            self.old = old;
            self.bits = bits as *mut u8;
            self.w = w;
            self.h = h;
            true
        }
    }

    fn release(&mut self) {
        if self.hdc.is_null() {
            return;
        }
        // SAFETY: handles were created by `ensure` and are released only here.
        unsafe {
            if !self.bmp.is_null() {
                if !self.old.is_null() {
                    SelectObject(self.hdc, self.old);
                }
                DeleteObject(self.bmp as HGDIOBJ);
            }
            DeleteDC(self.hdc);
        }
        self.hdc = std::ptr::null_mut();
        self.bmp = std::ptr::null_mut();
        self.old = std::ptr::null_mut();
        self.bits = std::ptr::null_mut();
        self.w = 0;
        self.h = 0;
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.release();
    }
}
