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
    CreateWindowExW, DestroyWindow, SendMessageW, WINDOW_EX_STYLE, WM_GETTEXTLENGTH, WS_POPUP,
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

    // ── 量测所需高度 ──────────────────────────────────────────────
    // `FORMATRANGE` 的 `rc`/`rcPage` 单位是 **twips**（1/1440 英寸），不是像素；
    // `EM_FORMATRANGE` 的返回值是「最后一个排入区域的字符索引 + 1」，不是高度。
    // （两者都曾按像素/高度误用：排版宽度被压成 1/15、表面高度变成字符数。）
    // 量测策略：高度倍增直到整篇排下（封顶防御巨型/畸形文档），渲染用同一尺寸。
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() }.max(1) as i32;
    let to_twips = |px: i32| (px as i64 * 1440 / dpi as i64) as i32;
    let total = SendMessageW(hwnd, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0)).0 as i64;
    let mut fr = FORMATRANGE {
        hdc: HDC::default(), // 量测：NULL = 按屏幕设备排版
        hdcTarget: HDC::default(),
        rc: RECT::default(),
        rcPage: RECT::default(),
        chrg: CHARRANGE { cpMin: 0, cpMax: -1 },
    };
    let mut h_used = 200i32;
    loop {
        let page = RECT { left: 0, top: 0, right: to_twips(render_width), bottom: to_twips(h_used) };
        fr.rc = page;
        fr.rcPage = page;
        let fitted = SendMessageW(
            hwnd,
            EM_FORMATRANGE,
            WPARAM(0),
            LPARAM(&mut fr as *mut FORMATRANGE as isize),
        )
        .0 as i64;
        if fitted >= total || h_used >= 20_000 {
            break;
        }
        h_used *= 2;
    }

    // 渲染进 DIB 表面（量测与渲染用同一宽度/twips 换算；hdcTarget 一致，排版不换行漂移）
    let surf = DibSurface::new(render_width, h_used)
        .ok_or_else(|| AppError::Config("RTF 目标表面创建失败".into()))?;
    // 白底打底（BGRA 全 0xFF）
    std::ptr::write_bytes(
        surf.bits() as *mut u8,
        0xFF,
        (render_width as usize) * (h_used as usize) * 4,
    );
    let page = RECT { left: 0, top: 0, right: to_twips(render_width), bottom: to_twips(h_used) };
    fr.hdc = surf.dc;
    fr.hdcTarget = surf.dc;
    fr.rc = page;
    fr.rcPage = page;
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
/// 返回 `(w, h, bgra)`（alpha 已置满）。`margin` 负值按 0 处理（防御配置污染）。
fn crop_tight(raw: &[u8], w: i32, h: i32, margin: i32) -> (i32, i32, Vec<u8>) {
    let margin = margin.max(0);
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
/// `<script>`/`<style>` 的内容不可见，整段跳过；支持 `&#NN;` 数字实体。
pub fn html_to_text(html: &str) -> String {
    // 跳到 <body> 之后（CF_HTML 头 + 文档结构）
    let start = html.find("<body").and_then(|i| html[i..].find('>').map(|j| i + j + 1));
    let mut body = start.map(|s| &html[s..]).unwrap_or(html);
    // 去掉 <script>/<style> 整段（其文本不是可见内容）
    let mut cleaned = String::with_capacity(body.len());
    loop {
        let lower = body.to_ascii_lowercase();
        // 取最靠前的一个（script/style 谁在前先剥谁）
        let (o, tag) = match (lower.find("<script"), lower.find("<style")) {
            (Some(a), Some(b)) => {
                if a <= b { (a, "</script") } else { (b, "</style") }
            }
            (Some(a), None) => (a, "</script"),
            (None, Some(b)) => (b, "</style"),
            (None, None) => break,
        };
        cleaned.push_str(&body[..o]);
        match lower[o..].find(tag) {
            Some(c) => {
                let after = &lower[o + c..];
                let end = after.find('>').map(|i| o + c + i + 1);
                body = end.map(|e| &body[e..]).unwrap_or("");
            }
            None => {
                body = "";
                break;
            }
        }
    }
    cleaned.push_str(body);
    let body = cleaned.as_str();

    let mut out = String::with_capacity(body.len());
    let mut in_tag = false;
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            '&' if !in_tag => {
                // 实体：&name; 或 &#NN;（到分号为止，超长/不闭合按原样）
                let mut ent = String::new();
                let mut closed = false;
                for c in chars.by_ref().take(12) {
                    if c == ';' {
                        closed = true;
                        break;
                    }
                    ent.push(c);
                }
                if !closed {
                    out.push('&');
                    out.push_str(&ent);
                    continue;
                }
                let decoded = match ent.as_str() {
                    "nbsp" => Some(' '),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "amp" => Some('&'),
                    "quot" => Some('"'),
                    "#39" => Some('\''),
                    _ if ent.starts_with('#') => ent[1..]
                        .parse::<u32>()
                        .ok()
                        .and_then(char::from_u32),
                    _ => None,
                };
                match decoded {
                    Some(c) => out.push(c),
                    None => {
                        out.push('&');
                        out.push_str(&ent);
                        out.push(';');
                    }
                }
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 按代码页把 ANSI 字节串解码为 Unicode（GBK/Big5/Shift-JIS 等双字节安全）。
/// 失败回退 Latin-1（逐字节映射，与历史行为一致）。
fn decode_ansi(bytes: &[u8], codepage: u32) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    if codepage == 65001 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    unsafe {
        use windows::Win32::Globalization::{MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
        let cp = if codepage == 0 { 0 } else { codepage }; // 0 = CP_ACP
        let n = MultiByteToWideChar(cp, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None);
        if n > 0 {
            let mut buf = vec![0u16; n as usize];
            if MultiByteToWideChar(cp, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut buf)) == n {
                return String::from_utf16_lossy(&buf);
            }
        }
        bytes.iter().map(|&b| b as char).collect()
    }
}

/// RTF → 粗略纯文本（RTF 渲染失败时的兜底）：跳过控件字/组，保留可见字符。
///
/// 编码处理：`\ansicpgNNNN` 设定 `\'hh` 字节的代码页（中文 RTF 为 936/GBK）；
/// 连续 `\'hh` 字节段**整体**按代码页解码（逐字节 Latin-1 会把 GBK 双字节
/// 拆成两个乱码字符）；`\uN` 按 Unicode 码点输出并跳过 `\ucN` 个回退字符。
pub fn rtf_to_text(rtf: &[u8]) -> String {
    let s = String::from_utf8_lossy(rtf);
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut skip_group_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut codepage: u32 = 1252;
    let mut uc_skip: usize = 1;
    // 连续 \'hh 字节段（遇到非 \'hh 内容时整体解码）
    let mut pending: Vec<u8> = Vec::new();
    macro_rules! flush_pending {
        () => {
            if !pending.is_empty() {
                if skip_group_depth.is_none() {
                    out.push_str(&decode_ansi(&pending, codepage));
                }
                pending.clear();
            }
        };
    }
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                flush_pending!();
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
                flush_pending!();
                if let Some(d) = skip_group_depth {
                    if depth <= d {
                        skip_group_depth = None;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            '\\' => {
                // \'hh（十六进制字节）：先攒入字节段
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    let h1 = chars.next().unwrap_or('0');
                    let h2 = chars.next().unwrap_or('0');
                    if let Ok(b) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                        pending.push(b);
                    }
                    continue;
                }
                flush_pending!();
                // 控件字：\word + 可选有符号数字参数
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
                    // 转义符号 \{ \} \\
                    if let Some(c) = chars.next() {
                        if skip_group_depth.is_none() && (c == '{' || c == '}' || c == '\\') {
                            out.push(c);
                        }
                    }
                    continue;
                }
                let mut num = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '-' || c.is_ascii_digit() {
                        num.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                // 跳过一个分隔空格
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
                // 代码页/回退设置即使在组内也生效（头部声明不在 {\*...} 组里）
                match word.as_str() {
                    "ansicpg" => codepage = num.parse().unwrap_or(1252),
                    "uc" => uc_skip = num.parse::<usize>().unwrap_or(1),
                    _ => {}
                }
                if skip_group_depth.is_some() {
                    continue;
                }
                match word.as_str() {
                    "par" | "line" => out.push('\n'),
                    "tab" => out.push('\t'),
                    "u" => {
                        if let Ok(mut v) = num.parse::<i32>() {
                            if v < 0 {
                                v += 65536;
                            }
                            if let Some(ch) = char::from_u32(v as u32) {
                                out.push(ch);
                            }
                        }
                        // 跳过 \uc 个回退字符（每个是 \'hh 或一个普通字符）
                        for _ in 0..uc_skip {
                            if chars.peek() == Some(&'\\') {
                                let mut look = chars.clone();
                                look.next();
                                if look.next() == Some('\'') {
                                    chars.next(); // '\'
                                    chars.next(); // '\''
                                    chars.next(); // h1
                                    chars.next(); // h2
                                    continue;
                                }
                            }
                            chars.next();
                        }
                    }
                    _ => {}
                }
            }
            '\r' | '\n' => {}
            _ => {
                flush_pending!();
                if skip_group_depth.is_none() {
                    out.push(ch);
                }
            }
        }
    }
    flush_pending!();
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
    fn rtf_to_text_decodes_gbk_hex_and_unicode() {
        // GBK（ansicpg936）：\'c4\'e3\'ba\'c3 = 「你好」；\u-722atur? 之类 \uN 也要还原
        let rtf = "{\\rtf1\\ansi\\ansicpg936 \\'c4\\'e3\\'ba\\'c3}";
        assert_eq!(rtf_to_text(rtf.as_bytes()), "你好");
        // \uN 控件字（含负数形式与 \uc1 回退字节）
        let rtf = "{\\rtf1\\ansi\\uc1\\u20320\\'ba\\u22909\\x}";
        let t = rtf_to_text(rtf.as_bytes());
        assert!(t.contains('你'), "\\u20320 应为「你」，得到 {t:?}");
        assert!(t.contains('好'), "\\u22909 应为「好」，得到 {t:?}");
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

    #[test]
    fn html_skips_script_style_and_numeric_entities() {
        let html = "<html><body><style>.a{color:red}</style>ok<script>var x=1;</script>&#65;&#20013;</body></html>";
        let text = html_to_text(html);
        assert!(text.contains("ok"));
        assert!(text.contains("A中"), "数字实体应还原: {text:?}");
        assert!(!text.contains("color"), "style 内容不可见");
        assert!(!text.contains("var"), "script 内容不可见");
    }
}
