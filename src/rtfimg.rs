//! RTF → 位图：用系统封装字体渲染器 `msftedit.dll`（RICHEDIT50W）离屏排版，
//! 经 `EM_FORMATRANGE` 渲染进 DIB 后读回 BGRA。用于 F3 贴图的富文本分支。
//!
//! 只在剪贴板**没有图像格式**、且带 "Rich Text Format" 时才走这里（见
//! `clipboard::classify` 的优先级）。`msftedit.dll` 为系统自带（Win7+），
//! 加载失败时调用方回退为纯文本渲染，保证「有图可贴」。
//!
//! 注：`FORMATRANGE`/`EDITSTREAM` 在 x64 上是 `#[repr(C, packed(4))]`（RichEdit
//! 头文件按 4 字节打包），故直接复用 windows crate 的定义，切勿自行声明为
//! `#[repr(C)]`——布局不一致会让 RichEdit 越界写栈，表现为进程崩溃。

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::UI::Controls::RichEdit::{
    CHARRANGE, EDITSTREAM, EM_FORMATRANGE, EM_SETBKGNDCOLOR, EM_STREAMIN, FORMATRANGE, SF_RTF,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WS_POPUP,
};

use crate::annot::session::DibSurface;
use crate::error::{AppError, Result};

/// 离屏排版的内容宽度与结果留白由配置提供（config.ini [rtfimg]）。
/// 内容超出宽度自动换行，随后按内容裁边。

/// `msftedit.dll` 模块句柄（进程级懒加载，0 = 尚未成功加载）。
static MSFTEDIT: AtomicIsize = AtomicIsize::new(0);

/// 懒加载 `msftedit.dll`。返回其模块句柄（失败返回 0）。
fn ensure_msftedit() -> isize {
    let cached = MSFTEDIT.load(Ordering::SeqCst);
    if cached != 0 {
        return cached;
    }
    unsafe {
        let name = crate::util::to_wide("msftedit.dll");
        match LoadLibraryW(PCWSTR::from_raw(name.as_ptr())) {
            Ok(h) => {
                let v = h.0 as isize;
                MSFTEDIT.store(v, Ordering::SeqCst);
                v
            }
            Err(e) => {
                crate::diag::log(&format!("[rtf] 加载 msftedit.dll 失败: {e}"));
                0
            }
        }
    }
}

/// RTF 字节流读取状态（作为 `EDITSTREAM.dwCookie` 传指针）。
struct StreamState {
    data: Vec<u8>,
    pos: usize,
}

unsafe extern "system" fn stream_cb(cookie: usize, buf: *mut u8, cb: i32, pcb: *mut i32) -> u32 {
    if cookie == 0 || buf.is_null() || pcb.is_null() {
        return 0;
    }
    let st = &mut *(cookie as *mut StreamState);
    let remaining = st.data.len().saturating_sub(st.pos);
    let n = (cb.max(0) as usize).min(remaining);
    if n > 0 {
        std::ptr::copy_nonoverlapping(st.data.as_ptr().add(st.pos), buf, n);
        st.pos += n;
    }
    *pcb = n as i32;
    0
}

/// 把 RTF 渲染为 BGRA 自上而下缓冲，返回 `(w, h, bgra)`。
pub fn render_rtf_to_bgra(rtf: &[u8]) -> Result<(i32, i32, Vec<u8>)> {
    if rtf.is_empty() {
        return Err(AppError::Config("RTF 数据为空".into()));
    }
    let hmod = ensure_msftedit();
    if hmod == 0 {
        return Err(AppError::Config("msftedit.dll 不可用".into()));
    }
    unsafe { render_impl(hmod, rtf) }
}

unsafe fn render_impl(hmod: isize, rtf: &[u8]) -> Result<(i32, i32, Vec<u8>)> {
    let class = crate::util::to_wide("RICHEDIT50W");
    // 离屏窗口：不可见、无边框，尺寸给足（宽度固定，高度先给大值）
    let hwnd: HWND = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR::from_raw(class.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        0,
        0,
        crate::config::get().rtfimg_render_width,
        2000,
        None,
        None,
        HINSTANCE(hmod as *mut c_void),
        None,
    )
    .unwrap_or_default();
    if hwnd.is_invalid() {
        return Err(AppError::Config("创建 RICHEDIT50W 窗口失败".into()));
    }
    let result = render_with_window(hwnd, rtf);
    let _ = DestroyWindow(hwnd);
    result
}

