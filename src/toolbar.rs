//! 截图操作工具栏（Fluent：无边框自绘标题栏 / 圆角卡片 / 悬停动画）。
//!
//! 从 `main.rs` 拆出（原本与托盘/热键/流程编排挤在同一文件，接近千行）。
//! 皮肤固定深色（见 `theme::toolbar_skin`），布局按窗口 DPI 换算并在 `WM_DPICHANGED` 重排。
//!
//! 子控件 ID 与窗口文本保持原样（E2E 依赖「贴图 / 复制 / 保存 / OCR 识别 / 关闭」按钮文本）。

use std::sync::Arc;

use crate::capture::{self, CapturedImage};
use crate::config;
use crate::error::{AppError, Result};
use crate::theme;
use crate::util::{self, add_button, raw, register_class_w};
use crate::{clipboard, imageio, ocr, pin, tray};

use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetStockObject,
    RoundRect, SelectObject, NULL_BRUSH, PS_SOLID, Rectangle,
};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;

// 工具栏按钮 ID（与 E2E 的控件查找协议一致）
const BTN_PIN: usize = 1;
const BTN_COPY: usize = 2;
const BTN_SAVE: usize = 3;
const BTN_OCR: usize = 4;
/// VK_ESCAPE（工具条上按 Esc 直接关闭，不执行操作）
const VK_ESCAPE: usize = 0x1B;

/// 逻辑客户区尺寸（96 DPI 基准）。工具条只承载一行操作按钮，故很矮。
const TB_W: i32 = 480;
/// 高度 = 上内边距 + 按钮高 + 下内边距（上下一致，见 `layout`）。
const TB_H: i32 = TB_PAD_V * 2 + TB_BTN_H;
/// 按钮到底/到顶的内边距（逻辑像素）。
const TB_PAD_V: i32 = 16;
/// 按钮高（逻辑像素）。
const TB_BTN_H: i32 = 38;
/// 按钮到窗口左右边缘的内边距（逻辑像素）。
const TB_PAD_H: i32 = 16;
/// 圆角卡片相对窗口边缘的内缩（逻辑像素）。
const TB_CARD_INSET: i32 = 8;

struct ToolbarData {
    image: Arc<CapturedImage>,
    /// 四个操作按钮：贴图 / 复制 / 保存 / OCR 识别
    buttons: [HWND; 4],
    /// 工具栏皮肤：固定深色
    skin: theme::Theme,
    /// Mica 背景是否启用（工具栏固定深色为纯色面，恒为 false）
    backdrop: bool,
    /// 按钮悬停动画
    fx: util::HoverFx,
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut ToolbarData> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut ToolbarData))
    }
}

/// 进入工具栏（模态，直到用户选择操作或关闭）。须在 GUI 线程调用。
pub fn run(image: Arc<CapturedImage>) -> Result<()> {
    unsafe { run_impl(image) }
}

unsafe fn run_impl(image: Arc<CapturedImage>) -> Result<()> {
    register_class_w("PixShotToolbar", toolbar_proc, false, false, false);

    let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32;
    let (tw, th) = (util::scaled(TB_W, dpi), util::scaled(TB_H, dpi));

    // 定位在选区下方 8px，越界则钳制回虚拟屏幕内
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    let x = image.x.clamp(vx, (vx + vw - tw).max(vx));
    let y = (image.y + image.height + util::scaled(8, dpi)).clamp(vy, (vy + vh - th).max(vy));

    let data = Box::into_raw(Box::new(ToolbarData {
        image,
        buttons: [HWND::default(); 4],
        skin: theme::toolbar_skin(),
        backdrop: false,
        fx: util::HoverFx::new(),
    }));

    // WS_THICKFRAME 供 DWM 绘制阴影；WM_NCCALCSIZE(0) 去除边框（全客户区）
    let hwnd = util::create_window(
        "PixShotToolbar",
        "PixShot 截图",
        WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0),
        WS_POPUP | WS_THICKFRAME,
        x,
        y,
        tw,
        th,
        data as *mut core::ffi::c_void,
    )?;
    if hwnd.is_invalid() {
        drop(Box::from_raw(data));
        return Err(AppError::Config("创建工具栏窗口失败".into()));
    }
    // 排除出截屏捕获：防止工具栏出现在后续截图/录屏画面中（截图工具惯例）
    let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);

    // 模态消息循环
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = DestroyWindow(hwnd);
    Ok(())
}

