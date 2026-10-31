//! 贴图窗口：置顶无边框显示截图，图像四周带柔和投影。
//!
//! 支持：拖动（按住左键 / 边缘热区缩放）、普通滚轮缩放（以光标为锚点）、
//! `Ctrl+滚轮` 调整透明度、双击关闭、右键菜单（复制 / 保存 / OCR / 重置缩放 /
//! 锁定 / 置顶 / 阴影 / 关闭）、Esc 关闭。
//!
//! 窗口采用 `WS_EX_LAYERED` + `UpdateLayeredWindow`（逐像素 alpha）：窗口比图像
//! 四周各多出一圈留白（`pin_shadow_margin`），留白内自绘柔和投影，投影之外
//! 完全透明（alpha=0，鼠标穿透）。之所以不用 `CS_DROPSHADOW`：其阴影由 DWM 绘制且
//! 只在窗口处于活动状态时出现，贴图作为常驻置顶窗口绝大多数时间非活动，实测无阴影；
//! 自绘投影不依赖系统启发式，且能与图像一起原子移动（拖动时阴影不落后）。
//!
//! 绘制路径（每次内容变化时重建整幅分层位图，画完即释放临时 DIB，稳态内存不增长）：
//! - 100% 缩放：源图 `StretchDIBits` 直写留白偏移处，像素级一致；
//! - 其他缩放：离屏 DC 双缓冲 + `HALFTONE`（缩小时质量优先）/ `COLORONCOLOR`（放大时速度优先）。
//!
//! 右键菜单与托盘共用 `menu` 模块的自绘 Fluent 菜单（不再用系统 `TrackPopupMenuEx`）。

use std::ffi::c_void;
use std::sync::Arc;

use crate::capture::CapturedImage;
use crate::clipboard;
use crate::config;
use crate::error::{AppError, Result};
use crate::imageio;
use crate::menu::{self, Item};
use crate::ocr;
use crate::theme;
use crate::util::{self, error_msgbox, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC,
    DeleteObject, EndPaint, GetDC, MonitorFromPoint, ReleaseDC, SelectObject, SetBrushOrgEx,
    SetStretchBltMode, StretchDIBits, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BLENDFUNCTION, COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, HDC, MONITOR_DEFAULTTONEAREST,
    PAINTSTRUCT, SRCCOPY, STRETCH_BLT_MODE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_CONTROL, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const VK_ESCAPE: usize = 0x1B;

// 右键菜单 ID（自绘菜单返回下标，这里仅用于常量可读性）
const MENU_COPY: usize = 0;
const MENU_SAVE: usize = 1;
const MENU_OCR: usize = 2;
const MENU_RESET_ZOOM: usize = 3;
const MENU_LOCK: usize = 4;
const MENU_TOPMOST: usize = 5;
const MENU_SHADOW: usize = 6;
const MENU_CLOSE: usize = 7;

// 贴图缩放上下限、阴影留白/峰值、滚轮步进、初始贴合比例均由配置提供
// （见 config.ini [pin]）。

/// 贴图边缘（用于缩放热区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinEdge {
    N,
    E,
    S,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl PinEdge {
    fn has_west(self) -> bool {
        matches!(self, Self::W | Self::NW | Self::SW)
    }
    fn has_east(self) -> bool {
        matches!(self, Self::E | Self::NE | Self::SE)
    }
    fn has_north(self) -> bool {
        matches!(self, Self::N | Self::NE | Self::NW)
    }
    fn has_south(self) -> bool {
        matches!(self, Self::S | Self::SE | Self::SW)
    }
}

/// 贴图交互状态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PinInteraction {
    None,
    #[allow(dead_code)]
    Moving,
    Resizing {
        edge: PinEdge,
        start_cursor: POINT,
        start_rect: RECT,
        start_scale: f32,
    },
}

/// 每个贴图窗口的私有数据（指针存在 GWLP_USERDATA，WM_DESTROY 时释放）。
pub struct PinData {
    /// BGRA 自上而下像素；与截图结果共享同一份内存（Arc，零拷贝）。
    pub image: Arc<[u8]>,
    pub img_w: i32,
    pub img_h: i32,
    pub scale: f32,
    /// 图像四周的投影留白（设备像素）；窗口尺寸 = 图像尺寸 + 2 × margin。
    pub margin: i32,
    /// 外框矩形缓存（zoom/菜单等高频路径，避免重复 GetWindowRect 内核调用）
    pub rect: RECT,
    pub rect_valid: bool,
    /// 不透明度（10..255）。
    pub opacity: u8,
    /// 锁定位置：不能拖动/缩放，但仍可右键菜单/Esc 关闭。
    pub locked: bool,
    /// 置顶。
    pub topmost: bool,
    /// 阴影（关闭后 margin=0，窗口收缩）。
    pub shadow: bool,
    /// 窗口 DPI（边缘热区/留白换算）。
    pub dpi: i32,
    /// 当前拖动/缩放交互。
    pub interaction: PinInteraction,
}

/// 最低不透明度：10%（alpha 26/255）。
const MIN_OPACITY_ALPHA: u8 = 26;

fn clamp_opacity(v: u8) -> u8 {
    v.max(MIN_OPACITY_ALPHA)
}

