//! OCR 结果窗口（Fluent 版）：无边框自绘标题栏 + Mica + 圆角卡片 + 悬停动画。
//! 后台线程识别完成后经 WM_APP 回填文本。
//!
//! E2E 依赖的窗口标题、EDIT 控件与按钮 ID 全部保持不变。
//!
//! 结果回填的安全设计（修复内存泄漏）：
//! 早期实现把 `Box<String>` 的裸指针塞进 `WPARAM` 跨线程传递。若用户在识别完成前
//! 关掉窗口，`PostMessage` 投递失败 → 该 `Box` 永久泄漏；极端情况下 HWND 被系统
//! 复用，指针还会被当成 payload 投进别的窗口。
//! 现改为「`Arc<Mutex<Option<_>>>` 结果槽 + `AtomicBool` 存活标志」：
//! 窗口销毁即置 `alive=false`，后台线程据此跳过投递，数据由引用计数自然释放。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::clipboard;
use crate::config;
use crate::error::{AppError, Result};
use crate::ocr;
use crate::theme;
use crate::util::{self, add_button, create_window, raw, register_class_w, to_wide, BtnStyle, HoverFx};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetStockObject,
    NULL_BRUSH, PAINTSTRUCT, PS_SOLID, Rectangle, RoundRect, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{SetWindowTheme, DRAWITEMSTRUCT};
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_OCR_DONE: u32 = WM_APP + 7;
const ID_COPY: usize = 100;
const ID_CLOSE: usize = 101;
const ID_MINIMIZE: usize = 102;

/// 逻辑客户区尺寸（96 DPI 基准，实际按窗口 DPI 换算）。
const RES_W: i32 = 560;
const RES_H: i32 = 420;
/// 自绘标题栏高度（逻辑像素）。
const HEADER_H: i32 = 44;
/// 边缘缩放命中宽度（逻辑像素）。
const EDGE: i32 = 8;
/// EM_SETRECT（windows 0.58 未提供，本地定义）。
const EM_SETRECT: u32 = 0x00B9;

type OcrResultSlot = Arc<Mutex<Option<(String, bool)>>>;

struct Data {
    edit: HWND,
    copy_btn: HWND,
    close_btn: HWND,
    /// 标题栏按钮
    close_hdr: HWND,
    min_hdr: HWND,
    /// Mica 是否启用
    backdrop: bool,
    /// 悬停动画
    fx: HoverFx,
    /// 识别结果槽（文本, 是否失败）
    slot: OcrResultSlot,
    /// 窗口是否仍存活（销毁后后台线程不再投递）
    alive: Arc<AtomicBool>,
}

/// 打开 OCR 结果窗口并在后台线程执行识别。
/// `image` 用 `Arc<[u8]>` 与截图结果共享，不复制像素。
pub fn start_ocr(image: Arc<[u8]>, w: i32, h: i32) -> Result<HWND> {
    register_class_w("PixShotOcrResult", ocr_proc, false, false, false);
    // 无边框：WS_THICKFRAME 供 DWM 阴影/缩放；标题栏自绘
    let style = WS_POPUP | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX;

    let slot: OcrResultSlot = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));

    let data = Box::into_raw(Box::new(Data {
        edit: HWND::default(),
        copy_btn: HWND::default(),
        close_btn: HWND::default(),
        close_hdr: HWND::default(),
        min_hdr: HWND::default(),
        backdrop: false,
        fx: HoverFx::new(),
        slot: Arc::clone(&slot),
        alive: Arc::clone(&alive),
    }));

    // 初始尺寸按系统 DPI 换算（PMv2 下 GetDpiForSystem 返回主屏 DPI）
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32 };
    let (ww, wh) = (util::scaled(RES_W, dpi), util::scaled(RES_H, dpi));
    // CW_USEDEFAULT 对 WS_POPUP 无效：改为主屏居中
    let x = unsafe { (GetSystemMetrics(SM_CXSCREEN) - ww) / 2 };
    let y = unsafe { (GetSystemMetrics(SM_CYSCREEN) - wh) / 2 };

    let hwnd = create_window(
        "PixShotOcrResult",
        "OCR 识别中…",
        WINDOW_EX_STYLE(0),
        style,
        x,
        y,
        ww,
        wh,
        data as *mut core::ffi::c_void,
    )?;
    if hwnd.is_invalid() {
        unsafe { drop(Box::from_raw(data)) };
        return Err(AppError::Config("创建 OCR 结果窗口失败".into()));
    }

    // 后台 OCR 线程：结果写入共享槽，若窗口仍存活则通知 GUI 线程刷新
    // （HWND 非 Send，跨线程以 isize 传递地址）
    let hwnd_addr = hwnd.0 as isize;
    std::thread::spawn(move || {
        let r = ocr::recognize(&image, w, h);
        let payload = match r {
            Ok(text) => (text, false),
            Err(e) => (format!("识别失败：{e}"), true),
        };
        if let Ok(mut g) = slot.lock() {
            *g = Some(payload);
        }
        // 窗口已关闭（或正在关闭）：不再投递，数据随 Arc 释放
        if alive.load(Ordering::SeqCst) {
            unsafe {
                let hwnd = HWND(hwnd_addr as *mut core::ffi::c_void);
                let _ = PostMessageW(hwnd, WM_OCR_DONE, WPARAM(0), LPARAM(0));
            }
        }
    });

    Ok(hwnd)
}

