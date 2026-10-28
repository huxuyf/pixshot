//! theme.rs —— Windows 11 Fluent 视觉基建（零新增第三方依赖）。
//!
//! 提供：
//! - 深/浅色板（跟随系统 `AppsUseLightTheme`，含用户强调色读取与亮度守卫）
//! - DPI 感知字体工厂（CJK 覆盖优先探测，回退 Segoe UI 系列）
//! - DWM 现代窗口特效（8px 圆角、1px 半透明描边、深色标题栏、Mica 背景），
//!   全部探测式降级：系统不支持时自动回退为实色表面，调用方零处理成本。
//!
//! 主题实时跟随：窗口过程收到 WM_SETTINGCHANGE 时调用 [`reload`] 即可。
//!
//! 线程模型：主题与字体族只在 GUI 线程访问，缓存用 `thread_local` 承载
//! （替代原先的 `static mut`，无数据竞争、无需 `allow(static_mut_refs)`）。

use std::cell::RefCell;

use crate::util::to_wide;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMSBT_TRANSIENTWINDOW,
    DWMWA_NCRENDERING_POLICY, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, GetDC, GetTextFaceW, ReleaseDC,
    SelectObject, HFONT,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;

/// Mica 一键回退开关已移至配置（config.ini [ui] mica）；此处保留说明。
/// 置 0 时所有窗口使用实色表面（色值即 Mica 同色）。

// ─────────────────────────────────────────────────────────────────
// 色板（Win11 Fluent tokens；浅色文本不用纯黑，兼容 Mica 黑色显影区域）
// COLORREF = 0x00BBGGRR
// ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub dark: bool,
    /// 窗口基底（Mica 不可用时的实色回退）
    pub bg: COLORREF,
    /// 内容卡片表面
    pub card: COLORREF,
    /// 卡片 1px 描边
    pub stroke: COLORREF,
    /// 主文本
    pub text: COLORREF,
    /// 次级文本
    pub text2: COLORREF,
    /// 强调色
    pub accent: COLORREF,
    /// 强调色表面上的文本
    pub accent_text: COLORREF,
    /// 微弱按钮悬停叠加色（与 card 混合）
    pub hover: COLORREF,
    /// 按下叠加色
    pub press: COLORREF,
    /// 标题栏关闭按钮悬停红
    pub close_hover: COLORREF,
}

pub(crate) const DARK: Theme = Theme {
    dark: true,
    bg: COLORREF(0x0020_2020),
    card: COLORREF(0x002B_2B2B),
    stroke: COLORREF(0x0035_3535),
    text: COLORREF(0x00FF_FFFF),
    text2: COLORREF(0x00C8_C8C8),
    accent: COLORREF(0x00FF_C24C), // #4CC2FF
    accent_text: COLORREF(0x0010_1010),
    hover: COLORREF(0x0038_3838), // 菜单悬停灰底（须明显区别于 card 0x2B）
    press: COLORREF(0x003A_3A3A),
    close_hover: COLORREF(0x001C_2BC4), // #C42B1C
};

pub(crate) const LIGHT: Theme = Theme {
    dark: false,
    bg: COLORREF(0x00F3_F3F3),
    card: COLORREF(0x00FF_FFFF),
    stroke: COLORREF(0x00E5_E5E5),
    text: COLORREF(0x001A_1A1A),
    text2: COLORREF(0x0061_6161),
    accent: COLORREF(0x00B8_5F00), // #005FB8
    accent_text: COLORREF(0x00FF_FFFF),
    hover: COLORREF(0x00EA_EAEA),
    press: COLORREF(0x00E0_E0E0),
    close_hover: COLORREF(0x001C_2BC4),
};

thread_local! {
    /// 当前主题缓存（None = 尚未读取过系统设置）
    static CACHE: RefCell<Option<Theme>> = const { RefCell::new(None) };
    /// UI 字体族（NUL 结尾 UTF-16，探测一次后缓存）
    static FAMILY: RefCell<Option<Vec<u16>>> = const { RefCell::new(None) };
}