/// 按 DPI 与客户区尺寸重排控件（4 个按钮均分可用宽度，高 DPI 下不溢出）。
unsafe fn layout(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);
    let s = |v: i32| util::scaled(v, dpi);
    if cw < s(240) || ch < s(48) {
        return;
    }

    let pad_h = s(TB_PAD_H);
    let btn_h = s(TB_BTN_H);
    let gap = s(8);
    let n = d.buttons.len() as i32;
    let total = cw - pad_h * 2;
    let btn_w = ((total - gap * (n - 1)) / n).max(s(56));

    // 垂直居中：按钮上缘到窗口顶部、下缘到底部的距离相等（均为 pad）。
    // 直接由客户区高算，避免"常量求和"在某个 DPI 下因四舍五入而不对称。
    let by = ((ch - btn_h) / 2).max(0);
    for (i, b) in d.buttons.iter().enumerate() {
        let bx = pad_h + i as i32 * (btn_w + gap);
        let _ = MoveWindow(*b, bx, by, btn_w, btn_h, BOOL(1));
    }
}

/// 工具栏自绘：深色基底 + 圆角内容卡片。
/// 除按钮自身文本外不绘制任何文字（标题/状态已移除）。
unsafe fn paint(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let t = d.skin;
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);
    let inset = util::scaled(TB_CARD_INSET, dpi);

    // 1. 基底
    let base = if d.backdrop { COLORREF(0) } else { t.bg };
    let base_brush = CreateSolidBrush(base);
    let rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(hdc, &rc, base_brush);
    let _ = DeleteObject(base_brush);

    // 1.5 自绘 1px 窗口轮廓（不用 DWMWA_BORDER_COLOR，避免 DWM 框架带）
    let pen = CreatePen(PS_SOLID, 1, theme::edge_line(t));
    let op = SelectObject(hdc, pen);
    let _ = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let _ = Rectangle(hdc, 0, 0, cw, ch);
    SelectObject(hdc, op);
    let _ = DeleteObject(pen);

    // 2. 圆角内容卡片（操作按钮绘制其上）
    let brush = CreateSolidBrush(t.card);
    let pen = CreatePen(PS_SOLID, 1, t.stroke);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(16, dpi);
    let _ = RoundRect(hdc, inset, inset, cw - inset, ch - inset, r, r);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn toolbar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => LRESULT(0), // 去边框：客户区 == 外框

        WM_NCHITTEST => {
            // 无边框：按钮行以外的区域（含上下内边距、按钮之间的间隙两侧）整块可拖动；
            // 按钮行内返回 HTCLIENT，让子控件（按钮）正常接收鼠标。
            let mut r = RECT::default();
            let mut cr = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() || GetClientRect(hwnd, &mut cr).is_err() {
                return LRESULT(HTCLIENT as isize);
            }
            let dpi = util::dpi_of(hwnd);
            let pad_h = util::scaled(TB_PAD_H, dpi);
            let btn_h = util::scaled(TB_BTN_H, dpi);
            let x = (((lp.0 & 0xFFFF) as u16 as i16 as i32) - r.left).max(0);
            let y = (((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 - r.top).max(0);
            let by = ((cr.bottom - btn_h) / 2).max(0);
            let in_button_band = y >= by && y < by + btn_h && x >= pad_h && x < cr.right - pad_h;
            LRESULT(if in_button_band {
                HTCLIENT as isize
            } else {
                HTCAPTION as isize
            })
        }

        WM_CREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            let Some(d) = data_of(hwnd) else {
                return LRESULT(-1);
            };
            d.skin = theme::toolbar_skin();
            d.backdrop = theme::apply_frame(hwnd, d.skin, false);

            // 工具条只放操作按钮：不创建任何标签文字，也不再有标题栏关闭按钮。
            // 先以占位尺寸创建，再由 layout 统一定位（DPI 变化时同一函数复用）。
            d.buttons[0] = add_button(hwnd, "贴图", 0, 0, 10, 10, BTN_PIN);
            d.buttons[1] = add_button(hwnd, "复制", 0, 0, 10, 10, BTN_COPY);
            d.buttons[2] = add_button(hwnd, "保存", 0, 0, 10, 10, BTN_SAVE);
            d.buttons[3] = add_button(hwnd, "OCR 识别", 0, 0, 10, 10, BTN_OCR);
            for b in d.buttons {
                d.fx.register(b);
            }
            layout(hwnd);
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }

        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                if let Some(d) = data_of(hwnd) {
                    let style = if (wp.0 & 0xFFFF) as usize == BTN_PIN {
                        util::BtnStyle::Accent
                    } else {
                        util::BtnStyle::Subtle
                    };
                    util::draw_od_button(hwnd, &*dis, &d.fx, style, d.skin);
                }
            }
            LRESULT(1)
        }

        WM_MOUSEMOVE => {
            if let Some(d) = data_of(hwnd) {
                d.fx.on_mouse_move(hwnd);
            }
            LRESULT(0)
        }

        util::WM_MOUSELEAVE => {
            if let Some(d) = data_of(hwnd) {
                d.fx.on_mouse_leave(hwnd);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            let mut handled = false;
            if let Some(d) = data_of(hwnd) {
                handled = d.fx.on_timer(hwnd, wp.0);
            }
            if handled {
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }

        WM_SETTINGCHANGE => {
            // 工具栏皮肤固定深色，此处只需重绘（系统主题变化不影响配色）
            let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 1);
            LRESULT(0)
        }

        WM_DPICHANGED => {
            let sug = lp.0 as *const RECT;
            if !sug.is_null() {
                let r = *sug;
                let _ = SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            layout(hwnd);
            let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 1);
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
            handle_command(hwnd, id);
            LRESULT(0)
        }
        // Esc = 不执行任何操作直接关闭。工具条已无任何关闭按钮，留一条键盘退路，
        // 避免误触截图后只能"随便选一个操作"。
        WM_KEYDOWN => {
            if wp.0 == VK_ESCAPE {
                close_bar(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                let mut data = Box::from_raw(p as *mut ToolbarData);
                data.fx.stop(hwnd);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 关闭工具条：隐藏后结束其模态消息循环，回到主循环。
/// 贴图 / 复制 / 保存 / OCR 四个操作都走这里 —— 工具条是「一次性操作条」。
unsafe fn close_bar(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_HIDE);
    PostQuitMessage(0);
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    match id {
        BTN_PIN => {
            let Some(d) = data_of(hwnd) else { return };
            match pin::create(&d.image) {
                Ok(_) => close_bar(hwnd),
                Err(e) => util::error_msgbox(&format!("贴图失败：{e}")),
            }
        }
        BTN_COPY => {
            let Some(d) = data_of(hwnd) else { return };
            // Arc 共享像素，copy_image 直接读切片，不再整幅复制
            match clipboard::copy_image(&d.image.data, d.image.width, d.image.height) {
                Ok(()) => {
                    crate::toast::show("已复制图像到剪贴板");
                    close_bar(hwnd);
                }
                Err(e) => util::error_msgbox(&format!("复制失败：{e}")),
            }
        }
        BTN_SAVE => {
            let Some(d) = data_of(hwnd) else { return };
            let cfg = config::get();
            match imageio::save_with_dialog(
                hwnd,
                &d.image.data,
                d.image.width,
                d.image.height,
                cfg.format,
                cfg.save_dir.as_deref(),
            ) {
                Ok(Some(path)) => {
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or(path);
                    crate::toast::show(&format!("已保存：{name}"));
                    close_bar(hwnd);
                }
                Ok(None) => close_bar(hwnd), // 取消保存同样关闭工具条
                Err(e) => util::error_msgbox(&format!("保存失败：{e}")),
            }
        }
        BTN_OCR => {
            let Some(d) = data_of(hwnd) else { return };
            // 不再弹出结果窗口：后台识别 → 直接写剪贴板 → 由主窗口弹 Toast。
            // 识别必须离开 GUI 线程（WinRT 异步调用不可阻塞消息循环），因此
            // 用 WM_APP_OCR_DONE 把结果码投递回主窗口展示提示。
            let image = Arc::clone(&d.image.data);
            let (w, h) = (d.image.width, d.image.height);
            // HWND 非 Send：跨线程以地址（isize）传递，线程内还原
            let main_addr = tray::main_hwnd().0 as isize;
            std::thread::spawn(move || {
                let code = match ocr::recognize(&image, w, h) {
                    Ok(text) if !text.trim().is_empty() => {
                        if clipboard::copy_text(&text).is_ok() {
                            tray::OCR_DONE_COPIED
                        } else {
                            tray::OCR_DONE_FAIL
                        }
                    }
                    Ok(_) => tray::OCR_DONE_EMPTY,
                    Err(e) => {
                        crate::diag::log(&format!("[ocr] 工具条识别失败: {e}"));
                        tray::OCR_DONE_FAIL
                    }
                };
                if main_addr != 0 {
                    unsafe {
                        let main = HWND(main_addr as *mut core::ffi::c_void);
                        let _ = PostMessageW(main, tray::WM_APP_OCR_DONE, WPARAM(code), LPARAM(0));
                    }
                }
            });
            close_bar(hwnd);
        }
        _ => {}
    }
}
