//! 托盘图标 + 全局热键 + 单实例保护 + 主窗口消息过程。
//!
//! 从 `main.rs` 拆出（原文件近千行）。这里集中了所有「最容易静默失败」的系统集成：
//! `Shell_NotifyIcon`、`RegisterHotKey`、`SetWindowsHookEx` —— 每一次调用都写诊断日志。
//!
//! 两个关键修复（见各函数注释）：
//! 1. **托盘回调协议按版本分派**：早期实现把 V0 的 `lParam=事件` 误判为 V4 布局，
//!    导致旧外壳下右键菜单坐标被解析成图标 ID（菜单跑到屏幕左上角）。
//! 2. **低级键盘钩子遵守修饰键**：钩子不占热键槽位，因此在配置了 `Ctrl+Shift+S`
//!    这类组合键时必须自行校验修饰键状态，否则单独按 S 也会触发截图。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, RegisterHotKey};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config;
use crate::diag;
use crate::menu;
use crate::util;

const WM_APP_TRAY: u32 = WM_USER + 1;
const TRAY_UID: u32 = 1;
const HOTKEY_ID: i32 = 1;
/// NIN_SELECT / NIN_KEYSELECT（V4 通知事件；右键事件即 WM_CONTEXTMENU 0x7B）
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;
/// LLKHF_INJECTED：注入按键标记（钩子中忽略，避免与自动化工具相互触发）
const LLKHF_INJECTED: u32 = 0x10;
/// ERROR_ALREADY_EXISTS
const ERROR_ALREADY_EXISTS: u32 = 183;
/// VK_SHIFT / VK_CONTROL / VK_MENU / VK_LWIN / VK_RWIN
const VK_SHIFT_V: i32 = 0x10;
const VK_CONTROL_V: i32 = 0x11;
const VK_MENU_V: i32 = 0x12;
const VK_LWIN_V: i32 = 0x5B;
const VK_RWIN_V: i32 = 0x5C;

/// 托盘是否已成功升级到 `NOTIFYICON_VERSION_4` 回调协议。
static TRAY_V4: AtomicBool = AtomicBool::new(false);
/// TaskbarCreated 消息 id（0 = 尚未注册）
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
/// 钩子宿主窗口
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);
/// 钩子要匹配的虚拟键码与修饰键
static HOOK_VK: AtomicU32 = AtomicU32::new(0);
static HOOK_MODS: AtomicU32 = AtomicU32::new(0);
/// 已安装的钩子句柄
static LL_HOOK: AtomicIsize = AtomicIsize::new(0);
/// 单实例互斥体句柄（进程存活期间保持；进程退出由系统释放）
static INSTANCE_MUTEX: AtomicIsize = AtomicIsize::new(0);
/// 托盘菜单是否正在显示（一次真实右键会投递两条回调，必须去重）
static TRAY_MENU_BUSY: AtomicBool = AtomicBool::new(false);

// ─── 单实例保护 ──────────────────────────────────────────────────

/// 获取单实例锁。false = 已有实例在运行。
pub fn acquire_single_instance() -> bool {
    unsafe {
        let name = util::to_wide(r"Local\PixShot_SingleInstance_v1");
        match windows::Win32::System::Threading::CreateMutexW(
            None,
            false,
            PCWSTR::from_raw(name.as_ptr()),
        ) {
            Ok(h) => {
                let err = windows::Win32::Foundation::GetLastError().0;
                if err == ERROR_ALREADY_EXISTS {
                    return false;
                }
                INSTANCE_MUTEX.store(h.0 as isize, Ordering::SeqCst);
                diag::log(&format!(
                    "[single] 已获取单实例互斥体 handle=0x{:X}",
                    h.0 as isize
                ));
                true
            }
            Err(e) => {
                // 无法判定时不阻塞启动（宁可多开，也不要打不开）
                diag::log(&format!("[single] CreateMutexW 失败({e})，跳过单实例检查"));
                true
            }
        }
    }
}

