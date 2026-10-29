//! 文字输入的**子进程隔离**：主进程 `ImmDisableIME`，输入法只活在按需拉起的临时子进程里。
//!
//! 背景（见 `main.rs` 启动处注释）：文字标注需要中文输入，而一旦主进程让某个窗口
//! 成为输入激活目标，系统就会把用户输入法 DLL 注入本进程（第三方输入法实测可达
//! 数十 MB），且**装入后无法卸载**（Win32 没有 `ImmEnableIME`）。因此主进程干脆
//! 在启动时 `ImmDisableIME(-1)` 彻底拒绝注入，把"要输入法的文字输入"外包给子进程：
//!
//! - 用户点选文字工具、在画面上落点时，主进程用 `start()` 拉起 `pixshot.exe --text-input`；
//! - 子进程是个 **WS_POPUP + 标准 EDIT 控件**的小窗，它不禁用 IME，因此中文组字/候选窗
//!   由系统原生处理（不需要手工 `ImmSetCompositionWindow`），窗口贴在标注锚点上；
//! - 正文每次变化经 `WM_COPYDATA` 回传主进程（主进程据此保存"输入中的正文"，
//!   保证点别处/切工具时提交不丢字）；
//! - 结束（Esc / 焦点移走 / 主进程终止）后子进程退出，其内的输入法内存随之释放。
//!
//! 生命周期由主进程兜底：`stop()` 直接 `TerminateProcess` 子进程；子进程自身也用
//! 定时器监视 owner 窗口，owner 没了就自杀（避免主进程异常退出后残留）。

use std::process::{Child, Command};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect,
    GetStockObject, SetBkColor, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, WHITE_BRUSH,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

/// 子进程 → 主进程：正文变化（未提交）。
pub const PAYLOAD_UPDATE: usize = 0;
/// 子进程 → 主进程：结束输入（提交当前正文）。
pub const PAYLOAD_DONE: usize = 1;

/// 主进程侧持有的子进程句柄（同一时刻至多一个）。
static HELPER: Mutex<Option<Child>> = Mutex::new(None);

/// 拉起文字输入子进程；成功返回 true。失败时调用方应回退到进程内输入（仅 ASCII 可用）。
pub fn start(owner: HWND, x: i32, y: i32, font_px: i32) -> bool {
    stop();
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            crate::diag::log(&format!("[textinput] 取自身路径失败: {e}"));
            return false;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--text-input")
        .arg((owner.0 as isize).to_string())
        .arg(x.to_string())
        .arg(y.to_string())
        .arg(font_px.to_string());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：GUI 子系统下无控制台，显式声明避免任何窗口闪烁
        cmd.creation_flags(0x0800_0000);
    }
    match cmd.spawn() {
        Ok(child) => {
            crate::diag::log(&format!("[textinput] 已拉起输入子进程 pid={}", child.id()));
            // 让子进程有权把它的输入窗置前（否则可能拿不到键盘焦点）
            unsafe {
                let _ = AllowSetForegroundWindow(child.id());
            }
            *HELPER.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
            true
        }
        Err(e) => {
            crate::diag::log(&format!("[textinput] 拉子进程失败: {e}"));
            false
        }
    }
}

/// 结束文字输入子进程（幂等）。
pub fn stop() {
    let mut g = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = g.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 主进程收到 `WM_COPYDATA` 时解析载荷：`(是否结束输入, 正文)`。
/// 非本模块的 `WM_COPYDATA`（`dwData` 非 0/1）返回 `None`。
pub unsafe fn parse_payload(lp: LPARAM) -> Option<(bool, String)> {
    if lp.0 == 0 {
        return None;
    }
    let cds = &*(lp.0 as *const COPYDATASTRUCT);
    let kind = cds.dwData;
    if kind != PAYLOAD_UPDATE && kind != PAYLOAD_DONE {
        return None;
    }
    let bytes: &[u8] = if cds.cbData == 0 || cds.lpData.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize)
    };
    Some((kind == PAYLOAD_DONE, String::from_utf8_lossy(bytes).into_owned()))
}

