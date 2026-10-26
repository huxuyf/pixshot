//! PixShot —— 截图 / 贴图 / OCR（Rust + Win32 + 本地 ONNX 推理）
//!
//! 流程：热键（默认 F1）/ 托盘 → 框选 → 从同一帧快照裁剪 → 操作工具栏
//! （贴图 / 复制 / 保存 / OCR）。贴图窗口置顶显示，支持拖动、滚轮缩放、
//! 右键菜单（含 OCR）、双击或 Esc 关闭。
//!
//! 模块划分（按职责分层，避免单文件膨胀）：
//! - `tray`    托盘图标 / 全局热键 / 单实例 / 主窗口消息过程
//! - `toolbar` 截图操作工具栏
//! - `overlay` 框选遮罩（唯一一次全屏捕获）
//! - `pin` / `menu` / `toast`  UI 窗口与自绘控件
//! - `capture` / `imageio` / `clipboard` / `ocr` 能力层
//! - `theme` / `util`  视觉与 Win32 辅助
//! - `config` / `diag` / `error`  配置、诊断、错误类型
//! - `autostart`  开机自启（写入 HKCU 的 Run 注册表项）

#![windows_subsystem = "windows"]

mod annot;
mod autostart;
mod capture;
mod clipboard;
mod config;
mod cursor;
mod diag;
mod error;
mod icons;
mod imageio;
mod menu;
mod ocr;
mod overlay;
mod pin;
mod svgpath;
mod theme;
mod toast;
mod toolbar;
mod tooltip;
mod tray;
mod util;

use std::panic::AssertUnwindSafe;

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostQuitMessage, TranslateMessage, MSG, WS_OVERLAPPEDWINDOW,
};

fn main() {
    // panic 可见性：本程序是 windows 子系统（无控制台），默认 panic 完全静默。
    install_panic_hook();

    // 单实例保护（先于日志初始化，避免第二实例截断第一实例的诊断日志）。
    // 若允许多开：后启动的实例 RegisterHotKey 必然失败（旧实例占用），
    // 用户会看到「进程在、但按键无反应」的困惑局面。
    if !tray::acquire_single_instance() {
        diag::log("[single] 检测到已有实例在运行 -> 本次启动退出");
        if alerts_enabled() {
            util::info_msgbox("PixShot已启动，无需再次执行。");
        }
        return;
    }

    diag::begin_session();
    diag::log(&format!(
        "[boot] main() 进入，exe={:?}, args={:?}",
        std::env::current_exe(),
        std::env::args().collect::<Vec<_>>()
    ));

    // 本应用无任何文本输入（框选/贴图/只读 OCR 结果），在首个窗口创建前禁用
    // 当前线程 IME，阻止输入法 DLL 注入（实测可省 ~34MB 内存、消除 ~1% 空闲 CPU）
    unsafe {
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::Input::Ime::ImmDisableIME;
        let _ = ImmDisableIME(GetCurrentThreadId());
        diag::log("[boot] ImmDisableIME 已调用");
    }

    // 每显示器 DPI 感知：保证截屏/窗口坐标为物理像素
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }

    // 配置加载失败/非法项不影响启动（内部一律回退默认值）
    let _ = config::get();

    // --smoke：无 GUI 冒烟测试（全屏截图 → 保存 → OCR），用于验证核心链路
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--smoke" || a == "smoke") {
        std::process::exit(run_smoke());
    }

    if let Err(e) = run_gui() {
        util::error_msgbox(&format!("PixShot 启动失败：{e}"));
        std::process::exit(1);
    }
}

/// 安装 panic hook：写入诊断日志（含位置），保证"程序突然消失"可追查。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "?".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".into());
        diag::log(&format!("[panic] {msg} @ {loc}"));
        // windows 子系统下 stderr 通常不可见，但保留给 --smoke / 调试器附加场景
        eprintln!("[panic] {msg} @ {loc}");
    }));
}