unsafe fn data_of(hwnd: HWND) -> Option<&'static mut Data> {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    if p == 0 {
        None
    } else {
        Some(&mut *(p as *mut Data))
    }
}

/// 深浅色切换时同步 EDIT 滚动条主题（浅色还原默认）。
unsafe fn theme_edit_scrollbar(edit: HWND, dark: bool) {
    if dark {
        let s = to_wide("DarkMode_Explorer");
        let _ = SetWindowTheme(edit, PCWSTR::from_raw(s.as_ptr()), PCWSTR::null());
    } else {
        let _ = SetWindowTheme(edit, PCWSTR::null(), PCWSTR::null());
    }
}

/// EDIT 文本内边距（EM_SETRECT）。
unsafe fn margin_edit(edit: HWND) {
    let mut cr = RECT::default();
    let _ = GetClientRect(edit, &mut cr);
    let dpi = util::dpi_of(edit);
    let mut m = RECT {
        left: util::scaled(6, dpi),
        top: util::scaled(4, dpi),
        right: cr.right - util::scaled(6, dpi),
        bottom: cr.bottom - util::scaled(4, dpi),
    };
    let _ = SendMessageW(
        edit,
        EM_SETRECT,
        WPARAM(0),
        LPARAM(&mut m as *mut RECT as isize),
    );
}

/// 自绘：基底 + 标题（含 OCR 进行中/结果的动态标题）+ 圆角卡片。
unsafe fn paint_ocr(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else {
        return;
    };
    let t = theme::current();
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);
    let header_h = util::scaled(HEADER_H, dpi);

    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    // 1. 基底（Mica 显影黑 / 实色回退）
    let base = if d.backdrop {
        windows::Win32::Foundation::COLORREF(0)
    } else {
        t.bg
    };
    let base_brush = CreateSolidBrush(base);
    let rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(hdc, &rc, base_brush);
    let _ = DeleteObject(base_brush);

    // 1.5 自绘 1px 窗口轮廓（半透明描边观感；不用 DWMWA_BORDER_COLOR 以避免 DWM 框架带）
    let pen = CreatePen(PS_SOLID, 1, theme::edge_line(t));
    let op = SelectObject(hdc, pen);
    let _ = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let _ = Rectangle(hdc, 0, 0, cw, ch);
    SelectObject(hdc, op);
    let _ = DeleteObject(pen);

    // 2. 标题（窗口标题文本，随识别状态更新）
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    if n > 0 {
        let font = util::gui_font(hwnd, 10, 600);
        let old_font = SelectObject(hdc, font);
        raw::SetBkMode(hdc.0 as isize, 1);
        raw::SetTextColor(hdc.0 as isize, t.text);
        let mut tr = RECT {
            left: util::scaled(18, dpi),
            top: 0,
            right: cw - util::scaled(110, dpi),
            bottom: header_h,
        };
        let tw: Vec<u16> = buf[..(n as usize).min(255)].to_vec();
        let _ = raw::DrawTextW(
            hdc.0 as isize,
            PCWSTR::from_raw(tw.as_ptr()),
            -1,
            &mut tr,
            0x20 | 0x4 | 0x8000, // SINGLELINE | VCENTER | END_ELLIPSIS
        );
        SelectObject(hdc, old_font);
    }

    // 3. 圆角内容卡片（EDIT/按钮作为子控件绘制其上）
    let brush = CreateSolidBrush(t.card);
    let pen = CreatePen(PS_SOLID, 1, t.stroke);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(16, dpi);
    let _ = RoundRect(hdc, util::scaled(12, dpi), header_h, cw - util::scaled(12, dpi), ch - util::scaled(12, dpi), r, r);
    SelectObject(hdc, ob);
    SelectObject(hdc, op);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);

    let _ = EndPaint(hwnd, &ps);
}