/// 校验 `WM_COPYDATA` 的发送者窗口（wParam）是否属于当前输入子进程。
/// 同会话的任意进程都能向冻结层发 WM_COPYDATA，不校验即可被注入标注文字。
pub fn is_expected_sender(wp: WPARAM) -> bool {
    if wp.0 == 0 {
        return false;
    }
    let pid = HELPER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|c| c.id());
    let Some(pid) = pid else { return false };
    let mut sender_pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(HWND(wp.0 as *mut core::ffi::c_void), Some(&mut sender_pid));
    }
    sender_pid == pid
}

// ─────────────────────────────────────────────────────────────────
// 子进程侧实现
// ─────────────────────────────────────────────────────────────────

const CLASS: &str = "PixShotTextInput";
/// owner 存活性轮询定时器。
const TIMER_ALIVE: usize = 1;
/// owner 轮询周期（ms）：主进程异常退出后子进程最多存活这么久。
const ALIVE_POLL_MS: u32 = 400;

/// Edit 通知码 `EN_CHANGE`（WinUser）。
const EN_CHANGE_CODE: u32 = 0x0300;
const VK_ESCAPE_K: usize = 0x1B;

/// 子进程窗口状态（`GWLP_USERDATA`）。
struct Helper {
    owner: HWND,
    edit: HWND,
    font: HFONT,
    bg: HBRUSH,
    border: HBRUSH,
    /// 输入字号（物理像素，来自主进程的标注文字样式）。
    font_px: i32,
    /// 被我们替换前的 Edit 窗口过程（子类化后链式调用）。
    orig_edit_proc: isize,
    /// Edit 是否至少拿到过一次焦点。用于区分"真的失焦（点别处）"与
    /// "窗口刚建好、焦点尚未落定"，避免后者被误判为结束输入。
    had_focus: bool,
    /// 是否已发出"结束"、正在收尾（抑制重复发送与 WM_KILLFOCUS 误触发）。
    done: bool,
    /// 从未获焦的计时（TIMER_ALIVE 节拍数）：置前/焦点失败时兜底自毁，
    /// 避免输入窗永远挂在屏幕上。
    focus_wait_ticks: u32,
}

/// 子进程入口：解析 `--text-input <owner_hwnd(十进制)> <x> <y> <font_px>` 并跑输入窗口。
/// 返回进程退出码。
pub fn run_helper(args: &[String]) -> i32 {
    let Some(i) = args.iter().position(|a| a == "--text-input") else {
        return 2;
    };
    let tail = &args[i + 1..];
    if tail.len() < 4 {
        return 2;
    }
    let owner = match tail[0].parse::<isize>() {
        Ok(v) => HWND(v as *mut core::ffi::c_void),
        Err(_) => return 2,
    };
    let (x, y, font_px) = match (
        tail[1].parse::<i32>(),
        tail[2].parse::<i32>(),
        tail[3].parse::<i32>(),
    ) {
        (Ok(x), Ok(y), Ok(f)) => (x, y, f.clamp(8, 256)),
        _ => return 2,
    };
    unsafe { run_window(owner, x, y, font_px) }
}

unsafe fn run_window(owner: HWND, x: i32, y: i32, font_px: i32) -> i32 {
    // 与主进程一致的每显示器 DPI 感知：传入的 x/y 是物理像素
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    // IME/Edit 需要 COM（这里**不**调用 ImmDisableIME —— 本进程就是要用输入法）
    let _ = windows::Win32::System::Com::CoInitializeEx(
        None,
        windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
    );

    crate::util::register_class_w(CLASS, helper_proc, false, true, false);
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    let w = (font_px * 14).clamp(200, 720);
    let h = font_px * 2 + 16;

    let data = Box::into_raw(Box::new(Helper {
        owner,
        edit: HWND::default(),
        font: HFONT::default(),
        bg: HBRUSH::default(),
        border: HBRUSH::default(),
        font_px,
        orig_edit_proc: 0,
        had_focus: false,
        done: false,
        focus_wait_ticks: 0,
    }));

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR::from_raw(crate::util::to_wide(CLASS).as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
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
        return 3;
    }

    let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    // 显示后再确认一次键盘焦点（WM_CREATE 里那次可能因窗口尚未激活而落空）
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
    if !p.is_null() && !(*p).edit.is_invalid() {
        let _ = SetFocus((*p).edit);
    }

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    0
}