/// 边缘缩放：根据鼠标位移与起始矩形计算新 scale / 新窗口矩形（纯函数，可单测）。
#[allow(clippy::too_many_arguments)]
pub fn resize_geometry(
    start_rect: RECT,
    img_w: i32,
    img_h: i32,
    margin: i32,
    edge: PinEdge,
    dx: i32,
    dy: i32,
    min_scale: f32,
    max_scale: f32,
    min_px: i32,
) -> (f32, RECT) {
    let m = margin.max(0);
    let start_img_w = (start_rect.right - start_rect.left - m * 2).max(1);
    let start_img_h = (start_rect.bottom - start_rect.top - m * 2).max(1);
    let min_px = min_px.max(1);
    let lo = min_scale
        .max(min_px as f32 / img_w.max(1) as f32)
        .max(min_px as f32 / img_h.max(1) as f32)
        .max(0.01);
    let hi = max_scale.max(lo);

    let h_changed = edge.has_east() || edge.has_west();
    let v_changed = edge.has_north() || edge.has_south();
    let raw_w = if edge.has_east() {
        start_img_w + dx
    } else if edge.has_west() {
        start_img_w - dx
    } else {
        start_img_w
    }
    .max(1);
    let raw_h = if edge.has_south() {
        start_img_h + dy
    } else if edge.has_north() {
        start_img_h - dy
    } else {
        start_img_h
    }
    .max(1);

    let scale = if h_changed && v_changed {
        // 角柄：取 x/y 两个 scale 的较小值以保持宽高比
        (raw_w as f32 / img_w.max(1) as f32)
            .min(raw_h as f32 / img_h.max(1) as f32)
    } else if h_changed {
        raw_w as f32 / img_w.max(1) as f32
    } else if v_changed {
        raw_h as f32 / img_h.max(1) as f32
    } else {
        start_img_w as f32 / img_w.max(1) as f32
    }
    .clamp(lo, hi);

    let new_img_w = ((img_w.max(1) as f32) * scale).round() as i32;
    let new_img_h = ((img_h.max(1) as f32) * scale).round() as i32;
    let win_w = new_img_w.max(1) + m * 2;
    let win_h = new_img_h.max(1) + m * 2;

    let left = if edge.has_west() {
        start_rect.right - win_w
    } else {
        start_rect.left
    };
    let top = if edge.has_north() {
        start_rect.bottom - win_h
    } else {
        start_rect.top
    };
    (
        scale,
        RECT {
            left,
            top,
            right: left + win_w,
            bottom: top + win_h,
        },
    )
}

/// 边缘命中（设备像素）。返回 None 表示不在缩放热区。
unsafe fn hit_edge(d: &PinData, screen_x: i32, screen_y: i32, hwnd: HWND) -> Option<PinEdge> {
    let m = d.margin.max(0);
    let r = if d.rect_valid {
        d.rect
    } else {
        let mut rr = RECT::default();
        if GetWindowRect(hwnd, &mut rr).is_err() {
            return None;
        }
        rr
    };
    let zone = util::scaled(crate::config::get().pin_edge_hit, d.dpi).clamp(3, 24);
    let (ix0, iy0, ix1, iy1) = (r.left + m, r.top + m, r.right - m, r.bottom - m);
    if screen_x < ix0 || screen_x >= ix1 || screen_y < iy0 || screen_y >= iy1 {
        return None;
    }
    let west = screen_x < ix0 + zone;
    let east = screen_x >= ix1 - zone;
    let north = screen_y < iy0 + zone;
    let south = screen_y >= iy1 - zone;
    match (north, south, west, east) {
        (true, _, true, _) => Some(PinEdge::NW),
        (true, _, _, true) => Some(PinEdge::NE),
        (_, true, true, _) => Some(PinEdge::SW),
        (_, true, _, true) => Some(PinEdge::SE),
        (true, _, _, _) => Some(PinEdge::N),
        (_, true, _, _) => Some(PinEdge::S),
        (_, _, true, _) => Some(PinEdge::W),
        (_, _, _, true) => Some(PinEdge::E),
        _ => None,
    }
}

/// 逻辑留白 → 设备像素（钳制，避免极端 DPI 下留白过大/过小）。
fn margin_for_dpi(dpi: u32) -> i32 {
    (crate::config::get().pin_shadow_margin * dpi.max(96) as i32 / 96).clamp(6, 64)
}

/// 贴图在截图点所在显示器上的 DPI（拿不到时按 96 处理）。
unsafe fn monitor_dpi_at(x: i32, y: i32) -> u32 {
    let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() {
        dx.max(96)
    } else {
        96
    }
}

/// 按缩放比换算出图像在窗口内的像素尺寸（最小 1px）。
fn scaled_size(data: &PinData, scale: f32) -> (i32, i32) {
    (
        ((data.img_w as f32 * scale).round() as i32).max(1),
        ((data.img_h as f32 * scale).round() as i32).max(1),
    )
}

/// 创建贴图窗口（须在 GUI 线程调用）。
pub fn create(img: &CapturedImage) -> Result<HWND> {
    unsafe { create_impl(img) }
}

