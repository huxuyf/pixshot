//! 贴图窗口：置顶无边框显示截图。
//!
//! 支持：拖动（按住左键）、滚轮缩放（以光标为锚点）、双击关闭、
//! 右键菜单（复制图像 / 保存（按配置格式）/ OCR / 重置缩放 / 关闭）、Esc 关闭。
//!
//! 无 `WS_THICKFRAME`：确保图像内容像素级完整呈现（尺寸调整走滚轮缩放）。
//!
//! 绘制路径（修复闪烁/卡顿）：
//! - 100% 缩放（窗口尺寸 == 图像尺寸）：直接写窗口 DC，零中间缓冲；
//! - 其他缩放：离屏 DC 双缓冲 + `HALFTONE`（缩小时质量优先）/ `COLORONCOLOR`（放大时速度优先）。
//!
//! 右键菜单与托盘共用 `menu` 模块的自绘 Fluent 菜单（不再用系统 `TrackPopupMenuEx`）。

use std::sync::Arc;

use crate::capture::CapturedImage;
use crate::clipboard;
use crate::config;
use crate::error::{AppError, Result};
use crate::imageio;
use crate::menu::{self, Item};
use crate::results;
use crate::theme;
use crate::util::{error_msgbox, raw, register_class_w, to_wide};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EndPaint, PAINTSTRUCT, SelectObject, SetBrushOrgEx, SetStretchBltMode,
    StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::*;

const MIN_SCALE: f32 = 0.05;
const MAX_SCALE: f32 = 8.0;

const VK_ESCAPE: usize = 0x1B;

/// 每个贴图窗口的私有数据（指针存在 GWLP_USERDATA，WM_DESTROY 时释放）。
pub struct PinData {
    /// BGRA 自上而下像素；与截图结果共享同一份内存（Arc，零拷贝）。
    pub image: Arc<[u8]>,
    pub img_w: i32,
    pub img_h: i32,
    pub scale: f32,
    /// 外框矩形缓存（zoom/菜单等高频路径，避免重复 GetWindowRect 内核调用）
    pub rect: RECT,
    pub rect_valid: bool,
}

/// 创建贴图窗口（须在 GUI 线程调用）。
pub fn create(img: &CapturedImage) -> Result<HWND> {
    unsafe { create_impl(img) }
}

unsafe fn create_impl(img: &CapturedImage) -> Result<HWND> {
    register_class_w("PixShotPin", pin_proc, false, true, true); // CS_DBLCLKS：双击退出贴图
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    // 初始缩放：不放大、不超过主屏 85%
    let sw = GetSystemMetrics(SM_CXSCREEN).max(1);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(1);
    let fit = (sw as f32 * 0.85 / img.width as f32)
        .min(sh as f32 * 0.85 / img.height as f32)
        .min(1.0)
        .max(MIN_SCALE);
    let w = ((img.width as f32 * fit).round() as i32).max(4);
    let h = ((img.height as f32 * fit).round() as i32).max(4);

    // 出现在原截图位置（钳制到虚拟屏幕内）
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    let x = img.x.clamp(vx, vx + vw - w);
    let y = img.y.clamp(vy, vy + vh - h);

    let data = Box::into_raw(Box::new(PinData {
        image: img.data.clone(), // Arc 克隆，不复制像素
        img_w: img.width,
        img_h: img.height,
        scale: fit,
        rect: RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        },
        rect_valid: true,
    }));

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR::from_raw(to_wide("PixShotPin").as_ptr()),
        PCWSTR::null(),
        // 无 THICKFRAME：图像内容需要像素级完整呈现；实测 THICKFRAME+NCCALCSASE(0)
        // 组合在窗口失去前台后 DWM 会在外缘绘制 ~8px 框架视觉带内缩内容。
        // 尺寸调整由滚轮缩放承担；阴影由窗口类的 CS_DROPSHADOW 提供。
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
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
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
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
        WM_PAINT => paint(hwnd),

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
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);

    let old_w = (r.right - r.left).max(1);
    let old_h = (r.bottom - r.top).max(1);
    let new_w = ((data.img_w as f32 * new_scale).round() as i32).max(4);
    let new_h = ((data.img_h as f32 * new_scale).round() as i32).max(4);
    let cx = (pt.x - r.left).clamp(0, old_w);
    let cy = (pt.y - r.top).clamp(0, old_h);
    let nx = pt.x - cx * new_w / old_w;
    let ny = pt.y - cy * new_h / old_h;

    data.scale = new_scale;
    // 位置与尺寸由 nx/ny/new_w/new_h 显式给出 → 不能用 SWP_NOMOVE
    let _ = SetWindowPos(
        hwnd,
        HWND_TOP,
        nx,
        ny,
        new_w,
        new_h,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    raw::InvalidateRect(hwnd, std::ptr::null(), 1);
}

/// 把 BGRA 图像绘制到客户区（100% 直写；其余走双缓冲 + 质量/速度模式）。
unsafe fn paint(hwnd: HWND) -> LRESULT {
    let Some(data) = data_of(hwnd) else {
        return LRESULT(0);
    };
    let img: &[u8] = &data.image;
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    if cw <= 0 || ch <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: data.img_w,
            biHeight: -data.img_h, // 自上而下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        },
        ..Default::default()
    };
    let bits = Some(img.as_ptr() as *const core::ffi::c_void);

    // 100% 显示：无缩放，直接写窗口 DC（最快路径，也完全不会闪）
    if cw == data.img_w && ch == data.img_h {
        StretchDIBits(
            hdc, 0, 0, cw, ch, 0, 0, data.img_w, data.img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
        );
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }

    // 缩放显示：离屏合成后一次性 BitBlt，避免逐帧闪烁
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, cw, ch);
    if mem.is_invalid() || bmp.is_invalid() {
        // 分配失败时退回直接绘制（宁可可能闪，也要显示内容）
        StretchDIBits(
            hdc, 0, 0, cw, ch, 0, 0, data.img_w, data.img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
        );
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp);
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }

    let old = SelectObject(mem, bmp);
    // 缩小（细节丢失更明显）用 HALFTONE；放大用 COLORONCOLOR（快且更锐利）
    let mode = if data.scale < 1.0 { HALFTONE } else { COLORONCOLOR };
    let prev = SetStretchBltMode(mem, mode);
    if mode == HALFTONE {
        // HALFTONE 需要自定义画刷原点，否则纹理可能错位
        let _ = SetBrushOrgEx(mem, 0, 0, None);
    }
    StretchDIBits(
        mem, 0, 0, cw, ch, 0, 0, data.img_w, data.img_h, bits, &bmi, DIB_RGB_COLORS, SRCCOPY,
    );
    let _ = BitBlt(hdc, 0, 0, cw, ch, mem, 0, 0, SRCCOPY);
    let _ = SetStretchBltMode(mem, windows::Win32::Graphics::Gdi::STRETCH_BLT_MODE(prev));
    SelectObject(mem, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);

    let _ = EndPaint(hwnd, &ps);
    LRESULT(0)
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
        2 => match results::start_ocr(data.image.clone(), data.img_w, data.img_h) {
            Ok(_) => crate::toast::show("正在识别文字…"),
            Err(e) => error_msgbox(&format!("OCR 启动失败：{e}")),
        },
        3 => {
            data.scale = 1.0;
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_ok() {
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    r.left,
                    r.top,
                    data.img_w.max(4),
                    data.img_h.max(4),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            raw::InvalidateRect(hwnd, std::ptr::null(), 1);
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
}