unsafe fn render_with_window(hwnd: HWND, rtf: &[u8]) -> Result<(i32, i32, Vec<u8>)> {
    let cfg = crate::config::get();
    let (render_width, margin) = (cfg.rtfimg_render_width, cfg.rtfimg_render_margin);
    // 白底（避免透明/系统色），随后按内容裁边
    let _ = SendMessageW(hwnd, EM_SETBKGNDCOLOR, WPARAM(0), LPARAM(0x00FF_FFFF));

    // 载入 RTF
    let mut state = StreamState { data: rtf.to_vec(), pos: 0 };
    let mut es = EDITSTREAM {
        dwCookie: &mut state as *mut StreamState as usize,
        dwError: 0,
        pfnCallback: Some(stream_cb),
    };
    let loaded = SendMessageW(
        hwnd,
        EM_STREAMIN,
        WPARAM(SF_RTF as usize),
        LPARAM(&mut es as *mut EDITSTREAM as isize),
    )
    .0 as i32;
    if loaded <= 0 || es.dwError != 0 {
        return Err(AppError::Config(format!(
            "RTF 载入失败（loaded={loaded}, err={}）",
            es.dwError
        )));
    }

    // 量测所需高度（fRendering=FALSE，hdc=NULL）
    let page = RECT { left: 0, top: 0, right: render_width, bottom: 1_000_000 };
    let mut fr = FORMATRANGE {
        hdc: HDC::default(),
        hdcTarget: HDC::default(),
        rc: page,
        rcPage: page,
        chrg: CHARRANGE { cpMin: 0, cpMax: -1 },
    };
    let h_used = SendMessageW(
        hwnd,
        EM_FORMATRANGE,
        WPARAM(0),
        LPARAM(&mut fr as *mut FORMATRANGE as isize),
    )
    .0 as i32;
    let h_used = if h_used <= 0 { 200 } else { h_used.min(20_000) };

    // 渲染进 DIB 表面
    let surf = DibSurface::new(render_width, h_used)
        .ok_or_else(|| AppError::Config("RTF 目标表面创建失败".into()))?;
    // 白底打底（BGRA 全 0xFF）
    std::ptr::write_bytes(
        surf.bits() as *mut u8,
        0xFF,
        (render_width as usize) * (h_used as usize) * 4,
    );
    fr.hdc = surf.dc;
    fr.hdcTarget = surf.dc;
    fr.rc = RECT { left: 0, top: 0, right: render_width, bottom: h_used };
    fr.rcPage = fr.rc;
    fr.chrg = CHARRANGE { cpMin: 0, cpMax: -1 };
    let _ = SendMessageW(
        hwnd,
        EM_FORMATRANGE,
        WPARAM(1),
        LPARAM(&mut fr as *mut FORMATRANGE as isize),
    );
    // 释放 RichEdit 缓存的排版信息
    let _ = SendMessageW(hwnd, EM_FORMATRANGE, WPARAM(0), LPARAM(0));

    let raw = surf.read_rows();
    let (w, h, out) = crop_tight(&raw, render_width, h_used, margin);
    drop(surf);
    Ok((w, h, out))
}