/// 依据客户区大小与 DPI 重排全部控件（含标题栏按钮）。
unsafe fn layout(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else {
        return;
    };
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right - cr.left, cr.bottom - cr.top);
    let dpi = util::dpi_of(hwnd);
    let s = |v: i32| util::scaled(v, dpi);
    if cw < s(160) || ch < s(160) {
        return;
    }

    let header_h = s(HEADER_H);
    let btn_h = s(38);
    let btn_w = s(120);
    let pad = s(24);
    let footer = s(58);

    // EDIT：卡片内（标题栏下方，底部为按钮行留出空间）
    let _ = MoveWindow(
        d.edit,
        pad,
        header_h + s(14),
        cw - pad * 2,
        (ch - footer - header_h - s(14)).max(s(40)),
        BOOL(1),
    );
    margin_edit(d.edit);

    // 底部按钮行
    let _ = MoveWindow(d.copy_btn, pad, ch - footer, btn_w, btn_h, BOOL(1));
    let _ = MoveWindow(d.close_btn, cw - pad - btn_w, ch - footer, btn_w, btn_h, BOOL(1));

    // 标题栏按钮（右对齐）
    let glyph_w = s(46);
    let glyph_h = s(32);
    let _ = MoveWindow(d.close_hdr, cw - s(52), s(6), glyph_w, glyph_h, BOOL(1));
    let _ = MoveWindow(d.min_hdr, cw - s(98), s(6), glyph_w, glyph_h, BOOL(1));
}