/// 重新读取系统主题（WM_SETTINGCHANGE 时调用实现实时跟随）。
///
/// 当前无调用方（跟随系统的 OCR 结果窗口已移除；工具条为固定配色无需跟随），
/// 保留以便后续窗口接入实时主题。
#[allow(dead_code)]
pub fn reload() {
    let t = load_theme();
    CACHE.with(|c| *c.borrow_mut() = Some(t));
}

/// 当前主题（首次调用时自动加载）。
pub fn current() -> Theme {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(load_theme());
        }
        c.unwrap_or(DARK)
    })
}

/// 截图工具栏皮肤：**固定浅色**（纯白底 + 纯黑图标），不随系统深色主题变化。
/// 理由：工具条按钮为深色线性图标，白底黑图标对比度最高、观感最干净；
/// 固定配色可保证任何系统主题下外观一致。窗口自带 1px 浅灰描边与圆角，
/// 白底不会与桌面内容糊在一起。
pub fn toolbar_skin() -> Theme {
    Theme {
        dark: false,
        bg: COLORREF(0x00FF_FFFF),     // 纯白底
        card: COLORREF(0x00FF_FFFF),   // 整个窗口即卡片（不再内缩分区）
        stroke: COLORREF(0x00E2_E2E2), // 浅灰描边
        text: COLORREF(0x0000_0000),   // 纯黑图标
        text2: COLORREF(0x0055_5555),
        accent: LIGHT.accent,
        accent_text: LIGHT.accent_text,
        hover: COLORREF(0x00EC_ECEC),  // 悬停淡灰
        press: COLORREF(0x00DC_DCDC),
        close_hover: LIGHT.close_hover,
    }
}

fn load_theme() -> Theme {
    let light = read_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    )
    .map(|v| v != 0)
    .unwrap_or(true);
    if light {
        with_user_accent(LIGHT)
    } else {
        with_user_accent(DARK)
    }
}

fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    unsafe {
        let sk = to_wide(subkey);
        let vn = to_wide(value);
        let mut v: u32 = 0;
        let mut sz: u32 = 4;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(sk.as_ptr()),
            PCWSTR::from_raw(vn.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut core::ffi::c_void),
            Some(&mut sz),
        );
        r.is_ok().then_some(v)
    }
}

/// 用户强调色是否可用于当前底色（过暗在深色主题不可见，过亮在浅色主题上白字不清晰）。
fn accent_usable(dark: bool, lum: f32) -> bool {
    if dark {
        (0.35..=0.95).contains(&lum)
    } else {
        lum <= 0.70
    }
}

/// 读取用户强调色（HKCU\DWM\ColorizationColor，0xAABBGGRR），带亮度守卫。
fn with_user_accent(mut t: Theme) -> Theme {
    if let Some(abgr) = read_dword(r"Software\Microsoft\Windows\DWM", "ColorizationColor") {
        let lum = luminance(abgr);
        if accent_usable(t.dark, lum) {
            t.accent = COLORREF(abgr & 0x00FF_FFFF);
            // 强调色表面上的文本：亮强调色配深字，暗强调色配白字
            t.accent_text = if lum > 0.55 {
                COLORREF(0x0010_1010)
            } else {
                COLORREF(0x00FF_FFFF)
            };
        }
    }
    t
}

/// 相对亮度（0..1）。入参为 `0xAABBGGRR`。
fn luminance(bbrr: u32) -> f32 {
    let r = (bbrr & 0xFF) as f32;
    let g = ((bbrr >> 8) & 0xFF) as f32;
    let b = ((bbrr >> 16) & 0xFF) as f32;
    (0.299 * r + 0.587 * g + 0.114 * b) / 255.0
}

// ─────────────────────────────────────────────────────────────────
// 字体（DPI 感知；CJK 覆盖优先探测 → Segoe UI 回退）
// ─────────────────────────────────────────────────────────────────

