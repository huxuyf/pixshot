//! 截图操作工具栏（Fluent：无边框自绘标题栏 / 圆角卡片 / 悬停动画）。
//!
//! 从 `main.rs` 拆出（原本与托盘/热键/流程编排挤在同一文件，接近千行）。
//! 皮肤固定深色（见 `theme::toolbar_skin`），布局按窗口 DPI 换算并在 `WM_DPICHANGED` 重排。
//!
//! 按钮为**纯图标**（矢量自绘，见 [`crate::icons`]），依次为
//! 贴图 / 保存图像 / 复制并关闭 / 复制识别到的文字 / 矩形 / 箭头 / 步骤序号 /
//! 椭圆 / 线段 / 画笔 / 马赛克 / 高亮 / 文字 / 关闭；
//! 悬停延时后在图标上方弹出提示气泡（[`crate::tooltip`]）。
//!
//! 控件 ID 保持稳定（E2E 用 `Find-ChildById` 定位，见 `scripts/gui_e2e_*.ps1`）：
//! 1=贴图 / 2=复制 / 3=保存 / 4=OCR / 5=关闭；标注工具 6=矩形 / 7=箭头 / 8=步骤序号 /
//! 9=马赛克 / 10=高亮 / 11=文字；新增图形 12=椭圆 / 13=线段 / 14=画笔。
//!
//! 标注（见设计文档 1.2 D/E / 5.5）：工具按钮把当前工具经 `WM_APP_SET_TOOL`
//! 投递给冻结层，并按冻结层回传的 `WM_APP_TOOL_CHANGED` 高亮；文字输入经
//! `WM_APP_CHAR` 转发 `WM_CHAR`（输入中字母键放行为正文，不当工具快捷键，
//! 见 `WM_APP_QUERY_TYPING`）；贴图/复制/保存/OCR 动作在导出前先经
//! [`crate::annot::composite`] 合成标注，且先 `commit_typing` 完成输入中的文字。

use std::sync::Arc;

use crate::annot;
use crate::capture::{self, CapturedImage};
use crate::config;
use crate::error::{AppError, Result};
use crate::overlay::{
    Selection, WM_APP_CHAR, WM_APP_ESC_CLOSE, WM_APP_KEY, WM_APP_QUERY_TYPING, WM_APP_SET_TOOL,
    WM_APP_TOOL_CHANGED,
};
use crate::theme;
use crate::util::{self, add_button, raw, register_class_w, BtnStyle};
use crate::{clipboard, icons, imageio, ocr, pin, tooltip};

