//! 截图操作工具栏（Fluent：无边框自绘标题栏 / 圆角卡片 / 悬停动画）。
//!
//! 从 `main.rs` 拆出（原本与托盘/热键/流程编排挤在同一文件，接近千行）。
//! 皮肤固定深色（见 `theme::toolbar_skin`），布局按窗口 DPI 换算并在 `WM_DPICHANGED` 重排。
//!
//! 按钮为**纯图标**（矢量自绘，见 [`crate::icons`]），依次为
//! 贴图 / 保存图像 / 复制并关闭 / 复制识别到的文字 / 关闭；
//! 悬停延时后在图标上方弹出提示气泡（[`crate::tooltip`]）。
//!
//! 控件 ID 保持稳定（E2E 用 `Find-ChildById` 定位，见 `scripts/gui_e2e_*.ps1`）：
//! 1=贴图 / 2=复制 / 3=保存 / 4=OCR / 5=关闭。

use std::sync::Arc;

use crate::capture::{self, CapturedImage};
use crate::config;
use crate::error::{AppError, Result};
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
/// 按钮在窗口内的从左到右排列顺序（图标顺序 = 需求给定顺序）
const BTN_ORDER: [usize; 5] = [BTN_PIN, BTN_SAVE, BTN_COPY, BTN_OCR, BTN_CLOSE];
/// 各按钮的悬停提示文字（与 [`BTN_ORDER`] 一一对应）
const TIP_TEXTS: [&str; 5] = ["贴图", "保存图像", "复制并关闭", "复制识别到的文字", "关闭"];
/// 各按钮的快捷键（与 [`BTN_ORDER`] 一一对应，显示在提示文字之后）
const SHORTCUTS: [&str; 5] = ["Ctrl+T", "Ctrl+S", "Ctrl+C", "Ctrl+O", "Esc"];
/// 悬停后弹出提示气泡的延时（毫秒）
const TIP_DELAY_MS: u32 = 450;
/// 提示气泡定时器 ID
const TIMER_TIP: usize = 779;
/// VK_ESCAPE（关闭）/ VK_T / VK_S / VK_C / VK_O（配合 Ctrl 触发对应动作）
const VK_ESCAPE: usize = 0x1B;
const VK_T: usize = 0x54;
const VK_S: usize = 0x53;
const VK_C: usize = 0x43;
const VK_O: usize = 0x4F;
/// VK_CONTROL（GetKeyState 读物理按键状态）
const VK_CONTROL: i32 = 0x11;

/// 图标按钮边长（逻辑像素）——方形，紧凑。
const TB_BTN_SZ: i32 = 32;
/// 按钮之间的间隔（逻辑像素）。
const TB_GAP: i32 = 2;
/// 按钮到窗口上下边缘的内边距（逻辑像素，上下一致，见 `layout`）。
const TB_PAD_V: i32 = 6;
/// 按钮到窗口左右边缘的内边距（逻辑像素）。
const TB_PAD_H: i32 = 6;
/// 逻辑客户区尺寸（96 DPI 基准）：仅容纳一行方形图标按钮，力求紧凑。
const TB_W: i32 = TB_BTN_SZ * 5 + TB_GAP * 4 + TB_PAD_H * 2; // 180
/// 高度 = 上内边距 + 按钮 + 下内边距。
const TB_H: i32 = TB_PAD_V * 2 + TB_BTN_SZ; // 44
/// 窗口圆角半径（逻辑像素）。
const TB_RADIUS: i32 = 8;

struct ToolbarData {
    image: Arc<CapturedImage>,
    /// 五个图标按钮，顺序见 [`BTN_ORDER`]
    buttons: [HWND; 5],
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

    // 定位在选区下方 8px，水平对齐选区中线，越界则钳制回虚拟屏幕内
    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    let x = (image.x + (image.width - tw) / 2).clamp(vx, (vx + vw - tw).max(vx));
    let y = (image.y + image.height + util::scaled(8, dpi)).clamp(vy, (vy + vh - th).max(vy));

    let data = Box::into_raw(Box::new(ToolbarData {
        image,
        buttons: [HWND::default(); 5],
        skin: theme::toolbar_skin(),
        backdrop: false,
        fx: util::HoverFx::new(),
        tip: tooltip::Tip::none(),
        tip_for: 0,
        tip_shown: false,
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
    if cw < s(TB_BTN_SZ) || ch < s(TB_BTN_SZ) {
        return;
    }

    let btn = s(TB_BTN_SZ);
    let gap = s(TB_GAP);
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
    let r = util::scaled(TB_RADIUS, dpi);
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
            let btn = util::scaled(TB_BTN_SZ, dpi);
            let gap = util::scaled(TB_GAP, dpi);
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
                    draw_icon_frame(hwnd, &*dis, style, d.skin, &d.fx, icon_of(id));
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
) {
    let r = dis.rcItem;
    let (bw, bh) = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
    let v = fx.val(dis.hwndItem);
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    // 只需前景（描边）色：底色由 util::draw_od_button 画，图标不再需要底色遮罩
    let (_, ink) = util::btn_colors(style, skin, v, pressed);

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
        util::draw_od_button(parent, &dis2, fx, style, skin);
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
        util::draw_od_button(parent, dis, fx, style, skin);
        icons::draw(dis.hDC, r, icon, ink);
    }
}

/// 处理工具条快捷键。返回 true 表示已消费该按键。
/// 映射见 [`SHORTCUTS`]：Esc=关闭，Ctrl+T=贴图，Ctrl+S=保存，Ctrl+C=复制，Ctrl+O=OCR。
unsafe fn handle_shortcut(hwnd: HWND, vk: usize) -> bool {
    let (id, need_ctrl) = match vk {
        VK_ESCAPE => return {
            close_bar(hwnd);
            true
        },
        VK_T => (BTN_PIN, true),
        VK_S => (BTN_SAVE, true),
        VK_C => (BTN_COPY, true),
        VK_O => (BTN_OCR, true),
        _ => return false,
    };
    if need_ctrl && !ctrl_down() {
        return false;
    }
    handle_command(hwnd, id);
    true
}

/// 控件 ID → 图标。
fn icon_of(id: usize) -> icons::Icon {
    match id {
        BTN_PIN => icons::Icon::Pin,
        BTN_SAVE => icons::Icon::Save,
        BTN_COPY => icons::Icon::Copy,
        BTN_OCR => icons::Icon::Ocr,
        _ => icons::Icon::Close,
    }
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
                        let _ = SetTimer(parent, TIMER_TIP, TIP_DELAY_MS, None);
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
            // 后台识别 → 直接写剪贴板 → 由主窗口弹 Toast（不弹结果窗口）。
            // 与贴图右键菜单的「识别文字 (OCR)」共用 ocr::recognize_to_clipboard_async。
            ocr::recognize_to_clipboard_async(Arc::clone(&d.image.data), d.image.width, d.image.height);
            close_bar(hwnd);
        }
        _ => {}
    }
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
            let btn = util::scaled(TB_BTN_SZ, dpi);
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
                    draw_icon_frame(HWND::default(), &dis, *style, skin, &fx, icon_of(id));
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