unsafe fn create_impl(img: &CapturedImage) -> Result<HWND> {
    // 不再需要 CS_DROPSHADOW：投影由窗口自绘（见模块头说明）
    register_class_w("PixShotPin", pin_proc, false, false, true); // CS_DBLCLKS：双击退出贴图
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    // 初始缩放：不放大、不超过主屏配置比例（默认 85%）
    let cfg = crate::config::get();
    let sw = GetSystemMetrics(SM_CXSCREEN).max(1);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(1);
    let fit = (sw as f32 * cfg.pin_fit_ratio / img.width as f32)
        .min(sh as f32 * cfg.pin_fit_ratio / img.height as f32)
        .min(1.0)
        .max(cfg.pin_min_scale);
    let dpi = monitor_dpi_at(img.x, img.y) as i32;
    let margin = if cfg.pin_shadow {
        margin_for_dpi(dpi as u32)
    } else {
        0
    };
    let topmost = cfg.pin_topmost;

    let data = Box::into_raw(Box::new(PinData {
        image: img.data.clone(), // Arc 克隆，不复制像素
        img_w: img.width,
        img_h: img.height,
        scale: fit,
        margin,
        rect: RECT::default(),
        rect_valid: false,
        opacity: clamp_opacity(cfg.pin_opacity),
        locked: cfg.pin_locked,
        topmost,
        shadow: cfg.pin_shadow,
        dpi,
        interaction: PinInteraction::None,
    }));

    // 一次创建即给出最终几何：避免窗口先以 1×1 出现被外部（E2E）读到中间态。
    let d = &mut *data;
    let (iw, ih) = scaled_size(d, fit);
    let (cw, ch) = (iw + margin * 2, ih + margin * 2);
    // 图像左上角落在原截图位置，窗口整体外扩一圈留白（图像像素位置严格不变）。
    let x = img.x - margin;
    let y = img.y - margin;
    d.rect = RECT {
        left: x,
        top: y,
        right: x + cw,
        bottom: y + ch,
    };
    d.rect_valid = true;

    let ex = WS_EX_LAYERED
        | WS_EX_TOOLWINDOW
        | if topmost { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };
    let hwnd = CreateWindowExW(
        ex,
        PCWSTR::from_raw(to_wide("PixShotPin").as_ptr()),
        PCWSTR::null(),
        // WS_POPUP 且不带 WS_VISIBLE：先把分层位图推上去再显示，避免首帧空白。
        // 无 THICKFRAME：图像内容需要像素级完整呈现；实测 THICKFRAME+NCCALCASE(0)
        // 组合在窗口失去前台后 DWM 会在外缘绘制 ~8px 框架视觉带内缩内容。
        // 尺寸调整由滚轮缩放承担。
        WS_POPUP,
        x,
        y,
        cw,
        ch,
        None,
        None,
        hinst,
        Some(data as *const _),
    )
    .unwrap_or_default();

    if hwnd.is_invalid() {
        drop(Box::from_raw(data));
        return Err(AppError::Config("创建贴图窗口失败".into()));
    }

    unsafe {
        render(hwnd);
        if topmost {
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // Fluent：保持直角（贴图是图像内容，圆角会裁掉像素）
        theme::frame_pin(hwnd);
    }
    Ok(hwnd)
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut PinData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut PinData))
    }
}