use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, EndPaint, FillRect, RoundRect, SelectObject, PS_SOLID, SRCCOPY,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// 工具栏按钮 ID（与 E2E 的控件查找协议一致）
const BTN_PIN: usize = 1;
const BTN_COPY: usize = 2;
const BTN_SAVE: usize = 3;
const BTN_OCR: usize = 4;
const BTN_CLOSE: usize = 5;
/// 标注工具按钮 ID（见 5.5）。
const BTN_RECT: usize = 6;
const BTN_ARROW: usize = 7;
const BTN_STEP: usize = 8;
/// 标注工具按钮 ID 扩展：马赛克 / 高亮 / 文字。
const BTN_MOSAIC: usize = 9;
const BTN_HIGHLIGHT: usize = 10;
const BTN_TEXT: usize = 11;
/// 新增图形工具按钮 ID：椭圆 / 线段 / 画笔（追加在末尾，保持既有 ID 稳定）。
const BTN_ELLIPSE: usize = 12;
const BTN_LINE: usize = 13;
const BTN_PEN: usize = 14;
/// 按钮在窗口内的从左到右排列顺序（图标顺序 = 需求给定顺序）
const BTN_ORDER: [usize; 14] = [
    BTN_PIN,
    BTN_SAVE,
    BTN_COPY,
    BTN_OCR,
    BTN_RECT,
    BTN_ARROW,
    BTN_STEP,
    BTN_ELLIPSE,
    BTN_LINE,
    BTN_PEN,
    BTN_MOSAIC,
    BTN_HIGHLIGHT,
    BTN_TEXT,
    BTN_CLOSE,
];
/// 各按钮的悬停提示文字（与 [`BTN_ORDER`] 一一对应）
const TIP_TEXTS: [&str; 14] = [
    "贴图",
    "保存图像",
    "复制并关闭",
    "复制识别到的文字",
    "矩形",
    "箭头",
    "步骤序号",
    "椭圆",
    "线段",
    "画笔",
    "马赛克",
    "高亮",
    "文字",
    "关闭",
];
/// 各按钮的快捷键（与 [`BTN_ORDER`] 一一对应，显示在提示文字之后）
const SHORTCUTS: [&str; 14] = [
    "Ctrl+T",
    "Ctrl+S",
    "Ctrl+C",
    "Ctrl+O",
    "R",
    "A",
    "N",
    "E",
    "L",
    "P",
    "B",
    "H",
    "T",
    "Esc",
];
/// 悬停后弹出提示气泡的延时（毫秒）
/// 悬停提示延时（毫秒）由配置提供，见 config.ini [ui] tooltip_delay_ms。
/// 提示气泡定时器 ID
const TIMER_TIP: usize = 779;
/// VK_ESCAPE（关闭）/ VK_T / VK_S / VK_C / VK_O（配合 Ctrl 触发对应动作）
const VK_ESCAPE: usize = 0x1B;
const VK_T: usize = 0x54;
const VK_S: usize = 0x53;
const VK_C: usize = 0x43;
const VK_O: usize = 0x4F;
/// 标注工具键：R=矩形 / A=箭头 / N=步骤序号 / E=椭圆 / L=线段 / P=画笔 /
/// B=马赛克 / H=高亮（无 Ctrl，见 5.5）；T 在无 Ctrl 时 = 文字工具（有 Ctrl 时 = 贴图）。
const VK_R: usize = 0x52;
const VK_A: usize = 0x41;
const VK_N: usize = 0x4E;
const VK_E: usize = 0x45;
const VK_L: usize = 0x4C;
const VK_P: usize = 0x50;
const VK_B: usize = 0x42;
const VK_H: usize = 0x48;
/// 撤销/重做/删除：Ctrl+Z / Ctrl+Y / Delete（转发给冻结层，见 5.5）
const VK_Z: usize = 0x5A;
const VK_Y: usize = 0x59;
const VK_DELETE: usize = 0x2E;
/// VK_CONTROL（GetKeyState 读物理按键状态）
const VK_CONTROL: i32 = 0x11;

/// 工具栏布局尺寸来自配置（逻辑像素，96 DPI 基准），见 config.ini [ui] 段。
fn btn_sz() -> i32 {
    crate::config::get().ui_toolbar_btn_size
}
fn gap_l() -> i32 {
    crate::config::get().ui_toolbar_gap
}
fn radius_l() -> i32 {
    crate::config::get().ui_toolbar_radius
}
/// 逻辑客户区尺寸：仅容纳一行方形图标按钮，力求紧凑。
fn logical_size() -> (i32, i32) {
    let c = crate::config::get();
    let n = BTN_ORDER.len() as i32;
    let w = c.ui_toolbar_btn_size * n + c.ui_toolbar_gap * (n - 1) + c.ui_toolbar_pad_h * 2;
    let h = c.ui_toolbar_pad_v * 2 + c.ui_toolbar_btn_size;
    (w, h)
}