/// 读取 Edit 当前正文（UTF-16 → Rust String）。
unsafe fn edit_text(edit: HWND) -> String {
    let n = GetWindowTextLengthW(edit);
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(edit, &mut buf);
    if got <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..got as usize])
}

/// 把正文回传 owner：`commit=false` 为变化中，`true` 为结束输入。
/// `from` = 本进程输入窗口句柄，经 wParam 带给主进程做发送者校验
/// （同会话的其他进程不得向冻结层注入标注文字，见 `is_expected_sender`）。
unsafe fn send_text(owner: HWND, from: HWND, commit: bool, text: &str) {
    let bytes = text.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: if commit { PAYLOAD_DONE } else { PAYLOAD_UPDATE },
        cbData: bytes.len() as u32,
        lpData: if bytes.is_empty() {
            std::ptr::null_mut()
        } else {
            bytes.as_ptr() as *mut core::ffi::c_void
        },
    };
    let _ = SendMessageW(
        owner,
        WM_COPYDATA,
        WPARAM(from.0 as usize),
        LPARAM(&cds as *const _ as isize),
    );
}

/// 收尾：发出"结束"（若还没发过）并请求销毁窗口。幂等。
///
/// 销毁走**投递** WM_CLOSE（异步）：本函数可能在 Edit 子类过程的调用栈上被调
/// （Esc / WM_KILLFOCUS），同步 `DestroyWindow` 会在控件自身窗口过程内销毁
/// 该控件——销毁后还要沿子类过程返回，属重入风险点；改为下一轮消息循环处理。
unsafe fn finish(hwnd: HWND, data: *mut Helper) {
    let d = &mut *data;
    if !d.done {
        d.done = true;
        send_text(d.owner, hwnd, true, &edit_text(d.edit));
    }
    let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
}