/// 是否允许弹窗提示（自动化测试可设 PIXSHOT_NO_UI_ALERTS=1 屏蔽）。
fn alerts_enabled() -> bool {
    std::env::var_os("PIXSHOT_NO_UI_ALERTS").is_none()
}

// ─── GUI 主流程 ──────────────────────────────────────────────────

fn run_gui() -> error::Result<()> {
    diag::log(&format!(
        "[boot] 启动 run_gui，exe={:?}",
        std::env::current_exe()
    ));
    // 记录进程完整性：低完整性会让托盘注册必然失败（UIPI），必须显式可见
    let il = diag::integrity_rid();
    diag::log(&format!(
        "[boot] 进程完整性 = {}",
        il.map(|r| format!("{} (0x{r:X})", diag::integrity_name(r)))
            .unwrap_or_else(|| "未知".into())
    ));
    let low_il = diag::is_low_integrity();
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    util::register_class_w("PixShotMain", tray::main_proc, false, false, false);
    let hwnd = util::create_window(
        "PixShotMain",
        "PixShot",
        windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
        WS_OVERLAPPEDWINDOW, // 隐藏窗口：承载托盘与热键
        0,
        0,
        0,
        0,
        std::ptr::null_mut(),
    )?;
    diag::log(&format!(
        "[boot] 主窗口创建成功 hwnd=0x{:X}",
        hwnd.0 as usize
    ));

    let (tray_ok, tray_err, hotkey_ok) = tray::init(hwnd);

    // 托盘失败会让程序"没有可见入口"，必须让用户知道原因（测试可用环境变量屏蔽弹窗）
    if !tray_ok && alerts_enabled() {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.display().to_string()))
            .unwrap_or_default();
        let cause = if low_il || tray_err == 5 {
            format!(
                "【已定位原因】本进程处于低完整性，Windows 的 UIPI 机制禁止其访问资源管理器\n\
                 的托盘窗口（Shell_NotifyIcon 返回错误 5）。\n\
                 通常是因为 exe 文件带有「Low Mandatory Level」强制完整性标签\n\
                 （沙箱类工具写入目录时会打标，并被其中的 exe 继承）。\n\n\
                 任选一种修复后重启本程序：\n\
                 A) 在文件资源管理器中把 pixshot.exe 复制到别处（如桌面新建文件夹）再运行；\n\
                 B) 以【管理员】PowerShell 对其所在目录执行：\n\
                    icacls \"{exe_dir}\" /setintegritylevel (OI)(CI)Medium /T\n\
                 （本进程当前完整性：{}）",
                il.map(|r| diag::integrity_name(r).to_string())
                    .unwrap_or_else(|| "未知".into())
            )
        } else {
            format!(
                "Shell_NotifyIcon 返回失败（Win32 错误 {tray_err}）。\n\
                 常见原因：外壳尚未就绪、或安全软件拦截；可尝试重启「文件资源管理器」。"
            )
        };
        util::error_msgbox(&format!(
            "托盘图标注册失败。\n\n{cause}\n\n\
             其他可能：Windows 11 默认把新图标收进托盘「^」溢出区，点击任务栏 ^ 可看到。\n\n\
             诊断日志：{}",
            diag::log_path().display()
        ));
    }

    // 热键完全不可用（注册与钩子都失败）时同样必须可见，否则用户只会觉得"按键没反应"
    if !hotkey_ok && alerts_enabled() {
        util::error_msgbox(&format!(
            "全局热键 {} 注册失败，且低级键盘钩子也未能安装。\n\n\
             可尝试：\n\
             1) 关闭占用该快捷键的其他程序；\n\
             2) 编辑配置文件改一个热键（路径：{}）后重启；\n\
             3) 直接左键点击托盘图标截图（无需热键）。\n\n\
             诊断日志：{}",
            config::get().hotkey_label(),
            config::path().display(),
            diag::log_path().display()
        ));
    }

    diag::log("[boot] 进入主消息循环");
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