// ─── 托盘：注册 / 恢复 / 回调 ────────────────────────────────────

/// 托盘提示文本（含当前热键）
fn tray_tip() -> String {
    format!(
        "PixShot：按 {} 截图（右键退出）",
        config::get().hotkey_label()
    )
}

/// 注册并缓存 TaskbarCreated 消息 id。Explorer 重启后会广播该消息，
/// 此时必须重新 `Shell_NotifyIcon(NIM_ADD)`，否则托盘图标永久消失。
fn taskbar_created_msg() -> u32 {
    let cached = TASKBAR_CREATED.load(Ordering::SeqCst);
    if cached != 0 {
        return cached;
    }
    unsafe {
        let s = util::to_wide("TaskbarCreated");
        let id = RegisterWindowMessageW(PCWSTR::from_raw(s.as_ptr()));
        TASKBAR_CREATED.store(id, Ordering::SeqCst);
        diag::log(&format!("[tray] TaskbarCreated 消息 id=0x{id:X}"));
        id
    }
}

/// 加载应用图标：优先从自身模块资源（build.rs 嵌入的 ID=1 多尺寸图标）取
/// 当前系统小图标尺寸（DPI 感知），失败回退系统默认图标。
fn load_app_icon() -> HICON {
    unsafe {
        let cx = GetSystemMetrics(SM_CXSMICON).max(16);
        let cy = GetSystemMetrics(SM_CYSMICON).max(16);
        let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
            .unwrap_or_default();
        // MAKEINTRESOURCE(1)：嵌入资源的图标组 ID
        let name = PCWSTR::from_raw(1usize as *const u16);
        match LoadImageW(HINSTANCE(hinst.0), name, IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
            Ok(h) => {
                diag::log(&format!(
                    "[icon] LoadImageW(module=0x{:X}, ID=1, {}x{}) -> OK hIcon=0x{:X}",
                    hinst.0 as usize, cx, cy, h.0 as usize
                ));
                HICON(h.0)
            }
            Err(e) => {
                let fb = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
                diag::log(&format!(
                    "[icon] LoadImageW 失败({e}) -> 回退 LoadIconW(IDI_APPLICATION) hIcon=0x{:X}",
                    fb.0 as usize
                ));
                fb
            }
        }
    }
}

/// 注册托盘图标：5 次重试（外壳可能尚未就绪）+ 成功后升级 V4 回调协议。
/// 返回 (是否成功, 最后一次 Win32 错误码)。
fn add_tray_icon(hwnd: HWND) -> (bool, u32) {
    let mut tip = [0u16; 128];
    for (i, c) in tray_tip().encode_utf16().take(127).enumerate() {
        tip[i] = c;
    }
    let icon = load_app_icon();
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: icon,
        szTip: tip,
        ..Default::default()
    };
    unsafe {
        let mut last_err = 0u32;
        for attempt in 1..=5u32 {
            let ok = Shell_NotifyIconW(NIM_ADD, &nid);
            last_err = windows::Win32::Foundation::GetLastError().0;
            diag::log(&format!(
                "[tray] NIM_ADD 第 {attempt}/5 次 -> {} (Win32 错误 {last_err}, hIcon=0x{:X})",
                ok.as_bool(),
                icon.0 as usize
            ));
            if ok.as_bool() {
                // 升级到 NOTIFYICON_VERSION_4：现代回调协议（坐标/事件更可靠）
                let mut v = nid;
                v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let sok = Shell_NotifyIconW(NIM_SETVERSION, &v);
                // 只有 SETVERSION 成功，回调布局才是 V4；否则必须按 V0 解析
                TRAY_V4.store(sok.as_bool(), Ordering::SeqCst);
                diag::log(&format!(
                    "[tray] NIM_SETVERSION({}) -> {} (Win32 错误 {}) => 按 {} 解析回调",
                    NOTIFYICON_VERSION_4,
                    sok.as_bool(),
                    windows::Win32::Foundation::GetLastError().0,
                    if sok.as_bool() { "V4" } else { "V0/V3" }
                ));
                return (true, last_err);
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        TRAY_V4.store(false, Ordering::SeqCst);
        (false, last_err)
    }
}

fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_UID,
        ..Default::default()
    };
    unsafe {
        let ok = Shell_NotifyIconW(NIM_DELETE, &nid);
        diag::log(&format!(
            "[tray] Shell_NotifyIconW(NIM_DELETE) -> {}",
            ok.as_bool()
        ));
    }
}