struct ToolbarData {
    image: Arc<CapturedImage>,
    /// 图标按钮，顺序见 [`BTN_ORDER`]
    buttons: [HWND; 14],
    /// 工具栏皮肤：固定深色
    skin: theme::Theme,
    /// Mica 背景是否启用（工具栏固定深色为纯色面，恒为 false）
    backdrop: bool,
    /// 按钮悬停动画
    fx: util::HoverFx,
    /// 悬停提示气泡窗口
    tip: tooltip::Tip,
    /// 已武装提示延时的按钮 ID（0 = 无）
    tip_for: usize,
    /// 提示气泡当前是否可见
    tip_shown: bool,
    /// 冻结层窗口（转发工具切换/按键；见 5.5）
    overlay: HWND,
    /// 冻结层 [`Selection`]（只读借用其标注文档做导出合成；见 6）
    sel: *const Selection,
    /// 当前激活的标注工具按钮 ID（0 = 无），用于高亮
    active_tool: usize,
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
///
/// `overlay`：确认后的冻结层（提供窗口句柄用于消息转发、标注文档用于导出合成）。
pub fn run(image: Arc<CapturedImage>, overlay: &Selection) -> Result<()> {
    unsafe { run_impl(image, overlay) }
}

unsafe fn run_impl(image: Arc<CapturedImage>, overlay: &Selection) -> Result<()> {
    register_class_w("PixShotToolbar", toolbar_proc, false, false, false);

    let dpi = windows::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32;
    let (lw, lh) = logical_size();
    let (tw, th) = (util::scaled(lw, dpi), util::scaled(lh, dpi));

    // 定位在选区下方 8px，水平对齐选区中线，越界则钳制回虚拟屏幕内
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    let x = (image.x + (image.width - tw) / 2).clamp(vx, (vx + vw - tw).max(vx));
    let y = (image.y + image.height + util::scaled(8, dpi)).clamp(vy, (vy + vh - th).max(vy));

    let data = Box::into_raw(Box::new(ToolbarData {
        image,
        buttons: [HWND::default(); 14],
        skin: theme::toolbar_skin(),
        backdrop: false,
        fx: util::HoverFx::new(),
        tip: tooltip::Tip::none(),
        tip_for: 0,
        tip_shown: false,
        overlay: overlay.hwnd(),
        sel: overlay as *const Selection,
        active_tool: 0,
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
    // 显式提到置顶带最上层：框选遮罩此时是**不透明的置顶窗口**（显示冻结画面），
    // 工具条必须稳稳压在它上面才看得见。工具条本身已是 WS_EX_TOPMOST 且在其后创建，
    // 这里再显式 SetWindowPos 一次做保险（SWP_NOACTIVATE：只调 z 序，不改前台）。
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    // 模态消息循环
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = DestroyWindow(hwnd);
    Ok(())
}

/// 按 DPI 重排控件：5 个方形图标按钮横向等距排布（紧凑，不随窗口拉伸）。
unsafe fn layout(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);
    let s = |v: i32| util::scaled(v, dpi);
    // 窗口尚未成尺寸时先不动（避免控件堆在 (0,0)）
    if cw < s(btn_sz()) || ch < s(btn_sz()) {
        return;
    }

    let btn = s(btn_sz());
    let gap = s(gap_l());
    let n = d.buttons.len() as i32;
    // 水平居中：两侧留白均分剩余空间（窗口即为按钮行量身定制，差值为取整误差）
    let used = btn * n + gap * (n - 1);
    let x0 = ((cw - used) / 2).max(0);

    // 垂直居中：按钮上缘到顶部、下缘到底部的距离相等（均由客户区高算出）
    let by = ((ch - btn) / 2).max(0);
    for (i, b) in d.buttons.iter().enumerate() {
        let bx = x0 + i as i32 * (btn + gap);
        let _ = MoveWindow(*b, bx, by, btn, btn, BOOL(1));
    }
}

/// 工具栏自绘：整窗即白底圆角卡片（1px 浅灰描边），按钮图标绘制其上。
/// 除图标外不绘制任何文字。
unsafe fn paint(hwnd: HWND) {
    let Some(d) = data_of(hwnd) else { return };
    let t = d.skin;
    let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut cr = RECT::default();
    let _ = GetClientRect(hwnd, &mut cr);
    let (cw, ch) = (cr.right, cr.bottom);
    let dpi = util::dpi_of(hwnd);

    // 1. 白底铺满整个窗口
    let base = if d.backdrop { COLORREF(0) } else { t.card };
    let base_brush = CreateSolidBrush(base);
    let rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    FillRect(hdc, &rc, base_brush);
    let _ = DeleteObject(base_brush);

    // 2. 圆角 + 1px 描边（内缩 1px，避免被窗口边缘裁掉半像素）
    let brush = CreateSolidBrush(t.card);
    let pen = CreatePen(PS_SOLID, 1, t.stroke);
    let ob = SelectObject(hdc, brush);
    let op = SelectObject(hdc, pen);
    let r = util::scaled(radius_l(), dpi);
    let _ = RoundRect(hdc, 0, 0, cw, ch, r, r);
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
            let btn = util::scaled(btn_sz(), dpi);
            let gap = util::scaled(gap_l(), dpi);
            let n = BTN_ORDER.len() as i32;
            let used = btn * n + gap * (n - 1);
            let x0 = ((cr.right - used) / 2).max(0);
            let x = (((lp.0 & 0xFFFF) as u16 as i16 as i32) - r.left).max(0);
            let y = (((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32 - r.top).max(0);
            let by = ((cr.bottom - btn) / 2).max(0);
            let in_button_band = y >= by && y < by + btn && x >= x0 && x < x0 + used;
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

            // 图标按钮：按钮不带可见文本（图标自绘），窗口文本保留为提示文案
            // （可访问性/自动化可读）。先以占位尺寸创建，再由 layout 统一定位。
            for (i, &id) in BTN_ORDER.iter().enumerate() {
                d.buttons[i] = add_button(hwnd, TIP_TEXTS[i], 0, 0, 10, 10, id);
            }
            for b in d.buttons {
                d.fx.register(b);
                // 子类化以获取按钮自身的悬停/离开消息（父窗口收不到子控件上的鼠标消息）
                util::subclass(b, btn_proc);
            }
            d.tip = tooltip::Tip::new(hwnd);
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
                    let id = (wp.0 & 0xFFFF) as usize;
                    let style = if id == BTN_CLOSE {
                        BtnStyle::IconClose
                    } else {
                        BtnStyle::Icon
                    };
                    let selected = d.active_tool != 0 && id == d.active_tool;
                    draw_icon_frame(hwnd, &*dis, style, d.skin, &d.fx, icon_of(id), selected);
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
                if wp.0 == TIMER_TIP {
                    show_tip_for(d);
                    let _ = KillTimer(hwnd, TIMER_TIP);
                    handled = true;
                } else {
                    handled = d.fx.on_timer(hwnd, wp.0);
                }
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
        // 快捷键：Esc 关闭；Ctrl+T/S/C/O 分别对应贴图/保存/复制/OCR
        WM_KEYDOWN => {
            handle_shortcut(hwnd, wp.0);
            LRESULT(0)
        }
        // 字符输入（TranslateMessage 生成）：全部转发给冻结层（文字标注）。
        // 冻结层仅在文字输入中消费（追加/换行/退格）；非输入时为无操作。
        WM_CHAR => {
            if let Some(d) = data_of(hwnd) {
                let _ = PostMessageW(d.overlay, WM_APP_CHAR, wp, LPARAM(0));
            }
            LRESULT(0)
        }
        // 拖动工具条（点在按钮行以外的 HTCAPTION 区）时立即收起气泡
        WM_NCLBUTTONDOWN => {
            if let Some(d) = data_of(hwnd) {
                d.tip.hide();
                d.tip_shown = false;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        // 冻结层 → 工具条：工具已变化（点击工具按钮/Esc/工具键后回传真实工具）
        WM_APP_TOOL_CHANGED => {
            if let Some(d) = data_of(hwnd) {
                d.active_tool = tool_button_id_from_wp(wp.0);
            }
            refresh_buttons(hwnd);
            LRESULT(0)
        }

        // 冻结层 → 工具条：Esc 交还，关闭会话
        WM_APP_ESC_CLOSE => {
            close_bar(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if p != 0 {
                let mut data = Box::from_raw(p as *mut ToolbarData);
                data.fx.stop(hwnd);
                data.tip.destroy();
                // 按钮随父窗口销毁；先注销子类化登记（工具条每次截图都会重建）
                for b in data.buttons {
                    util::unsubclass(b);
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 关闭工具条：隐藏后结束其模态消息循环，回到主循环。
/// 贴图 / 保存 / 复制 / OCR / 关闭五个动作都走这里 —— 工具条是「一次性操作条」。
unsafe fn close_bar(hwnd: HWND) {
    if let Some(d) = data_of(hwnd) {
        d.tip.hide();
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
    PostQuitMessage(0);
}

/// Ctrl 是否按下（读物理按键状态，不依赖焦点/键序）。
unsafe fn ctrl_down() -> bool {
    (GetKeyState(VK_CONTROL) as u16) & 0x8000 != 0
}

/// 绘制一个图标按钮的**整帧**（`WM_DRAWITEM` 与预览测试共用）。
///
/// 双缓冲：整帧（圆角底 + 抗锯齿图标）先合成到内存位图，再一次性 `BitBlt` 上屏。
/// 直接画到按钮 DC 会分两批落屏 —— GDI+ 开始绘制前会先冲刷 GDI 队列，于是"底色"先到屏、
/// "图标"后到屏；悬停动画 60fps 下看起来就是图标一闪一闪。
unsafe fn draw_icon_frame(
    parent: HWND,
    dis: &DRAWITEMSTRUCT,
    style: BtnStyle,
    skin: theme::Theme,
    fx: &util::HoverFx,
    icon: icons::Icon,
    selected: bool,
) {
    let r = dis.rcItem;
    let (bw, bh) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
    // 激活的工具按钮恒以"悬停底 + 强调色图标"高亮
    let v = if selected { 1.0 } else { fx.val(dis.hwndItem) };
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let (_, ink) = util::btn_colors(style, skin, v, pressed);
    let ink = if selected { skin.accent } else { ink };

    let mem = CreateCompatibleDC(dis.hDC);
    let bmp = CreateCompatibleBitmap(dis.hDC, bw, bh);
    if !mem.is_invalid() && !bmp.is_invalid() {
        let old_bmp = SelectObject(mem, bmp);
        let full = RECT {
            left: 0,
            top: 0,
            right: bw,
            bottom: bh,
        };
        // 先铺父窗口卡片底色：圆角以外的四角与工具条底色一致（否则是未初始化像素）
        let card = CreateSolidBrush(skin.card);
        FillRect(mem, &full, card);
        let _ = DeleteObject(card);

        let mut dis2 = *dis;
        dis2.hDC = mem;
        dis2.rcItem = full;
        util::draw_od_button(parent, &dis2, v, style, skin);
        icons::draw(mem, full, icon, ink);

        let _ = BitBlt(dis.hDC, r.left, r.top, bw, bh, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        let _ = DeleteObject(bmp);
        let _ = DeleteDC(mem);
    } else {
        // 退让：拿不到离屏资源时直接绘制（宁可闪一下，也不能留下空白按钮）
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp);
        }
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        util::draw_od_button(parent, dis, v, style, skin);
        icons::draw(dis.hDC, r, icon, ink);
    }
}

/// 处理工具条快捷键。返回 true 表示已消费该按键。
///
/// 映射（见 5.5）：Esc / Ctrl+Z / Ctrl+Y / Delete → 转发冻结层 `WM_APP_KEY`；
/// R/A/N/B/H/T（无 Ctrl）→ 工具切换；Ctrl+T/S/C/O → 贴图/保存/复制/OCR。
/// **文字输入中**字母键放行（返回 false），让 WM_CHAR 流向冻结层作正文输入。
unsafe fn handle_shortcut(hwnd: HWND, vk: usize) -> bool {
    let Some(d) = data_of(hwnd) else {
        return false;
    };
    let overlay = d.overlay;
    match vk {
        // Esc 不再本地直接关闭：交由冻结层按标注状态决定（取消绘制/取消选中/交还关闭）
        VK_ESCAPE => {
            if PostMessageW(overlay, WM_APP_KEY, WPARAM(VK_ESCAPE), LPARAM(0)).is_err() {
                close_bar(hwnd); // 冻结层不可达时兜底关闭
            }
            true
        }
        VK_T if ctrl_down() => {
            handle_command(hwnd, BTN_PIN);
            true
        }
        VK_S if ctrl_down() => {
            handle_command(hwnd, BTN_SAVE);
            true
        }
        VK_C if ctrl_down() => {
            handle_command(hwnd, BTN_COPY);
            true
        }
        VK_O if ctrl_down() => {
            handle_command(hwnd, BTN_OCR);
            true
        }
        // 标注工具键（无 Ctrl）。文字输入中放行：字母是正文，不是工具键
        // （同步查询冻结层；输入中返回 false 不消费，WM_CHAR 随后流向冻结层）
        VK_R | VK_A | VK_N | VK_E | VK_L | VK_P | VK_B | VK_H | VK_T if !ctrl_down() => {
            if overlay_typing(overlay) {
                return false;
            }
            handle_command(hwnd, button_id_for_key(vk));
            true
        }
        // 撤销/重做：Ctrl+Z / Ctrl+Y（转发冻结层）
        VK_Z | VK_Y => {
            if !ctrl_down() {
                return false;
            }
            let _ = PostMessageW(overlay, WM_APP_KEY, WPARAM(vk), LPARAM(1));
            true
        }
        // 删除选中标注：Delete（转发冻结层，无 Ctrl）
        VK_DELETE => {
            let _ = PostMessageW(overlay, WM_APP_KEY, WPARAM(VK_DELETE), LPARAM(0));
            true
        }
        _ => false,
    }
}

/// 工具键 VK → 按钮控件 ID。
fn button_id_for_key(vk: usize) -> usize {
    match vk {
        VK_R => BTN_RECT,
        VK_A => BTN_ARROW,
        VK_N => BTN_STEP,
        VK_E => BTN_ELLIPSE,
        VK_L => BTN_LINE,
        VK_P => BTN_PEN,
        VK_B => BTN_MOSAIC,
        VK_H => BTN_HIGHLIGHT,
        VK_T => BTN_TEXT,
        _ => 0,
    }
}

/// 冻结层是否处于文字输入中（同步查询；冻结层不可达视为否）。
unsafe fn overlay_typing(overlay: HWND) -> bool {
    overlay.0 as usize != 0
        && SendMessageW(overlay, WM_APP_QUERY_TYPING, WPARAM(0), LPARAM(0)).0 != 0
}

/// 控件 ID → 图标。
fn icon_of(id: usize) -> icons::Icon {
    match id {
        BTN_PIN => icons::Icon::Pin,
        BTN_SAVE => icons::Icon::Save,
        BTN_COPY => icons::Icon::Copy,
        BTN_OCR => icons::Icon::Ocr,
        BTN_RECT => icons::Icon::Rect,
        BTN_ARROW => icons::Icon::Arrow,
        BTN_STEP => icons::Icon::StepNo,
        BTN_ELLIPSE => icons::Icon::Ellipse,
        BTN_LINE => icons::Icon::Line,
        BTN_PEN => icons::Icon::Pen,
        BTN_MOSAIC => icons::Icon::Mosaic,
        BTN_HIGHLIGHT => icons::Icon::Highlight,
        BTN_TEXT => icons::Icon::Text,
        _ => icons::Icon::Close,
    }
}

/// 工具按钮 ID → [`annot::Tool`]。
fn button_id_tool(id: usize) -> annot::Tool {
    match id {
        BTN_RECT => annot::Tool::Rect,
        BTN_ARROW => annot::Tool::Arrow,
        BTN_STEP => annot::Tool::StepNo,
        BTN_ELLIPSE => annot::Tool::Ellipse,
        BTN_LINE => annot::Tool::Line,
        BTN_PEN => annot::Tool::Pen,
        BTN_MOSAIC => annot::Tool::Mosaic,
        BTN_HIGHLIGHT => annot::Tool::Highlight,
        BTN_TEXT => annot::Tool::Text,
        _ => annot::Tool::None,
    }
}

/// [`annot::Tool`] 判别值（`WM_APP_TOOL_CHANGED` 的 WPARAM）→ 工具按钮 ID。
fn tool_button_id_from_wp(v: usize) -> usize {
    match v {
        1 => BTN_RECT,
        2 => BTN_ARROW,
        3 => BTN_STEP,
        4 => BTN_ELLIPSE,
        5 => BTN_LINE,
        6 => BTN_PEN,
        7 => BTN_MOSAIC,
        8 => BTN_HIGHLIGHT,
        9 => BTN_TEXT,
        _ => 0,
    }
}

/// 重绘全部工具按钮（激活工具变化时同步高亮）。
unsafe fn refresh_buttons(hwnd: HWND) {
    if let Some(d) = data_of(hwnd) {
        for b in d.buttons {
            let _ = raw::InvalidateRect(b, std::ptr::null(), 1);
        }
    }
}

/// 导出前把标注合成进成品图（无标注时零拷贝，见 6）。
unsafe fn compose(d: &ToolbarData) -> Arc<CapturedImage> {
    if d.sel.is_null() {
        return Arc::clone(&d.image);
    }
    let doc = (*d.sel).doc(); // 短借用：立即使用，不跨消息持有
    Arc::new(annot::composite(&d.image, doc))
}

/// 控件 ID → 在 [`BTN_ORDER`] 中的下标。
fn index_of_id(id: usize) -> Option<usize> {
    BTN_ORDER.iter().position(|&x| x == id)
}

/// 弹出 `d.tip_for` 对应按钮的提示气泡（延时到点后由其屏幕矩形定位）。
unsafe fn show_tip_for(d: &mut ToolbarData) {
    let Some(idx) = index_of_id(d.tip_for) else { return };
    let btn = d.buttons[idx];
    let mut r = RECT::default();
    if GetWindowRect(btn, &mut r).is_err() {
        return;
    }
    d.tip.show(TIP_TEXTS[idx], SHORTCUTS[idx], &r);
    d.tip_shown = true;
}

/// 图标按钮的窗口过程：补齐父窗口收不到的按钮级鼠标消息（悬停提示 + 高亮）。
/// 除这几条外全部转发给按钮原过程，保证点击/键盘等行为不变。
unsafe extern "system" fn btn_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let parent = GetParent(hwnd).unwrap_or_default();
    if !parent.is_invalid() {
        let id = GetDlgCtrlID(hwnd) as usize;
        match msg {
            WM_MOUSEMOVE => {
                if let Some(d) = data_of(parent) {
                    // 悬停高亮：父窗口在按钮上方收不到 WM_MOUSEMOVE，这里代它驱动
                    d.fx.on_mouse_move(parent);
                    // 提示延时：仅首次进入某按钮时武装定时器；已显示则不动
                    if !d.tip_shown && d.tip_for != id {
                        d.tip_for = id;
                        let _ = SetTimer(parent, TIMER_TIP, crate::config::get().ui_tooltip_delay_ms, None);
                    }
                }
                // 仅在**真实光标**落在按钮内时才武装离开跟踪。
                // 若光标不在窗口内，TrackMouseEvent 会立刻回一个 WM_MOUSELEAVE，
                // 把刚武装的提示延时一并取消（也让消息驱动的自动化测试无法验证提示）。
                let mut pt = POINT::default();
                let mut br = RECT::default();
                if GetCursorPos(&mut pt).is_ok()
                    && GetWindowRect(hwnd, &mut br).is_ok()
                    && pt.x >= br.left
                    && pt.x < br.right
                    && pt.y >= br.top
                    && pt.y < br.bottom
                {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                }
            }
            util::WM_MOUSELEAVE => {
                if let Some(d) = data_of(parent) {
                    // 收尾高亮轮询（移到父窗口其它区域时会由父窗口的 WM_MOUSEMOVE 重新开启）
                    d.fx.on_mouse_leave(parent);
                    d.tip.hide();
                    d.tip_shown = false;
                    d.tip_for = 0;
                }
                let _ = KillTimer(parent, TIMER_TIP);
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                if let Some(d) = data_of(parent) {
                    d.tip.hide();
                    d.tip_shown = false;
                }
            }
            // 按钮持有焦点时快捷键同样生效（转发给工具条的统一处理）；
            // 守卫为真即表示该键已被消费，直接返回，不再转发给按钮原过程。
            WM_KEYDOWN if handle_shortcut(parent, wp.0) => return LRESULT(0),
            _ => {}
        }
    }
    util::call_orig(hwnd, msg, wp, lp)
}

unsafe fn handle_command(hwnd: HWND, id: usize) {
    match id {
        // 关闭：不执行任何操作，直接收起工具条（与按 Esc 等价）
        BTN_CLOSE => close_bar(hwnd),
        // 工具按钮：把工具投递给冻结层；高亮由冻结层回传的 TOOL_CHANGED 同步
        BTN_RECT | BTN_ARROW | BTN_STEP | BTN_ELLIPSE | BTN_LINE | BTN_PEN | BTN_MOSAIC
        | BTN_HIGHLIGHT | BTN_TEXT => {
            let Some(d) = data_of(hwnd) else { return };
            let tool = button_id_tool(id);
            let _ = PostMessageW(d.overlay, WM_APP_SET_TOOL, WPARAM(tool as usize), LPARAM(0));
        }
        BTN_PIN => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let img = compose(d);
            match pin::create(&img) {
                Ok(_) => close_bar(hwnd),
                Err(e) => util::error_msgbox(&format!("贴图失败：{e}")),
            }
        }
        BTN_COPY => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let img = compose(d);
            // 双格式（参照 SnowShot）：CF_DIB（主）+ 注册的 "PNG" 格式
            // （现代应用粘贴优先取用 PNG）；编码失败只降级为 CF_DIB。
            let png = imageio::encode_png(&img.data, img.width, img.height).ok();
            // Arc 共享像素，copy_image 直接读切片，不再整幅复制
            match clipboard::copy_image_with_png(&img.data, img.width, img.height, png.as_deref()) {
                Ok(()) => {
                    crate::toast::show("已复制图像到剪贴板");
                    close_bar(hwnd);
                }
                Err(e) => util::error_msgbox(&format!("复制失败：{e}")),
            }
        }
        BTN_SAVE => {
            let Some(d) = data_of(hwnd) else { return };
            commit_typing_if_any(d);
            let cfg = config::get();
            let img = compose(d);
            match imageio::save_with_dialog(
                hwnd,
                &img.data,
                img.width,
                img.height,
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
            // 后台识别 → 直接写剪贴板 → 由主窗口弹 Toast（不弹结果窗口）。
            // 与贴图右键菜单的「识别文字 (OCR)」共用 ocr::recognize_to_clipboard_async。
            commit_typing_if_any(d);
            let img = compose(d);
            ocr::recognize_to_clipboard_async(
                Arc::clone(&img.data),
                img.width,
                img.height,
            );
            close_bar(hwnd);
        }
        _ => {}
    }
}

/// 导出动作前完成输入中的文字（若有）：把瞬态正文落入标注文档，避免导出丢文字。
/// 冻结层不可达（直接模式的异常路径）时跳过。
unsafe fn commit_typing_if_any(d: &ToolbarData) {
    if d.sel.is_null() {
        return;
    }
    (*d.sel).commit_typing();
}

#[cfg(test)]
mod preview {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC,
    };

    /// 渲染工具条按钮（默认态 / 悬停态 / 关闭悬停态）供人工核对。
    /// 走的就是 `WM_DRAWITEM` 用的那条双缓冲路径：`cargo test --bin pixshot render_toolbar_preview -- --ignored`
    #[test]
    #[ignore = "仅在需要人工核对工具条按钮观感时运行"]
    fn render_toolbar_preview() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let dpi = 96;
            let btn = util::scaled(btn_sz(), dpi);
            let (cols, rows) = (BTN_ORDER.len() as i32, 3);
            let (w, h) = (cols * btn, rows * btn);
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
            let hbmp =
                CreateDIBSection(HDC::default(), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap();
            let mem = CreateCompatibleDC(None);
            let old = SelectObject(mem, hbmp);

            let skin = theme::toolbar_skin();
            let card = CreateSolidBrush(skin.card);
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(mem, &full, card);
            let _ = DeleteObject(card);

            // 钉子句柄不可用（Icon 样式不读窗口文本），悬停值靠手工设置 entries 模拟
            let fake = HWND::default();
            for (row, (style, v)) in [
                (BtnStyle::Icon, 0.0f32),        // 默认态
                (BtnStyle::Icon, 1.0),           // 悬停态（淡灰底）
                (BtnStyle::IconClose, 1.0),      // 关闭按钮悬停态（红底白叉）
            ]
            .iter()
            .enumerate()
            {
                for (i, &id) in BTN_ORDER.iter().enumerate() {
                    let rc = RECT {
                        left: i as i32 * btn,
                        top: row as i32 * btn,
                        right: (i as i32 + 1) * btn,
                        bottom: (row as i32 + 1) * btn,
                    };
                    let mut fx = util::HoverFx::new();
                    fx.register(fake);
                    fx.entries[0].1 = *v; // 直接把插值设到目标态
                    fx.entries[0].2 = *v;
                    // itemState 用默认值（未按下）：外观完全由悬停插值 v 决定
                    let dis = DRAWITEMSTRUCT {
                        hDC: mem,
                        rcItem: rc,
                        hwndItem: fake,
                        ..Default::default()
                    };
                    draw_icon_frame(HWND::default(), &dis, *style, skin, &fx, icon_of(id), false);
                }
            }

            let n = (w * h) as usize;
            let mut px = std::slice::from_raw_parts(bits as *const u8, n * 4).to_vec();
            for p in px.chunks_exact_mut(4) {
                p[3] = 255;
            }
            SelectObject(mem, old);
            let _ = DeleteObject(hbmp);
            let _ = DeleteDC(mem);
            let _ = crate::imageio::save_png("target/toolbar_preview.png", &px, w, h);
        }
    }
}