// ─── 截图流程编排 ────────────────────────────────────────────────

/// 截图流程入口（可被托盘/热键在任意时刻调用）。
///
/// 两层保护：
/// - `BusyGuard`：流程进行中拒绝重入，且 panic 时也会自动复位；
/// - `catch_unwind`：panic 不会穿过 `extern "system"` 窗口过程边界（那会直接 abort），
///   而是转成一次可见的错误提示。
pub(crate) fn screenshot_flow() {
    let Some(_guard) = util::BusyGuard::acquire() else {
        return; // 已有流程进行中
    };

    let r = std::panic::catch_unwind(AssertUnwindSafe(|| unsafe { flow_impl() }));
    match r {
        Ok(Ok(())) => {}
        Ok(Err(e)) => util::error_msgbox(&format!("截图流程出错：{e}")),
        Err(_) => util::error_msgbox(
            "截图流程遇到内部错误并已中止（详情见诊断日志）。\n\n程序仍可继续使用。",
        ),
    }

    // 嵌套循环可能吞掉托盘发出的退出消息，这里补发
    if util::quit_requested() {
        unsafe { PostQuitMessage(0) };
    }
}

unsafe fn flow_impl() -> error::Result<()> {
    // 1. 框选屏幕区域（模态）。返回「已裁好的成品 + 仍在显示的红框」。
    //    红框窗口在工具条关闭前一直保留，用户操作期间能看到自己框选的范围。
    let Some(sel) = overlay::select_region() else {
        return Ok(()); // 用户取消
    };
    // 2. 成品已在框选确认时从整屏快照裁好（`sel.image`），整屏快照也在那一刻随之释放。
    //    这里不再重新截屏，也不等待遮罩消失（遮罩与工具栏都设了 WDA_EXCLUDEFROMCAPTURE）。
    // 3. 操作工具栏：贴图 / 复制 / 保存 / OCR（均在导出前合成标注）+ 标注工具。
    //    工具条一旦关闭（点任一图标 / Esc / 关闭按钮），`sel` 在函数返回时析构，
    //    冻结层随之消失。标注编辑挂在冻结层窗口过程上（sel 仍存活），故须把 &sel 传入。
    let image = sel.image.clone();
    toolbar::run(image, &sel)
}

// ─── smoke 冒烟测试 ──────────────────────────────────────────────

fn run_smoke() -> i32 {
    use windows::Win32::System::Com::COINIT_MULTITHREADED;
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        // WIC/WinRT 需要 COM
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    println!("PixShot smoke 测试：全屏截图 → 保存图片 → OCR 文字识别");
    println!();

    let fmt = config::get().format;
    let out = format!("pixshot_smoke.{}", fmt.ext());

    let (vx, vy, vw, vh) = capture::virtual_screen_rect();
    println!("[1/3] 截取全屏（虚拟屏幕 {vw}×{vh} @ {vx},{vy}）...");
    let img = match capture::capture_region_ex(vx, vy, vw, vh, config::get().capture_cursor) {
        Ok(i) => {
            println!("[OK] 截屏成功：{}×{}", i.width, i.height);
            i
        }
        Err(e) => {
            println!("[失败] 截屏出错：{e}");
            return 1;
        }
    };

    println!("[2/3] 保存 {out} ...");
    match imageio::save(&out, &img.data, img.width, img.height, fmt) {
        Ok(()) => println!("[OK] 已保存 {out}"),
        Err(e) => {
            println!("[失败] 保存出错：{e}");
            return 1;
        }
    }

    println!("[3/3] OCR 识别（本地 ONNX 推理链）...");
    match ocr::recognize(&img.data, img.width, img.height) {
        Ok(text) => {
            println!("[OK] 识别完成，共 {} 字符：", text.chars().count());
            println!("────");
            println!("{text}");
            println!("────");
            0
        }
        Err(e) => {
            println!("[失败] OCR 出错：{e}");
            1
        }
    }
}