// ─── 热键：RegisterHotKey 主路径 + 低级键盘钩子兜底 ──────────────

/// 当前热键绑定（配置 → (修饰键, 虚拟键码)）；无法解析时回退 F1。
fn hotkey_binding() -> (u32, u32) {
    let cfg = config::get();
    match config::parse_hotkey(&cfg.hotkey) {
        Some(b) => b,
        None => {
            diag::log(&format!(
                "[hotkey] 配置热键 {:?} 无法解析 -> 回退 F1",
                cfg.hotkey
            ));
            (config::MOD_NOREPEAT, 0x70)
        }
    }
}

fn register_hotkey(hwnd: HWND, mods: u32, vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS;
    unsafe {
        let r = RegisterHotKey(hwnd, HOTKEY_ID, HOT_KEY_MODIFIERS(mods), vk);
        diag::log(&format!(
            "[hotkey] RegisterHotKey(hwnd=0x{:X}, id={}, mods=0x{mods:X}, vk=0x{vk:X}) -> {}, GetLastError={}",
            hwnd.0 as usize,
            HOTKEY_ID,
            r.is_ok(),
            windows::Win32::Foundation::GetLastError().0
        ));
        r.is_ok()
    }
}

/// 当前是否按下某个修饰键。
fn key_down(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) < 0 }
}

/// 钩子回调中对修饰键做严格校验（钩子不占热键槽位，不校验会误触发）。
fn modifiers_match(mods: u32) -> bool {
    let want_ctrl = mods & config::MOD_CONTROL != 0;
    let want_alt = mods & config::MOD_ALT != 0;
    let want_shift = mods & config::MOD_SHIFT != 0;
    let want_win = mods & config::MOD_WIN != 0;

    let has_ctrl = key_down(VK_CONTROL_V);
    let has_alt = key_down(VK_MENU_V);
    let has_shift = key_down(VK_SHIFT_V);
    let has_win = key_down(VK_LWIN_V) || key_down(VK_RWIN_V);

    has_ctrl == want_ctrl && has_alt == want_alt && has_shift == want_shift && has_win == want_win
}

/// 是否放行「注入按键」（`keybd_event` / `SendInput` 产生的按键）。
///
/// 默认 **拒绝**：避免自动化工具（键盘宏、其他测试脚本）误触发截图。
/// 但 E2E 只能通过注入按键模拟 F1，且当环境里 F1 已被其他程序占用时
/// （`RegisterHotKey` 返回 1409）必然走钩子兜底路径——此时若继续拒绝注入，
/// 该路径就无法被自动化验证。故提供 `PIXSHOT_ALLOW_INJECTED=1` 显式放开，
/// 仅用于测试，不影响默认行为。
static ALLOW_INJECTED: AtomicBool = AtomicBool::new(false);

