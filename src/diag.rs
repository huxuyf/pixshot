//! 轻量诊断日志：把易失败的系统集成调用（托盘/热键/图标加载）的结果写入文件。
//!
//! 背景：本程序为 `windows_subsystem = "windows"`（无控制台），且 `Shell_NotifyIconW`
//! 返回 `BOOL` 而非 `Result` —— 调用失败极易被静默吞掉。此模块让失败可见。

use std::io::Write;
use std::path::PathBuf;

/// 诊断日志路径：exe 同目录（确定性位置，避免 TEMP 重定向带来的歧义）；
/// 不可写时回退系统临时目录。
pub fn log_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("pixshot_diag.log");
            if std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&p)
                .is_ok()
            {
                return p;
            }
        }
    }
    std::env::temp_dir().join("pixshot_diag.log")
}

/// 追加一行诊断（失败静默：诊断本身不得影响功能）。
pub fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "{msg}");
    }
}

/// 启动时清空旧日志并写入会话头。
pub fn begin_session() {
    let _ = std::fs::write(
        log_path(),
        format!("==== PixShot 诊断会话 pid={} ====\n", std::process::id()),
    );
}

// ─────────────────────────────────────────────────────────────────
// 进程完整性级别自检
//
// 若 exe 文件带有 Low 强制完整性标签（沙箱类工具写入工作区时会打标），从它启动的
// 进程会以「低完整性」运行；此时 UIPI 会阻止它向资源管理器（中完整性）的托盘窗口
// 发送消息 —— Shell_NotifyIcon 返回 Win32 错误 5（拒绝访问）。此失败与代码无关，
// 必须在诊断中明确指出，否则极易误判。
// ─────────────────────────────────────────────────────────────────

/// 本进程完整性级别的 RID（0x1000=Low，0x2000=Medium，0x3000=High…）。
pub fn integrity_rid() -> Option<u32> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        if ok.is_err() {
            return None;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, (count as u32) - 1))
    }
}

/// 完整性级别描述。
pub fn integrity_name(rid: u32) -> &'static str {
    match rid {
        0x0000 => "Untrusted（不可信）",
        0x1000 => "Low（低完整性）",
        0x2000 => "Medium（普通用户）",
        0x2100 => "MediumPlus",
        0x3000 => "High（管理员）",
        0x4000 => "System",
        _ => "未知",
    }
}

/// 是否处于低/不可信完整性 —— 该状态下托盘注册必然被系统拒绝（UIPI 限制）。
pub fn is_low_integrity() -> bool {
    matches!(integrity_rid(), Some(0x0000) | Some(0x1000))
}