/// 以左上角像素为底色，裁掉右侧/底部空白，并在四周补 `margin` 留白。
/// 返回 `(w, h, bgra)`（alpha 已置满）。
fn crop_tight(raw: &[u8], w: i32, h: i32, margin: i32) -> (i32, i32, Vec<u8>) {
    let (wu, hu) = (w as usize, h as usize);
    let bg = [raw[0], raw[1], raw[2]];
    let diff = |i: usize| -> bool {
        let o = i * 4;
        raw[o] != bg[0] || raw[o + 1] != bg[1] || raw[o + 2] != bg[2]
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (wu, hu, 0usize, 0usize);
    for y in 0..hu {
        for x in 0..wu {
            if diff(y * wu + x) {
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
            }
        }
    }
    if min_x > max_x || min_y > max_y {
        // 全为底色（空内容）：返回原图
        let mut out = raw.to_vec();
        for px in out.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        return (w, h, out);
    }
    let cw = (max_x - min_x + 1) as i32;
    let ch = (max_y - min_y + 1) as i32;
    let nw = cw + margin * 2;
    let nh = ch + margin * 2;
    let mut out = vec![0u8; (nw as usize) * (nh as usize) * 4];
    for px in out.chunks_exact_mut(4) {
        px[0] = bg[0];
        px[1] = bg[1];
        px[2] = bg[2];
        px[3] = 0xFF;
    }
    for y in 0..ch as usize {
        let sy = min_y + y;
        let dy = (margin as usize) + y;
        for x in 0..cw as usize {
            let sx = min_x + x;
            let dx = (margin as usize) + x;
            let so = (sy * wu + sx) * 4;
            let do_ = (dy * (nw as usize) + dx) * 4;
            out[do_] = raw[so];
            out[do_ + 1] = raw[so + 1];
            out[do_ + 2] = raw[so + 2];
            out[do_ + 3] = 0xFF;
        }
    }
    (nw, nh, out)
}

/// 从 HTML 片段（CF_HTML）里提取可见文本（去标签、还原常见实体）。
/// 项目无 HTML 渲染器，富文本的 HTML 分支明确降级为纯文本出图。
pub fn html_to_text(html: &str) -> String {
    // 跳到 <body> 之后（CF_HTML 头 + 文档结构）
    let start = html.find("<body").and_then(|i| html[i..].find('>').map(|j| i + j + 1));
    let body = start.map(|s| &html[s..]).unwrap_or(html);
    let mut out = String::with_capacity(body.len());
    let mut in_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// RTF → 粗略纯文本（RTF 渲染失败时的兜底）：跳过控件字/组，保留可见字符。
pub fn rtf_to_text(rtf: &[u8]) -> String {
    let s = String::from_utf8_lossy(rtf);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut skip_group_depth: Option<usize> = None;
    let mut depth = 0usize;
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                depth += 1;
                // {\* ...} 目标组：整组跳过
                if chars.peek() == Some(&'\\') {
                    let mut look = chars.clone();
                    look.next();
                    if look.peek() == Some(&'*') {
                        skip_group_depth = Some(depth);
                    }
                }
            }
            '}' => {
                if let Some(d) = skip_group_depth {
                    if depth <= d {
                        skip_group_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            '\\' => {
                // 控件字：\word 或 \'hh（十六进制字节）
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphabetic() {
                        word.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if word.is_empty() {
                    // \'hh 或转义符号
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                        let h1 = chars.next().unwrap_or('0');
                        let h2 = chars.next().unwrap_or('0');
                        if skip_group_depth.is_none() {
                            if let Ok(b) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                                out.push(b as char);
                            }
                        }
                    } else if let Some(c) = chars.next() {
                        if skip_group_depth.is_none() && (c == '{' || c == '}' || c == '\\') {
                            out.push(c);
                        }
                    }
                    continue;
                }
                // 数字参数
                while let Some(&c) = chars.peek() {
                    if c == '-' || c.is_ascii_digit() {
                        chars.next();
                    } else {
                        break;
                    }
                }
                // 跳过一个分隔空格
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                if skip_group_depth.is_none() {
                    match word.as_str() {
                        "par" | "line" => out.push('\n'),
                        "tab" => out.push('\t'),
                        _ => {}
                    }
                }
            }
            '\r' | '\n' => {}
            _ => {
                if skip_group_depth.is_none() {
                    out.push(ch);
                }
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 需要 msftedit.dll + GDI，显式运行：简单 RTF 应渲染出非空图像。
    #[test]
    #[ignore = "需要 msftedit.dll，供显式运行验证"]
    fn renders_simple_rtf() {
        let rtf = "{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0\\fnil Microsoft YaHei UI;}}\\f0\\fs24 Hello \\b World\\b0\\par \u{4f60}\u{597d}\u{4e16}\u{754c}";
        let (w, h, buf) = render_rtf_to_bgra(rtf.as_bytes()).expect("RTF 渲染应成功");
        assert!(w > 0 && h > 0, "尺寸应为正：{w}x{h}");
        assert_eq!(buf.len(), (w * h * 4) as usize);
    }

    #[test]
    fn rtf_to_text_keeps_visible_words() {
        let rtf = r"{\rtf1\ansi\deff0 {\fonttbl {\f0 Arial;}}\f0\fs20 Hello \b World\b0\par Second}";
        let t = rtf_to_text(rtf.as_bytes());
        assert!(t.contains("Hello"));
        assert!(t.contains("World"));
        assert!(t.contains("Second"));
        assert!(!t.contains("fonttbl"));
        assert!(!t.contains('\\'));
    }

    #[test]
    fn html_strips_tags_and_entities() {
        let html = "Version:0.9\r\nStartHTML:000000\n<html><body><b>Hello</b>&nbsp;<i>世界</i><br>x</body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("世界"));
        assert!(!text.contains('<'));
        assert!(!text.contains("&nbsp;"));
    }
}