/// WH_KEYBOARD_LL 回调：捕获物理按键并投递 WM_HOTKEY（与 RegisterHotKey
/// 共用同一条已充分验证的触发链路）。低级键盘钩子不占用热键槽位，
/// 因此目标热键被其他程序占用时依然可用。
unsafe extern "system" fn ll_keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // code == HC_ACTION(0) 时 lp 才是有效的 KBDLLHOOKSTRUCT
    if code == 0 && lp.0 != 0 {
        let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        let is_down = wp.0 == WM_KEYDOWN as usize || wp.0 == WM_SYSKEYDOWN as usize;
        let injected = (kb.flags.0 & LLKHF_INJECTED) != 0;
        let allow_injected = ALLOW_INJECTED.load(Ordering::Relaxed);
        let want_vk = HOOK_VK.load(Ordering::Relaxed);
        if is_down && (allow_injected || !injected) && want_vk != 0 && kb.vkCode == want_vk {
            if modifiers_match(HOOK_MODS.load(Ordering::Relaxed)) {
                let p = HOOK_HWND.load(Ordering::Relaxed);
                if p != 0 {
                    // 钩子回调必须尽快返回，避免拖慢全系统输入
                    let _ = PostMessageW(
                        HWND(p as *mut core::ffi::c_void),
                        WM_HOTKEY,
                        WPARAM(HOTKEY_ID as usize),
                        LPARAM(0),
                    );
                }
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wp, lp)
}

/// 安装低级键盘钩子作为热键兜底。返回是否成功。
fn install_keyboard_hook(hwnd: HWND, mods: u32, vk: u32) -> bool {
    unsafe {
        // 仅测试用：显式设置该环境变量时，注入按键也能触发（见 ALLOW_INJECTED 注释）
        ALLOW_INJECTED.store(
            std::env::var_os("PIXSHOT_ALLOW_INJECTED").is_some(),
            Ordering::SeqCst,
        );
        HOOK_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        HOOK_VK.store(vk, Ordering::SeqCst);
        HOOK_MODS.store(mods, Ordering::SeqCst);
        let hinst =
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), HINSTANCE(hinst.0), 0) {
            Ok(h) => {
                LL_HOOK.store(h.0 as isize, Ordering::SeqCst);
                diag::log(&format!(
                    "[hotkey] 低级键盘钩子已安装 hhook=0x{:X}（vk=0x{vk:X} mods=0x{mods:X}）",
                    h.0 as isize
                ));
                true
            }
            Err(e) => {
                diag::log(&format!("[hotkey] 低级键盘钩子安装失败: {e}"));
                false
            }
        }
    }
}

/// 初始化托盘与热键。返回 (托盘成功, 托盘错误码, 热键可用)。
pub fn init(hwnd: HWND) -> (bool, u32, bool) {
    let _ = taskbar_created_msg(); // 预注册 TaskbarCreated，供 Explorer 重启后恢复
    let (tray_ok, tray_err) = add_tray_icon(hwnd);

    let (mods, vk) = hotkey_binding();
    let force_hook = std::env::var_os("PIXSHOT_FORCE_HOOK").is_some();
    let hotkey_ok = if force_hook {
        diag::log("[hotkey] PIXSHOT_FORCE_HOOK 已设置 -> 跳过 RegisterHotKey，直接使用钩子");
        install_keyboard_hook(hwnd, mods, vk)
    } else if register_hotkey(hwnd, mods, vk) {
        true
    } else {
        diag::log("[hotkey] RegisterHotKey 失败 -> 改用低级键盘钩子兜底");
        install_keyboard_hook(hwnd, mods, vk)
    };

    diag::log(&format!(
        "[boot] 集成状态: 托盘={tray_ok}(err={tray_err}), 热键可用={hotkey_ok}"
    ));
    (tray_ok, tray_err, hotkey_ok)
}

// ─── 主窗口消息过程 ──────────────────────────────────────────────