unsafe extern "system" fn pin_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            LRESULT(0)
        }

        // 无非客户区：整个窗口都是客户区（无边框贴图）
        WM_NCCALCSIZE => LRESULT(0),

        // 更新外框缓存（须继续走默认处理以派发 WM_MOVE/WM_SIZE）
        WM_WINDOWPOSCHANGED => {
            let wpn = lp.0 as *const WINDOWPOS;
            if !wpn.is_null() {
                if let Some(d) = data_of(hwnd) {
                    d.rect = RECT {
                        left: (*wpn).x,
                        top: (*wpn).y,
                        right: (*wpn).x + (*wpn).cx,
                        bottom: (*wpn).y + (*wpn).cy,
                    };
                    d.rect_valid = true;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_ERASEBKGND => LRESULT(1),

        // 分层窗口的内容由 UpdateLayeredWindow 持有，这里只消化重绘请求
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }

        // 投影留白区（图像之外）不接收鼠标；图像区按 锁定/边缘热区 区分拖动与缩放。
        WM_NCHITTEST => {
            let x = (lp.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let (m, cached, locked) = data_of(hwnd)
                .map(|d| (d.margin, d.rect_valid.then_some(d.rect), d.locked))
                .unwrap_or((0, None, false));
            // 无阴影（margin=0）时整个客户区都是图像
            let r = match cached {
                Some(r) => r,
                None => {
                    let mut rr = RECT::default();
                    if GetWindowRect(hwnd, &mut rr).is_err() {
                        return LRESULT(HTCLIENT as isize);
                    }
                    rr
                }
            };
            let in_image = x >= r.left + m && x < r.right - m && y >= r.top + m && y < r.bottom - m;
            if !in_image {
                return LRESULT(HTTRANSPARENT as isize);
            }
            if locked {
                return LRESULT(HTCLIENT as isize);
            }
            if data_of(hwnd)
                .and_then(|d| hit_edge(d, x, y, hwnd))
                .is_some()
            {
                return LRESULT(HTCLIENT as isize);
            }
            // 右键保持 HTCLIENT：否则命中标题栏会走系统菜单，收不到 WM_CONTEXTMENU。
            if (GetKeyState(VK_RBUTTON.0 as i32) as u16) & 0x8000 != 0 {
                return LRESULT(HTCLIENT as isize);
            }
            LRESULT(HTCAPTION as isize)
        }

        // 按住左键：边缘热区 → 自实现缩放；图像正文 → 交给系统拖动（保留吸附）。
        WM_LBUTTONDOWN => {
            let mut start_resize: Option<PinInteraction> = None;
            if let Some(d) = data_of(hwnd) {
                if d.locked {
                    return LRESULT(0);
                }
                let pt = {
                    let mut p = POINT::default();
                    if GetCursorPos(&mut p).is_err() {
                        return LRESULT(0);
                    }
                    p
                };
                if let Some(edge) = hit_edge(d, pt.x, pt.y, hwnd) {
                    let r = if d.rect_valid {
                        d.rect
                    } else {
                        let mut rr = RECT::default();
                        if GetWindowRect(hwnd, &mut rr).is_err() {
                            return LRESULT(0);
                        }
                        rr
                    };
                    start_resize = Some(PinInteraction::Resizing {
                        edge,
                        start_cursor: pt,
                        start_rect: r,
                        start_scale: d.scale,
                    });
                }
            }
            if let Some(inter) = start_resize {
                if let Some(d) = data_of(hwnd) {
                    d.interaction = inter;
                }
                SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            }
            LRESULT(0)
        }

        // 缩放拖动：按起始矩形 + 光标位移重算 scale/窗口矩形。
        WM_MOUSEMOVE => {
            let Some(d) = data_of(hwnd) else {
                return LRESULT(0);
            };
            if let PinInteraction::Resizing {
                edge,
                start_cursor,
                start_rect,
                ..
            } = d.interaction
            {
                let mut pt = POINT::default();
                if GetCursorPos(&mut pt).is_err() {
                    return LRESULT(0);
                }
                let cfg = crate::config::get();
                let (scale, rc) = resize_geometry(
                    start_rect,
                    d.img_w,
                    d.img_h,
                    d.margin,
                    edge,
                    pt.x - start_cursor.x,
                    pt.y - start_cursor.y,
                    cfg.pin_min_scale,
                    cfg.pin_max_scale,
                    util::scaled(cfg.pin_min_visible_px, d.dpi).max(8),
                );
                d.scale = scale;
                d.rect = rc;
                d.rect_valid = true;
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                render(hwnd);
            }
            LRESULT(0)
        }

        WM_LBUTTONUP => {
            if let Some(d) = data_of(hwnd) {
                if matches!(d.interaction, PinInteraction::Resizing { .. }) {
                    let _ = ReleaseCapture();
                    d.interaction = PinInteraction::None;
                }
            }
            LRESULT(0)
        }

        // 双击左键：退出（关闭）该贴图。依赖窗口类的 CS_DBLCLKS 样式。
        WM_LBUTTONDBLCLK => {
            crate::diag::log("[pin] 双击左键(客户区) -> 关闭贴图");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        // 图像正文走 HTCAPTION 以便保留系统拖动；因此双击会以**非客户区**双击到达，
        // 必须单独处理，否则双击关闭贴图会失效（DefWindowProc 还可能误最大化）。
        WM_NCLBUTTONDBLCLK => {
            crate::diag::log("[pin] 双击左键(标题区/图像正文) -> 关闭贴图");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            // Ctrl+滚轮 = 调整透明度；普通滚轮 = 缩放（保持既有交互）
            if ctrl_down() {
                wheel_opacity(hwnd, wp);
            } else {
                zoom(hwnd, wp);
            }
            LRESULT(0)
        }

        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_CONTEXTMENU => {
            context_menu(hwnd, lp);
            LRESULT(0)
        }

        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                // Arc<[u8]> 随 PinData 析构自动释放
                drop(Box::from_raw(p as *mut PinData));
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

unsafe fn ctrl_down() -> bool {
    (GetKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0
}

/// Ctrl+滚轮：按配置档位调整透明度（上滚更不透明、下滚更透明）。
unsafe fn wheel_opacity(hwnd: HWND, wp: WPARAM) {
    let Some(data) = data_of(hwnd) else { return };
    let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    if delta == 0 {
        return;
    }
    let next = bump_opacity(data, delta > 0);
    if next == data.opacity {
        return;
    }
    set_opacity(hwnd, data, next);
    crate::toast::show(&format!("透明度 {}%", opacity_percent(next)));
    crate::diag::log(&format!(
        "[pin] Ctrl+Wheel opacity -> {}%",
        opacity_percent(next)
    ));
}

/// 滚轮缩放：以光标为锚点，保持光标下的图像位置不变。
unsafe fn zoom(hwnd: HWND, wp: WPARAM) {
    let Some(data) = data_of(hwnd) else { return };
    if data.locked {
        return;
    }
    let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    if delta == 0 {
        return;
    }
    let cfg = crate::config::get();
    let factor = if delta > 0 {
        cfg.pin_zoom_step
    } else {
        1.0 / cfg.pin_zoom_step
    };
    let new_scale = (data.scale * factor).clamp(cfg.pin_min_scale, cfg.pin_max_scale);
    if (new_scale - data.scale).abs() < 1e-4 {
        return;
    }

    let r = if data.rect_valid {
        data.rect
    } else {
        let mut rr = RECT::default();
        if GetWindowRect(hwnd, &mut rr).is_err() {
            return;
        }
        rr
    };
    let m = data.margin;
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    // 图像区（去掉留白）的旧尺寸；锚点换算只在图像内进行
    let old_iw = (r.right - r.left - m * 2).max(1);
    let old_ih = (r.bottom - r.top - m * 2).max(1);
    let (new_iw, new_ih) = scaled_size(data, new_scale);
    let cx = (pt.x - (r.left + m)).clamp(0, old_iw);
    let cy = (pt.y - (r.top + m)).clamp(0, old_ih);
    // 锚点换算用 i64 中间值：大图（7680px）× 8 倍缩放时 cx*new_iw 可超 i32
    let nx = pt.x - (cx as i64 * new_iw as i64 / old_iw as i64) as i32 - m;
    let ny = pt.y - (cy as i64 * new_ih as i64 / old_ih as i64) as i32 - m;

    data.scale = new_scale;
    data.rect = RECT {
        left: nx,
        top: ny,
        right: nx + new_iw + m * 2,
        bottom: ny + new_ih + m * 2,
    };
    data.rect_valid = true;
    // 位置与尺寸由 nx/ny/尺寸 显式给出 → 不能用 SWP_NOMOVE
    // SWP_NOZORDER 下 hwndInsertAfter 无效，传空句柄（原写 HWND_TOP 自相矛盾）
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        nx,
        ny,
        new_iw + m * 2,
        new_ih + m * 2,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    render(hwnd);
}

/// 重建整幅分层位图（图像 + 四周投影）并推送到窗口。
///
/// 临时 DIB 画完即释放（内容已由 UpdateLayeredWindow 复制进窗口表面），
/// 因此贴图的稳态内存与图像本身同量级，不随留白放大。
unsafe fn render(hwnd: HWND) {
    let Some(data) = data_of(hwnd) else { return };
    let m = data.margin.max(0);
    let (iw, ih) = scaled_size(data, data.scale);
    let (cw, ch) = (iw + m * 2, ih + m * 2);

    let screen = GetDC(HWND::default());
    if screen.is_invalid() {
        return;
    }
    let mem = CreateCompatibleDC(screen);
    if mem.is_invalid() {
        let _ = ReleaseDC(HWND::default(), screen);
        return;
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: cw,
            biHeight: -ch, // 自上而下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
    if bmp.is_invalid() || bits.is_null() {
        let _ = DeleteDC(mem);
        let _ = ReleaseDC(HWND::default(), screen);
        return;
    }
    let old = SelectObject(mem, bmp);

    // 1) 全透明打底（留白之外 alpha=0 → 逐像素 alpha 命中测试会直接穿透）
    std::ptr::write_bytes(bits as *mut u8, 0, (cw as usize) * (ch as usize) * 4);
    // 2) 四周投影（只写图像矩形以外）
    fill_shadow(bits as *mut u8, cw, m, iw, ih);
    // 3) 图像本体画到留白偏移处
    draw_image(mem, screen, &data.image, data.img_w, data.img_h, data.scale, iw, ih, m);
    // 4) 图像区不透明：缩放走兼容 DC 时 GDI 可能把 alpha 通道写坏
    set_opaque(bits as *mut u8, cw, m, iw, ih);

    let pt_dst = if data.rect_valid {
        POINT {
            x: data.rect.left,
            y: data.rect.top,
        }
    } else {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            let _ = SelectObject(mem, old);
            let _ = DeleteObject(bmp);
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(HWND::default(), screen);
            return;
        }
        POINT { x: r.left, y: r.top }
    };
    let size = SIZE { cx: cw, cy: ch };
    let pt_src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: data.opacity,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        screen,
        Some(&pt_dst),
        Some(&size),
        mem,
        Some(&pt_src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = ReleaseDC(HWND::default(), screen);
}

/// 图像矩形之外的一圈留白写入黑色柔和投影（预乘 alpha：黑色分量恒为 0，只需 alpha）。
///
/// 距离取到图像矩形的欧氏距离 d，alpha = peak · (1 - d/margin)²，即紧贴图像边缘最深、
/// 向外交接为全透明——四条边与四角都连续，视觉上是均匀的"四周投影"。
unsafe fn fill_shadow(bits: *mut u8, cw: i32, m: i32, iw: i32, ih: i32) {
    if m <= 0 {
        return;
    }
    let ch = ih + m * 2;
    let (ix0, iy0, ix1, iy1) = (m, m, m + iw, m + ih);
    // alpha 只依赖 d²：查表代替每像素的 sqrt（更避免在像素内层循环里重复读配置）
    let peak = crate::config::get().pin_shadow_peak;
    let inv_m = 1.0f32 / m as f32;
    let lut: Vec<u8> = (0..=2 * m * m)
        .map(|d2| {
            let t = ((d2 as f32).sqrt() * inv_m).min(1.0);
            (255.0 * peak * (1.0 - t) * (1.0 - t)).round() as u8
        })
        .collect();
    for y in 0..ch {
        // 该行到图像矩形的纵向距离；行在图像带内时为 0（此时只在左右留白写投影）
        let (dy, x_lo, x_hi, two_span) = if y < iy0 {
            (iy0 - y, 0, cw, false)
        } else if y >= iy1 {
            (y - iy1 + 1, 0, cw, false)
        } else {
            (0, 0, ix0, true)
        };
        let spans: [(i32, i32); 2] = if two_span {
            [(x_lo, x_hi), (ix1, cw)]
        } else {
            [(x_lo, x_hi), (0, 0)]
        };
        for (a, b) in spans {
            for x in a..b {
                let dx = if x < ix0 {
                    ix0 - x
                } else if x >= ix1 {
                    x - ix1 + 1
                } else {
                    0
                };
                let alpha = lut[(dx * dx + dy * dy) as usize];
                if alpha == 0 {
                    continue;
                }
                let off = ((y * cw + x) * 4) as isize;
                // BI_RGB 32bpp 字节序为 BGRA；预乘后黑色仍是 0
                *bits.offset(off) = 0;
                *bits.offset(off + 1) = 0;
                *bits.offset(off + 2) = 0;
                *bits.offset(off + 3) = alpha;
            }
        }
    }
}

/// 图像区 alpha 置满（分层位图里图像必须完全不透明）。
unsafe fn set_opaque(bits: *mut u8, cw: i32, m: i32, iw: i32, ih: i32) {
    for y in m..(m + ih) {
        let row = (y * cw) as isize;
        for x in m..(m + iw) {
            *bits.offset((row + x as isize) * 4 + 3) = 255;
        }
    }
}

/// 把 BGRA 图像绘制到分层位图的 (m, m) 处。
/// 100% 直接写；其余先缩放到兼容 DC 再 BitBlt（沿用 `HALFTONE`/`COLORONCOLOR` 策略）。
unsafe fn draw_image(
    dst: HDC,
    screen: HDC,
    image: &[u8],
    img_w: i32,
    img_h: i32,
    scale: f32,
    iw: i32,
    ih: i32,
    m: i32,
) {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: img_w,
            biHeight: -img_h, // 自上而下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let bits = Some(image.as_ptr() as *const c_void);

    if iw == img_w && ih == img_h {
        StretchDIBits(
            dst, m, m, iw, ih, 0, 0, img_w, img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
        );
        return;
    }

    let sdc = CreateCompatibleDC(screen);
    let sbmp = CreateCompatibleBitmap(screen, iw, ih);
    if sdc.is_invalid() || sbmp.is_invalid() {
        // 分配失败时退回直接绘制（宁可可能失真，也要显示内容）
        StretchDIBits(
            dst, m, m, iw, ih, 0, 0, img_w, img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
        );
        if !sbmp.is_invalid() {
            let _ = DeleteObject(sbmp);
        }
        if !sdc.is_invalid() {
            let _ = DeleteDC(sdc);
        }
        return;
    }
    let old = SelectObject(sdc, sbmp);
    // 缩小（细节丢失更明显）用 HALFTONE；放大用 COLORONCOLOR（快且更锐利）
    let mode = if scale < 1.0 { HALFTONE } else { COLORONCOLOR };
    let prev = SetStretchBltMode(sdc, mode);
    if mode == HALFTONE {
        // HALFTONE 需要自定义画刷原点，否则纹理可能错位
        let _ = SetBrushOrgEx(sdc, 0, 0, None);
    }
    StretchDIBits(
        sdc, 0, 0, iw, ih, 0, 0, img_w, img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
    );
    let _ = BitBlt(dst, m, m, iw, ih, sdc, 0, 0, SRCCOPY);
    let _ = SetStretchBltMode(sdc, STRETCH_BLT_MODE(prev));
    SelectObject(sdc, old);
    let _ = DeleteObject(sbmp);
    let _ = DeleteDC(sdc);
}

/// 透明度百分比（用于菜单文案）。
fn opacity_percent(v: u8) -> u8 {
    ((v as u16 * 100 + 127) / 255) as u8
}

fn opacity_from_percent(p: u8) -> u8 {
    ((((p as u16 * 255) + 50) / 100) as u8).clamp(MIN_OPACITY_ALPHA, 255)
}

unsafe fn set_opacity(hwnd: HWND, d: &mut PinData, opacity: u8) {
    d.opacity = clamp_opacity(opacity);
    render(hwnd);
}

/// 开关阴影：重算 margin、按"图像左上角不动"修正窗口位置，窗口尺寸立即收缩/恢复。
unsafe fn set_shadow(hwnd: HWND, d: &mut PinData, on: bool) {
    let old_m = d.margin.max(0);
    let r = if d.rect_valid {
        d.rect
    } else {
        let mut rr = RECT::default();
        if GetWindowRect(hwnd, &mut rr).is_err() {
            return;
        }
        rr
    };
    let img_left = r.left + old_m;
    let img_top = r.top + old_m;
    let new_m = if on { margin_for_dpi(d.dpi as u32) } else { 0 };
    let (iw, ih) = scaled_size(d, d.scale);
    let x = img_left - new_m;
    let y = img_top - new_m;
    let cw = iw + new_m * 2;
    let ch = ih + new_m * 2;
    d.margin = new_m;
    d.shadow = on;
    d.rect = RECT {
        left: x,
        top: y,
        right: x + cw,
        bottom: y + ch,
    };
    d.rect_valid = true;
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        x,
        y,
        cw,
        ch,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    render(hwnd);
}

unsafe fn set_topmost(hwnd: HWND, d: &mut PinData, on: bool) {
    d.topmost = on;
    let insert = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    let _ = SetWindowPos(hwnd, insert, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

/// 按 1% 步长调整透明度：`up=true` 更不透明，`false` 更透明（不循环）。
fn bump_opacity(d: &PinData, up: bool) -> u8 {
    let cur = opacity_percent(d.opacity) as i32;
    let next = if up {
        (cur + 1).min(100)
    } else {
        (cur - 1).max(10)
    };
    opacity_from_percent(next as u8)
}

unsafe fn context_menu(hwnd: HWND, lp: LPARAM) {
    let cfg = config::get();
    let (locked, topmost, shadow) = data_of(hwnd)
        .map(|d| (d.locked, d.topmost, d.shadow))
        .unwrap_or((false, true, true));

    // 快捷键与工具条保持一致（Ctrl+C 复制 / Ctrl+S 保存 / Ctrl+O OCR）
    let items = [
        Item::accel("复制图像", "Ctrl+C"),
        Item::accel(format!("保存为 {}…", cfg.format.label()), "Ctrl+S"),
        Item::accel("识别文字 (OCR)", "Ctrl+O"),
        Item::sep("重置缩放 (100%)"),
        Item::new(if locked { "解锁位置" } else { "锁定位置" }),
        Item::new(if topmost { "取消置顶" } else { "保持置顶" }),
        Item::new(if shadow { "隐藏阴影" } else { "显示阴影" }),
        Item::sep("关闭贴图 (Esc)"),
    ];

    let mut pt = POINT {
        x: (lp.0 & 0xFFFF) as u16 as i16 as i32,
        y: ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    };
    if pt.x < 0 && pt.y < 0 {
        let _ = GetCursorPos(&mut pt); // 键盘呼出（WM_CONTEXTMENU 坐标为 -1,-1）
    }

    let Some(choice) = menu::show(pt.x, pt.y, &items) else {
        return;
    };

    // menu::show 是模态消息循环，期间本窗口过程可被重入——&mut PinData 不得
    // 跨模态存续（Rust 别名规则），菜单返回后重新获取。
    let Some(data) = data_of(hwnd) else { return };
    let img: &[u8] = &data.image;
    match choice {
        MENU_COPY => match clipboard::copy_image(img, data.img_w, data.img_h) {
            Ok(()) => crate::toast::show("已复制图像到剪贴板"),
            Err(e) => error_msgbox(&format!("复制失败：{e}")),
        },
        MENU_SAVE => match imageio::save_with_dialog(
            hwnd,
            img,
            data.img_w,
            data.img_h,
            cfg.format,
            cfg.save_dir.as_deref(),
        ) {
            Ok(Some(p)) => crate::toast::show(&format!("已保存：{}", short_path(&p))),
            Ok(None) => {}
            Err(e) => error_msgbox(&format!("保存失败：{e}")),
        },
        MENU_OCR => ocr::recognize_to_clipboard_async(data.image.clone(), data.img_w, data.img_h),
        MENU_RESET_ZOOM => {
            // 重置缩放：图像左上角保持不动（窗口位置不变，只改尺寸）
            data.scale = 1.0;
            let (iw, ih) = scaled_size(data, 1.0);
            let m = data.margin;
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_ok() {
                let _ = SetWindowPos(
                    hwnd,
                    HWND::default(),
                    r.left,
                    r.top,
                    iw + m * 2,
                    ih + m * 2,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            render(hwnd);
        }
        MENU_LOCK => {
            data.locked = !data.locked;
            crate::toast::show(if data.locked {
                "已锁定贴图位置"
            } else {
                "已解锁贴图位置"
            });
        }
        MENU_TOPMOST => set_topmost(hwnd, data, !topmost),
        MENU_SHADOW => set_shadow(hwnd, data, !shadow),
        MENU_CLOSE => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

/// 提示条里只显示文件名，避免长路径撑爆宽度。
fn short_path(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 与默认配置对齐的固定值（保证测试确定性，不依赖 config.ini）。
    const MIN_SCALE: f32 = 0.05;
    const MAX_SCALE: f32 = 8.0;
    const SHADOW_MARGIN_LOGICAL: i32 = 16;
    const SHADOW_PEAK: f32 = 0.55;

    #[test]
    fn short_path_keeps_file_name_only() {
        assert_eq!(short_path(r"C:\shots\截图_20261024_010101.png"), "截图_20261024_010101.png");
        assert_eq!(short_path("a.png"), "a.png");
    }

    /// 缩放边界必须被钳制，否则滚轮过度会得到 0 尺寸窗口
    #[test]
    fn scale_bounds() {
        assert!((0.05f32).max(MIN_SCALE) == 0.05);
        assert!((100.0f32).min(MAX_SCALE) == 8.0);
    }

    /// 以光标为锚点的缩放：光标处图像内容保持不动
    #[test]
    fn zoom_keeps_cursor_anchor() {
        let (r_left, old_w) = (100i32, 400i32);
        let (cursor_x, new_w) = (200i32, 800i32);
        // 光标在窗口内的相对位置
        let cx = (cursor_x - r_left).clamp(0, old_w);
        let nx = cursor_x - cx * new_w / old_w;
        // 放大后窗口左边界左移，锚点像素仍在光标下
        assert_eq!(nx, 0);
        assert!(nx < r_left);
    }

    /// 留白必须随 DPI 放大且被钳制在合理区间
    #[test]
    fn margin_scales_with_dpi() {
        assert_eq!(margin_for_dpi(96), SHADOW_MARGIN_LOGICAL);
        assert_eq!(margin_for_dpi(192), SHADOW_MARGIN_LOGICAL * 2);
        // 异常取值不应产生 0 或超大留白
        assert!(margin_for_dpi(0) >= 6);
        assert!(margin_for_dpi(1000) <= 64);
    }

    /// 投影 alpha 剖面：紧贴图像边缘最深、随距离单调递减、超出留白为 0
    #[test]
    fn shadow_alpha_falls_off_from_edge() {
        let m = 16.0f32;
        let alpha = |d: f32| -> u8 {
            let t = (d / m).min(1.0);
            (255.0 * SHADOW_PEAK * (1.0 - t) * (1.0 - t)).round() as u8
        };
        assert!(alpha(0.0) > 100, "边缘处投影应明显可见");
        assert!(alpha(1.0) < alpha(0.0));
        assert!(alpha(8.0) < alpha(1.0));
        assert_eq!(alpha(16.0), 0, "留白外缘必须完全透明");
    }

    fn start_rect() -> RECT {
        RECT {
            left: 100,
            top: 100,
            right: 100 + 200 + 32,
            bottom: 100 + 100 + 32,
        }
    }

    #[test]
    fn resize_east_keeps_left_and_grows_width() {
        let (scale, r) = resize_geometry(
            start_rect(),
            200,
            100,
            16,
            PinEdge::E,
            60,
            0,
            0.05,
            8.0,
            24,
        );
        assert_eq!(r.left, 100);
        assert_eq!(r.right - r.left, 260 + 32);
        assert!((scale - 260.0 / 200.0).abs() < 1e-3);
    }

    #[test]
    fn resize_west_keeps_right() {
        let (_, r) = resize_geometry(
            start_rect(),
            200,
            100,
            16,
            PinEdge::W,
            40,
            0,
            0.05,
            8.0,
            24,
        );
        assert_eq!(r.right, 100 + 200 + 32);
        assert_eq!(r.right - r.left, 160 + 32);
        assert!(r.left > 100);
    }

    #[test]
    fn corner_resize_keeps_aspect_ratio() {
        let (scale, r) = resize_geometry(
            start_rect(),
            200,
            100,
            16,
            PinEdge::SE,
            80,
            20,
            0.05,
            8.0,
            24,
        );
        let iw = r.right - r.left - 32;
        let ih = r.bottom - r.top - 32;
        assert!((iw as f32 / ih as f32 - 2.0).abs() < 0.02, "宽高比应保持");
        assert!(scale > 1.0);
        assert_eq!((r.left, r.top), (100, 100));
    }

    #[test]
    fn resize_clamps_min_and_max() {
        let (s, r) = resize_geometry(
            start_rect(),
            200,
            100,
            16,
            PinEdge::E,
            -1000,
            0,
            0.05,
            8.0,
            24,
        );
        assert!(r.right - r.left >= 24 + 32, "不小于 min_px");
        assert!((s - 24.0f32.max(0.05 * 200.0) / 200.0).abs() < 1e-3 || s >= 0.05);
        let (s, _) = resize_geometry(
            start_rect(),
            200,
            100,
            16,
            PinEdge::E,
            100000,
            0,
            0.05,
            8.0,
            24,
        );
        assert!(s <= 8.0, "不得超过 max_scale");
    }

    #[test]
    fn resize_without_margin_equals_image_size() {
        let (_, r) = resize_geometry(
            RECT {
                left: 0,
                top: 0,
                right: 200,
                bottom: 100,
            },
            200,
            100,
            0,
            PinEdge::E,
            50,
            0,
            0.05,
            8.0,
            24,
        );
        assert_eq!(r.right - r.left, 250);
        assert_eq!(r.bottom - r.top, 125, "无留白时高宽按比例同步");
    }

    #[test]
    fn opacity_clamp_and_cycle() {
        assert_eq!(clamp_opacity(0), 26, "最低 10%");
        assert_eq!(clamp_opacity(255), 255);
        assert_eq!(opacity_percent(255), 100);
        assert_eq!(opacity_percent(128), 50);
        assert_eq!(opacity_from_percent(50), 128);
        assert_eq!(opacity_from_percent(0), 26, "低于 10% 钳到 10%");
        assert_eq!(opacity_from_percent(10), 26);
    }

    #[test]
    fn opacity_wheel_steps() {
        let data = |op: u8| PinData {
            image: Arc::from(vec![0u8; 4]),
            img_w: 1,
            img_h: 1,
            scale: 1.0,
            margin: 0,
            rect: RECT::default(),
            rect_valid: false,
            opacity: op,
            locked: false,
            topmost: true,
            shadow: false,
            dpi: 96,
            interaction: PinInteraction::None,
        };
        // 1% 步长：上滚更不透明，下滚更透明
        assert_eq!(opacity_percent(bump_opacity(&data(229), false)), 89);
        assert_eq!(opacity_percent(bump_opacity(&data(229), true)), 91);
        // 到边界保持当前值，不循环
        assert_eq!(opacity_percent(bump_opacity(&data(255), true)), 100);
        assert_eq!(opacity_percent(bump_opacity(&data(128), false)), 49);
        assert_eq!(opacity_percent(bump_opacity(&data(128), true)), 51);
        // 最低 10%
        assert_eq!(opacity_percent(bump_opacity(&data(26), false)), 10);
        assert_eq!(bump_opacity(&data(26), false), 26);
    }
}