fn family() -> Vec<u16> {
    FAMILY.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_none() {
            // 优先级说明：本程序界面文本为中文，而 Segoe UI / Segoe UI Variable 不含 CJK 字形
            // （只能依赖 GDI 字体链接，在不同系统配置下并不可靠，表现为汉字乱码或方框）。
            // 因此优先选同时覆盖中英文、且为 Windows 中文版原生 UI 字体的
            // 「Microsoft YaHei UI」；英文系统上无该字体时自动回退到 Segoe UI 系列。
            let name = if probe_font("Microsoft YaHei UI") {
                "Microsoft YaHei UI"
            } else if probe_font("Microsoft YaHei") {
                "Microsoft YaHei"
            } else if probe_font("Segoe UI Variable Text") {
                "Segoe UI Variable Text"
            } else if probe_font("Segoe UI") {
                "Segoe UI"
            } else {
                "SimSun"
            };
            *f = Some(to_wide(name));
            crate::diag::log(&format!(
                "[theme] UI 字体族 = {name}（优先 CJK 覆盖以正确显示中文）"
            ));
        }
        f.as_ref().cloned().unwrap_or_default()
    })
}

/// GDI 字体映射探测：把请求字体选入内存 DC，用 GetTextFaceW 比对实际落选族名。
fn probe_font(name: &str) -> bool {
    unsafe {
        let screen = GetDC(HWND::default());
        if screen.is_invalid() {
            return false;
        }
        let mem = CreateCompatibleDC(screen);
        let _ = ReleaseDC(HWND::default(), screen);
        if mem.is_invalid() {
            return false;
        }
        let nm = to_wide(name);
        let font = CreateFontW(
            -16,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            PCWSTR::from_raw(nm.as_ptr()),
        );
        if font.is_invalid() {
            let _ = DeleteDC(mem);
            return false;
        }
        let old = SelectObject(mem, font);
        let mut buf = [0u16; 64];
        let n = GetTextFaceW(mem, Some(&mut buf));
        let _ = SelectObject(mem, old);
        let _ = DeleteObject(font);
        let _ = DeleteDC(mem);
        if n <= 0 {
            return false;
        }
        // 注意：GetTextFaceW 返回的字符数**包含结尾 NUL**，必须剔除后再比较，
        // 否则比较恒为假 —— 这会让字体探测永远失败并落到最后一个回退字体。
        let mut len = (n as usize).min(buf.len());
        while len > 0 && buf[len - 1] == 0 {
            len -= 1;
        }
        String::from_utf16_lossy(&buf[..len]).to_lowercase() == name.to_lowercase()
    }
}

/// 按窗口 DPI 创建 UI 字体（pt → 像素，负高度映射字符高）。调用方负责 DeleteObject。
pub fn ui_font(hwnd: HWND, pt: i32, weight: i32) -> HFONT {
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96) as i32;
        let h = -(pt * dpi / 72);
        let fam = family();
        CreateFontW(
            h,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            PCWSTR::from_raw(fam.as_ptr()),
        )
    }
}

// ─────────────────────────────────────────────────────────────────
// DWM 现代窗口特效（探测式，失败自动降级）
// ─────────────────────────────────────────────────────────────────

/// 为无边框/常规窗口应用现代框架：深色标题栏感知 + 8px 圆角，
/// 并按需启用 Mica（`DWMSBT_TRANSIENTWINDOW` + 全窗玻璃延展，黑色像素透出材质）。
/// 返回 backdrop 是否启用 —— false 时调用方应使用实色 `theme.bg` 绘制基底。
///
/// 注：不设置 DWMWA_BORDER_COLOR —— 实测对 WS_THICKFRAME + WM_NCCALCSIZE(0) 的
/// 无边框窗口，该属性会令 DWM 在窗口外缘合成 ~8px 可视框架带（贴图内容被内缩）。
/// 1px 描边由各窗口在 WM_PAINT 中自绘。
pub fn apply_frame(hwnd: HWND, t: Theme, mica: bool) -> bool {
    unsafe {
        let dark = t.dark as i32;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const core::ffi::c_void,
            4,
        );
        let pref = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const i32 as *const core::ffi::c_void,
            4,
        );
        if mica && crate::config::get().ui_mica {
            let bt = DWMSBT_TRANSIENTWINDOW.0;
            let a = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &bt as *const i32 as *const core::ffi::c_void,
                4,
            )
            .is_ok();
            let m = MARGINS {
                cxLeftWidth: -1,
                cxRightWidth: -1,
                cyTopHeight: -1,
                cyBottomHeight: -1,
            };
            let b = DwmExtendFrameIntoClientArea(hwnd, &m).is_ok();
            a && b
        } else {
            false
        }
    }
}