pub unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // Explorer 重启会广播 TaskbarCreated：必须重新添加图标，否则永久消失
    let tbc = taskbar_created_msg();
    if tbc != 0 && msg == tbc {
        diag::log("[tray] 收到 TaskbarCreated（Explorer 已重启）-> 重新注册托盘图标");
        let (ok, err) = add_tray_icon(hwnd);
        diag::log(&format!("[tray] 重启后恢复 -> 成功={ok} (错误 {err})"));
        return LRESULT(0);
    }

    match msg {
        WM_HOTKEY => {
            diag::log(&format!(
                "[hotkey] 收到 WM_HOTKEY: wp={} (期待 {})",
                wp.0, HOTKEY_ID
            ));
            if wp.0 == HOTKEY_ID as usize {
                crate::screenshot_flow();
            }
            LRESULT(0)
        }

        WM_APP_TRAY => {
            // 托盘回调协议解析（**按 AddTrayIcon 时 SETVERSION 的成败分派**，
            // 不再用「事件值反推版本」——那种写法会让 V0 的外壳走进 V4 分支，
            // 把图标 ID 当成坐标，右键菜单弹到屏幕左上角）。
            //
            // V4（NOTIFYICON_VERSION_4）实测布局：
            //   wParam = MAKELONG(x, y)         低字 = x，高字 = y
            //   lParam = MAKELONG(事件, 图标ID)  低字 = 事件，高字 = 图标 ID
            // 实测样本：右键(1492,922) -> wp=0x39A05D4  lp=0x1007B
            //
            // V0/V3：wParam = 图标 ID，lParam 低字 = 鼠标消息（无坐标）
            let lo_wp = (wp.0 & 0xFFFF) as u32;
            let hi_wp = ((wp.0 >> 16) & 0xFFFF) as u32;
            let lo_lp = (lp.0 & 0xFFFF) as u32;
            let hi_lp = ((lp.0 >> 16) & 0xFFFF) as u32;
            let v4 = TRAY_V4.load(Ordering::SeqCst);
            diag::log(&format!(
                "[tray] 回调({}): wp=0x{:X} lp=0x{:X} (x={lo_wp} y={hi_wp} 事件=0x{lo_lp:X} uID={hi_lp})",
                if v4 { "V4" } else { "V0" },
                wp.0,
                lp.0
            ));

            if v4 {
                // 坐标可能为负（副屏），按 i16 解释
                let x = lo_wp as u16 as i16 as i32;
                let y = hi_wp as u16 as i16 as i32;
                match lo_lp {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT => {
                        crate::screenshot_flow()
                    }
                    WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(hwnd, x, y),
                    _ => {}
                }
            } else {
                match lo_lp {
                    WM_LBUTTONUP | WM_LBUTTONDBLCLK => crate::screenshot_flow(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        // V0 不携带坐标：取当前光标位置
                        let mut pt = windows::Win32::Foundation::POINT::default();
                        let _ = GetCursorPos(&mut pt);
                        tray_menu(hwnd, pt.x, pt.y);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }

        // 部分外壳/V4 实现会把右键直接作为窗口消息投递过来：一并兜底处理，
        // 避免"右键无菜单"的兼容性差异。该窗口仅作托盘宿主，不会收到其他来源的 WM_CONTEXTMENU。
        WM_CONTEXTMENU => {
            diag::log("[tray] 收到 WM_CONTEXTMENU 窗口消息 -> 弹出托盘菜单");
            let mut pt = windows::Win32::Foundation::POINT::default();
            let _ = GetCursorPos(&mut pt);
            tray_menu(hwnd, pt.x, pt.y);
            LRESULT(0)
        }

        WM_DESTROY => {
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 托盘右键菜单（仅「退出」）。`(x,y)` 为回调携带的屏幕坐标。
///
/// 使用自绘 Fluent 弹窗而非系统 `TrackPopupMenuEx`：后者要求宿主窗口能成为
/// 前台窗口，而托盘宿主是隐藏窗口，实测会**立即返回 0 且不显示菜单**。
unsafe fn tray_menu(hwnd: HWND, x: i32, y: i32) {
    // 去重：菜单已显示时忽略重复回调，避免嵌套菜单
    if TRAY_MENU_BUSY.swap(true, Ordering::SeqCst) {
        diag::log("[tray] 菜单已在显示，忽略重复回调");
        return;
    }
    let quit = menu::show_quit(x, y);
    TRAY_MENU_BUSY.store(false, Ordering::SeqCst);
    diag::log(&format!("[tray] 自绘菜单返回 quit={quit}"));
    if quit {
        // 完全退出：置退出标志（截图流程进行中时向外层传播）并结束主循环
        util::request_quit();
        // 先清掉托盘图标，避免进程退出后留下"幽灵图标"
        remove_tray_icon(hwnd);
        PostQuitMessage(0);
    }
}