unsafe extern "system" fn ocr_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => LRESULT(0),

        WM_NCHITTEST => {
            // 边缘原生缩放；标题栏可拖动；其余客户区（子控件自行命中）
            let mut r = RECT::default();
            if GetWindowRect(hwnd, &mut r).is_err() {
                return LRESULT(HTCLIENT as isize);
            }
            let dpi = util::dpi_of(hwnd);
            let edge = util::scaled(EDGE, dpi);
            let header_h = util::scaled(HEADER_H, dpi);
            let x = ((lp.0 & 0xFFFF) as u16 as i16 as i32) - r.left;
            let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 - r.top;
            let w = r.right - r.left;
            let h = r.bottom - r.top;
            let code = if w <= edge * 2 || h <= edge * 2 {
                if y < header_h {
                    HTCAPTION
                } else {
                    HTCLIENT
                }
            } else if x < edge && y < edge {
                HTTOPLEFT
            } else if x >= w - edge && y < edge {
                HTTOPRIGHT
            } else if x < edge && y >= h - edge {
                HTBOTTOMLEFT
            } else if x >= w - edge && y >= h - edge {
                HTBOTTOMRIGHT
            } else if x < edge {
                HTLEFT
            } else if x >= w - edge {
                HTRIGHT
            } else if y < edge {
                HTTOP
            } else if y >= h - edge {
                HTBOTTOM
            } else if y < header_h {
                HTCAPTION
            } else {
                HTCLIENT
            };
            LRESULT(code as isize)
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
            let t = theme::current();
            d.backdrop = theme::apply_frame(hwnd, t, true);

            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let dpi = util::dpi_of(hwnd);
            // 只读多行 EDIT（无边框，卡片上文本区域内边距由 EM_SETRECT 控制）
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR::from_raw(to_wide("EDIT").as_ptr()),
                PCWSTR::from_raw(to_wide("正在识别文字…").as_ptr()),
                WINDOW_STYLE(
                    WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0 | WS_HSCROLL.0 | WS_TABSTOP.0
                        | 0x0004  // ES_MULTILINE
                        | 0x0800, // ES_READONLY
                ),
                util::scaled(24, dpi),
                util::scaled(58, dpi),
                util::scaled(RES_W - 48, dpi),
                util::scaled(RES_H - 116, dpi),
                hwnd,
                None,
                hinst,
                None,
            )
            .unwrap_or_default();
            let font = util::gui_font(hwnd, 9, 400);
            let _ = SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
            theme_edit_scrollbar(edit, t.dark);

            let copy_btn = add_button(hwnd, "复制文本", 0, 0, 10, 10, ID_COPY);
            let close_btn = add_button(hwnd, "关闭", 0, 0, 10, 10, ID_CLOSE);
            let close_hdr = add_button(hwnd, "✕", 0, 0, 10, 10, ID_CLOSE);
            let min_hdr = add_button(hwnd, "–", 0, 0, 10, 10, ID_MINIMIZE);

            d.edit = edit;
            d.copy_btn = copy_btn;
            d.close_btn = close_btn;
            d.close_hdr = close_hdr;
            d.min_hdr = min_hdr;
            for b in [copy_btn, close_btn, close_hdr, min_hdr] {
                d.fx.register(b);
            }
            layout(hwnd);

            let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
            let _ = SetForegroundWindow(hwnd);
            LRESULT(0)
        }

        WM_ERASEBKGND => LRESULT(1),

        WM_PAINT => {
            paint_ocr(hwnd);
            LRESULT(0)
        }

        // 只读 EDIT / STATIC：主题文本色 + 透明底（露出卡片）
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
            util::ctlcolor_static(wp, theme::current().text)
        }

        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                if let Some(d) = data_of(hwnd) {
                    let style = if (*dis).hwndItem == d.close_hdr {
                        BtnStyle::CloseGlyph
                    } else if (*dis).hwndItem == d.copy_btn {
                        BtnStyle::Accent
                    } else {
                        BtnStyle::Subtle
                    };
                    util::draw_od_button(hwnd, &*dis, &d.fx, style, theme::current());
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
            // 深/浅色实时跟随：重载主题 + 滚动条主题 + 重绘
            theme::reload();
            if let Some(d) = data_of(hwnd) {
                theme_edit_scrollbar(d.edit, theme::current().dark);
            }
            let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 1);
            LRESULT(0)
        }

        WM_SIZE => {
            layout(hwnd);
            LRESULT(0)
        }

        WM_DPICHANGED => {
            // 移动到系统建议的矩形，并按新 DPI 重排控件
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

        WM_OCR_DONE => {
            if let Some(d) = data_of(hwnd) {
                let payload = d.slot.lock().ok().and_then(|g| g.clone());
                if let Some((text, is_err)) = payload {
                    let _ = SetWindowTextW(d.edit, PCWSTR::from_raw(to_wide(&text).as_ptr()));
                    let title = if is_err {
                        "OCR 识别失败".to_string()
                    } else {
                        format!("OCR 识别结果（{} 字符）", text.chars().count())
                    };
                    let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(to_wide(&title).as_ptr()));
                    let _ = raw::InvalidateRect(hwnd, std::ptr::null(), 0);
                }
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
            if id == ID_COPY {
                copy_from_edit(hwnd);
            } else if id == ID_CLOSE {
                let _ = DestroyWindow(hwnd);
            } else if id == ID_MINIMIZE {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                let mut data = Box::from_raw(p as *mut Data);
                // 先停悬停定时器，再置存活标志，最后释放
                data.fx.stop(hwnd);
                data.alive.store(false, Ordering::SeqCst);
            }
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 复制 EDIT 控件全部文本到剪贴板。
unsafe fn copy_from_edit(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else {
        return;
    };
    let len = GetWindowTextLengthW(d.edit).max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    GetWindowTextW(d.edit, &mut buf);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..end]);
    match clipboard::copy_text(&text) {
        // 轻提示替代模态框：不再阻塞消息循环（原先 Modal 还会让 E2E 需专门关框）
        Ok(()) => {
            let _ = config::get(); // 确保配置已初始化（toast 依赖 toast_ms）
            crate::toast::show("已复制到剪贴板");
        }
        Err(e) => util::error_msgbox(&format!("复制失败：{e}")),
    }
}