/// 贴图窗口专用：保持直角（贴图是图像内容，圆角会裁掉像素），并禁用
/// DWM 非客户区渲染——否则当贴图失去前台（用户点击其他窗口）后，DWM 会在
/// WM_NCHITTEST 返回 HT* 的外缘 8px 调整大小带上绘制标准框架视觉带，
/// 将贴图内容视觉内缩 8px（实测 Win11 26300）。
pub fn frame_pin(hwnd: HWND) {
    unsafe {
        let nrp = DWMNCRP_DISABLED.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &nrp as *const i32 as *const core::ffi::c_void,
            4,
        );
        let pref = DWMWCP_DONOTROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const i32 as *const core::ffi::c_void,
            4,
        );
    }
}

/// 自绘 1px 窗口轮廓色（半透明描边观感：基色向文本色 10% 混合）。
pub fn edge_line(t: Theme) -> COLORREF {
    lerp(t.bg, t.text, 0.10)
}

/// 颜色插值（悬停动画用）：c = a + (b-a)*t
pub fn lerp(a: COLORREF, b: COLORREF, t: f32) -> COLORREF {
    let (ar, ag, ab) = (a.0 & 0xFF, (a.0 >> 8) & 0xFF, (a.0 >> 16) & 0xFF);
    let (br, bg, bb) = (b.0 & 0xFF, (b.0 >> 8) & 0xFF, (b.0 >> 16) & 0xFF);
    let t = t.clamp(0.0, 1.0);
    COLORREF(
        (ar as f32 + (br as f32 - ar as f32) * t) as u32
            | (((ag as f32 + (bg as f32 - ag as f32) * t) as u32) << 8)
            | (((ab as f32 + (bb as f32 - ab as f32) * t) as u32) << 16),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance_covers_extremes() {
        assert!((luminance(0x00FF_FFFF) - 1.0).abs() < 0.01); // 白
        assert!(luminance(0x0000_0000) < 0.01); // 黑
                                                // 纯红亮度 0.299（COLORREF 分量顺序为 BGR，0x000000FF = R=255）
        assert!((luminance(0x0000_00FF) - 0.299).abs() < 0.01);
    }

    #[test]
    fn accent_brightness_guard() {
        // 深色主题：过暗的强调色不可见
        assert!(!accent_usable(true, 0.10));
        assert!(accent_usable(true, 0.60));
        assert!(!accent_usable(true, 0.99));
        // 浅色主题：过亮的强调色让白字读不清（只需不过亮）
        assert!(accent_usable(false, 0.20));
        assert!(!accent_usable(false, 0.85));
    }

    #[test]
    fn lerp_endpoints_and_midpoint() {
        let a = COLORREF(0x0000_0000);
        let b = COLORREF(0x00FF_FFFF);
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        // 中点：各通道 ≈ 127
        let mid = lerp(a, b, 0.5).0;
        assert_eq!(mid & 0xFF, 0x7F);
        assert_eq!((mid >> 8) & 0xFF, 0x7F);
        assert_eq!((mid >> 16) & 0xFF, 0x7F);
    }

    #[test]
    fn lerp_clamps_out_of_range() {
        let a = COLORREF(0x0010_1010);
        let b = COLORREF(0x0020_2020);
        // 越界 t 不得溢出通道（曾经是 u32 相加后回绕的隐患点）
        assert_eq!(lerp(a, b, -1.0), a);
        assert_eq!(lerp(a, b, 2.0), b);
    }

    #[test]
    fn edge_line_is_subtle() {
        let e = edge_line(DARK);
        // 描边必须比正文暗、比底色亮
        assert!(e.0 > DARK.bg.0 && e.0 < DARK.text.0);
    }
}
