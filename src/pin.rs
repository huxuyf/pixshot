//! 贴图窗口：置顶无边框显示截图，图像四周带柔和投影。
//!
//! 支持：拖动（按住左键）、滚轮缩放（以光标为锚点）、双击关闭、
//! 右键菜单（复制图像 / 保存（按配置格式）/ OCR / 重置缩放 / 关闭）、Esc 关闭。
//!
//! 窗口采用 `WS_EX_LAYERED` + `UpdateLayeredWindow`（逐像素 alpha）：窗口比图像
//! 四周各多出一圈留白（[`SHADOW_MARGIN_LOGICAL`]），留白内自绘柔和投影，投影之外
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
use crate::util::{error_msgbox, register_class_w, to_wide};
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
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::*;

const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 8.0;

const VK_ESCAPE: usize = 0x1B;

/// 阴影留白（逻辑像素）——按窗口所在显示器 DPI 换算成设备像素。
const SHADOW_MARGIN_LOGICAL: i32 = 16;
/// 紧贴图像边缘处的投影峰值不透明度。
const SHADOW_PEAK: f32 = 0.55;

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
}

/// 逻辑留白 → 设备像素（钳制，避免极端 DPI 下留白过大/过小）。
fn margin_for_dpi(dpi: u32) -> i32 {
    (SHADOW_MARGIN_LOGICAL * dpi.max(96) as i32 / 96).clamp(6, 64)
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

    // 初始缩放：不放大、不超过主屏 85%
    let sw = GetSystemMetrics(SM_CXSCREEN).max(1);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(1);
    let fit = (sw as f32 * 0.85 / img.width as f32)
        .min(sh as f32 * 0.85 / img.height as f32)
        .min(1.0)
        .max(MIN_SCALE);
    let margin = margin_for_dpi(monitor_dpi_at(img.x, img.y));

    let data = Box::into_raw(Box::new(PinData {
        image: img.data.clone(), // Arc 克隆，不复制像素
        img_w: img.width,
        img_h: img.height,
        scale: fit,
        margin,
        rect: RECT::default(),
        rect_valid: false,
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

    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
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

        // 投影留白区（图像之外）不接收鼠标：交还给下层窗口，同时避免从阴影上误拖动
        WM_NCHITTEST => {
            let x = (lp.0 & 0xFFFF) as u16 as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            let m = data_of(hwnd).map(|d| d.margin).unwrap_or(0);
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() || m <= 0 {
                return LRESULT(HTCLIENT as isize);
            }
            let in_image = x >= r.left + m && x < r.right - m && y >= r.top + m && y < r.bottom - m;
            if in_image {
                LRESULT(HTCLIENT as isize)
            } else {
                LRESULT(HTTRANSPARENT as isize)
            }
        }

        // 按住左键拖动：伪造标题栏按下，交给系统移动
        WM_LBUTTONDOWN => {
            let _ = ReleaseCapture();
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), LPARAM(0));
            LRESULT(0)
        }

        // 双击左键：退出（关闭）该贴图。依赖窗口类的 CS_DBLCLKS 样式。
        WM_LBUTTONDBLCLK => {
            crate::diag::log("[pin] 双击左键 -> 关闭贴图");
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_MOUSEWHEEL => {
            zoom(hwnd, wp);
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

/// 滚轮缩放：以光标为锚点，保持光标下的图像位置不变。
unsafe fn zoom(hwnd: HWND, wp: WPARAM) {
    let Some(data) = data_of(hwnd) else { return };
    let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    if delta == 0 {
        return;
    }
    let factor = if delta > 0 { 1.1f32 } else { 1.0f32 / 1.1 };
    let new_scale = (data.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
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
    let nx = pt.x - cx * new_iw / old_iw - m;
    let ny = pt.y - cy * new_ih / old_ih - m;

    data.scale = new_scale;
    data.rect = RECT {
        left: nx,
        top: ny,
        right: nx + new_iw + m * 2,
        bottom: ny + new_ih + m * 2,
    };
    data.rect_valid = true;
    // 位置与尺寸由 nx/ny/尺寸 显式给出 → 不能用 SWP_NOMOVE
    let _ = SetWindowPos(
        hwnd,
        HWND_TOP,
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
        SourceConstantAlpha: 255,
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
    let inv_m = 1.0f32 / m as f32;
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
                let d = ((dx * dx + dy * dy) as f32).sqrt();
                let t = (d * inv_m).min(1.0);
                let alpha = (255.0 * SHADOW_PEAK * (1.0 - t) * (1.0 - t)).round() as u8;
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

unsafe fn context_menu(hwnd: HWND, lp: LPARAM) {
    let Some(data) = data_of(hwnd) else { return };
    let cfg = config::get();

    let items = [
        Item::new("复制图像"),
        Item::new(format!("保存为 {}…", cfg.format.label())),
        Item::new("识别文字 (OCR)"),
        Item::sep("重置缩放 (100%)"),
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

    let img: &[u8] = &data.image;
    match choice {
        0 => match clipboard::copy_image(img, data.img_w, data.img_h) {
            Ok(()) => crate::toast::show("已复制图像到剪贴板"),
            Err(e) => error_msgbox(&format!("复制失败：{e}")),
        },
        1 => match imageio::save_with_dialog(
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
        // OCR：与工具条「复制识别到的文字」完全一致 —— 后台识别 → 写入剪贴板 →
        // 主窗口弹 Toast，**不弹出结果窗口**（见 ocr::recognize_to_clipboard_async）。
        2 => ocr::recognize_to_clipboard_async(data.image.clone(), data.img_w, data.img_h),
        3 => {
            // 重置缩放：图像左上角保持不动（窗口位置不变，只改尺寸）
            data.scale = 1.0;
            let (iw, ih) = scaled_size(data, 1.0);
            let m = data.margin;
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_ok() {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    r.left,
                    r.top,
                    iw + m * 2,
                    ih + m * 2,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            render(hwnd);
        }
        4 => {
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
}