unsafe extern "system" fn helper_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lp.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let p = (*cs).lpCreateParams as isize;
                if p != 0 {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, p);
                }
            }
            LRESULT(1)
        }
        WM_CREATE => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            if p.is_null() {
                return LRESULT(-1);
            }
            let d = &mut *p;
            let font_px = d.font_px;
            let face = crate::util::to_wide(&crate::config::get().text_font);
            d.font = CreateFontW(
                -font_px,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32,
                PCWSTR::from_raw(face.as_ptr()),
            );
            d.bg = CreateSolidBrush(COLORREF(0x00FF_FFFF));
            d.border = CreateSolidBrush(COLORREF(0x0080_8080));

            // ES_MULTILINE(0x4) | ES_AUTOVSCROLL(0x40) | ES_WANTRETURN(0x1000)
            let edit_style = WINDOW_STYLE(0x0004 | 0x0040 | 0x1000)
                | WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL;
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("EDIT"),
                PCWSTR::null(),
                edit_style,
                2,
                2,
                1,
                1,
                Some(&hwnd),
                None,
                GetModuleHandleW(None).unwrap_or_default(),
                None,
            )
            .unwrap_or_default();
            if edit.is_invalid() {
                return LRESULT(-1);
            }
            d.edit = edit;
            let _ = SendMessageW(
                edit,
                WM_SETFONT,
                WPARAM(d.font.0 as usize),
                LPARAM(1),
            );
            // 左右内边距（EM_SETMARGINS=0x00D3, EC_LEFTMARGIN|EC_RIGHTMARGIN=0x3）
            let _ = SendMessageW(edit, 0x00D3u32, WPARAM(0x3), LPARAM(8 | (8 << 16)));
            // 子类化 Edit：捕获 Esc 与失焦（编辑器之外的收尾动作）
            d.orig_edit_proc =
                SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_subproc as *const () as isize);

            // 立刻按客户区铺满 Edit（不依赖 WM_SIZE 一定到达）
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let _ = MoveWindow(
                edit,
                2,
                2,
                (rc.right - 4).max(1),
                (rc.bottom - 4).max(1),
                true,
            );

            let _ = SetTimer(hwnd, TIMER_ALIVE, ALIVE_POLL_MS, None);
            let _ = SetFocus(edit);
            LRESULT(0)
        }
        WM_COMMAND => {
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            if code == EN_CHANGE_CODE {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
                if !p.is_null() {
                    let d = &mut *p;
                    if !d.done {
                        send_text(d.owner, hwnd, false, &edit_text(d.edit));
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wp.0 == TIMER_ALIVE {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
                if !p.is_null() {
                    let (owner_gone, never_focused) = {
                        let d = &mut *p;
                        if !d.had_focus && !d.done {
                            d.focus_wait_ticks += 1;
                        }
                        let gone = d.owner.is_invalid() || !IsWindow(d.owner).as_bool();
                        // 约 5 秒（12×400ms）从未获焦：置前/焦点失败，不能永远挂着
                        (gone, !d.had_focus && d.focus_wait_ticks >= 12)
                    };
                    if owner_gone {
                        // owner 窗口没了（主进程退出/会话结束）→ 自杀，避免残留
                        let _ = DestroyWindow(hwnd);
                    } else if never_focused {
                        finish(hwnd, p);
                    }
                }
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            if !p.is_null() {
                let d = &*p;
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FillRect(HDC(wp.0 as *mut core::ffi::c_void), &rc, d.bg);
            }
            LRESULT(1)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            if !p.is_null() {
                let d = &*p;
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                FrameRect(hdc, &rc, d.border);
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_SIZE => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            if !p.is_null() {
                let d = &*p;
                if !d.edit.is_invalid() {
                    let _ = MoveWindow(
                        d.edit,
                        2,
                        2,
                        ((lp.0 & 0xFFFF) as i16 as i32 - 4).max(1),
                        (((lp.0 >> 16) & 0xFFFF) as i16 as i32 - 4).max(1),
                        true,
                    );
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetBkColor(hdc, COLORREF(0x00FF_FFFF));
            SetTextColor(hdc, COLORREF(0x0000_0000));
            if !p.is_null() {
                return LRESULT((*p).bg.0 as isize);
            }
            LRESULT(GetStockObject(WHITE_BRUSH).0 as isize)
        }
        WM_CLOSE => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Helper;
            // 主进程主动终止输入：正文已由 update 回传，这里不再发 done，直接收尾
            if !p.is_null() {
                (*p).done = true;
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut Helper;
            if !p.is_null() {
                let d = Box::from_raw(p);
                let _ = KillTimer(hwnd, TIMER_ALIVE);
                if !d.edit.is_invalid() {
                    let _ = DestroyWindow(d.edit);
                }
                if !d.font.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(d.font.0));
                }
                if !d.bg.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(d.bg.0));
                }
                if !d.border.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(d.border.0));
                }
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Edit 的子类过程：Esc = 结束输入；失焦 = 结束输入（点别处即完成）。
unsafe extern "system" fn edit_subproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let parent = GetParent(hwnd).unwrap_or_default();
    let p = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut Helper;
    let orig = if p.is_null() { 0 } else { (*p).orig_edit_proc };

    if !p.is_null() {
        match msg {
            WM_KEYDOWN if wp.0 == VK_ESCAPE_K => {
                finish(parent, p);
                return LRESULT(0);
            }
            WM_SETFOCUS => {
                (*p).had_focus = true;
            }
            WM_KILLFOCUS => {
                // 只有"确实拿到过焦点"才算用户点了别处；否则视为焦点尚未落定
                if (*p).had_focus && !(*p).done {
                    finish(parent, p);
                    return LRESULT(0);
                }
            }
            _ => {}
        }
    }

    // 链回原始 Edit 过程
    if orig != 0 {
        let f: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
            std::mem::transmute(orig);
        return f(hwnd, msg, wp, lp);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
