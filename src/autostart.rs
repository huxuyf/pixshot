//! 开机自启：维护当前用户的 Run 注册表项。
//!
//! 位置：`HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`
//! 值名：`PixShot`，值数据：带引号的 `pixshot.exe` 绝对路径（`REG_SZ`，UTF-16）。
//!
//! 以注册表为唯一事实来源：托盘菜单的勾选状态直接查询注册表，
//! 避免「配置说已启用、系统实际未生效」的状态漂移。
//! HKCU 写入不需要管理员权限，也不需要额外依赖（复用 `Win32_System_Registry`）。

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPEN_CREATE_OPTIONS, REG_SZ,
};

use crate::diag;
use crate::util;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "PixShot";
/// ERROR_SUCCESS
const ERROR_SUCCESS: u32 = 0;
/// ERROR_FILE_NOT_FOUND：删除一个本就不存在的值时视为「已关闭」。
const ERROR_FILE_NOT_FOUND: u32 = 2;

/// 开机自启当前是否已启用（注册表项存在即视为启用）。
pub fn is_enabled() -> bool {
    unsafe {
        let sub = util::to_wide(RUN_KEY);
        let mut hkey = HKEY::default();
        let rc = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if rc.0 != ERROR_SUCCESS {
            return false;
        }
        let name = util::to_wide(VALUE_NAME);
        let mut len = 0u32;
        // 只查询类型/长度即可判定存在性
        let rc = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut len),
        );
        let _ = RegCloseKey(hkey);
        rc.0 == ERROR_SUCCESS
    }
}

/// 启用 / 取消开机自启。启用时写入指向自身 exe 的 Run 值，取消时删除该值。
pub fn set_enabled(on: bool) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法获取程序路径：{e}"))?;
    unsafe {
        let sub = util::to_wide(RUN_KEY);
        let mut hkey = HKEY::default();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPEN_CREATE_OPTIONS(0), // REG_OPTION_NON_VOLATILE
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        );
        if rc.0 != ERROR_SUCCESS {
            return Err(format!("打开注册表项失败（Win32 错误 {}）", rc.0));
        }

        let name = util::to_wide(VALUE_NAME);
        let rc = if on {
            // 路径可能含空格，必须加引号，否则系统会把它当成「可执行文件 + 参数」
            let mut data: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().collect();
            data.push(0); // REG_SZ 需含结尾 NUL
            let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            RegSetValueExW(hkey, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes))
        } else {
            RegDeleteValueW(hkey, PCWSTR(name.as_ptr()))
        };
        let _ = RegCloseKey(hkey);

        match rc.0 {
            ERROR_SUCCESS => {
                diag::log(&format!(
                    "[autostart] 已{} 开机自启 (exe={})",
                    if on { "启用" } else { "取消" },
                    exe.display()
                ));
                Ok(())
            }
            // 取消一个本就不存在的值：结果状态正确，不算失败
            ERROR_FILE_NOT_FOUND if !on => Ok(()),
            e => {
                diag::log(&format!("[autostart] 设置失败 (on={on}) Win32 错误 {e}"));
                Err(format!("写入注册表失败（Win32 错误 {e}）"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实读写 HKCU Run 往返，并在结束时恢复原状态（不污染用户环境）。
    #[test]
    fn enable_disable_roundtrip() {
        let prior = is_enabled();
        set_enabled(true).expect("启用开机自启失败");
        assert!(is_enabled(), "写入后应处于启用状态");
        set_enabled(false).expect("取消开机自启失败");
        assert!(!is_enabled(), "删除后应处于关闭状态");
        // 恢复测试前的状态
        set_enabled(prior).expect("恢复原状态失败");
        assert_eq!(is_enabled(), prior);
    }
}

