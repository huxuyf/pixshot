//! 文本 → 位图：把纯文本排版渲染成 BGRA（自上而下）缓冲，供 F3 贴图使用。
//!
//! 完全复用项目既有的 GDI+ 路径（[`crate::annot::render_gdiplus::graphics_from`] +
//! [`crate::annot::session::DibSurface`]），**不引入 ab_glyph/image/imageproc**。
//! 字体族 `Microsoft YaHei UI` 覆盖拉丁与 CJK（GDI+ 不做字体链接，Segoe UI 遇中文
//! 会出豆腐块，与标注引擎保持一致）。

use std::ptr::null_mut;

use windows::core::PCWSTR;
use windows::Win32::Graphics::GdiPlus::{
    GpFont, GpFontFamily, GpGraphics, GpStringFormat, RectF, UnitPixel, GdipCreateFont,
    GdipCreateFontFamilyFromName, GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush,
    GdipDeleteFont, GdipDeleteFontFamily, GdipDeleteStringFormat, GdipDrawString, GdipMeasureString,
    GpBrush, GpSolidFill,
};

use crate::annot::render_gdiplus::{free_graphics, graphics_from};
use crate::annot::session::DibSurface;
use crate::annot::style::Rgba;
use crate::error::{AppError, Result};

/// 文本排版选项。
pub struct TextOptions {
    pub font_family: String,
    pub font_px: i32,
    pub margin: i32,
    pub max_width: i32,
    pub line_spacing: f32,
    pub fg: Rgba,
    pub bg: Rgba,
}

impl Default for TextOptions {
    fn default() -> Self {
        // 默认排版来自配置（config.ini [textimg]），保持"文本贴图外观可配置"。
        let c = crate::config::get();
        Self {
            font_family: c.textimg_font_family.clone(),
            font_px: c.textimg_font_px,
            margin: c.textimg_margin,
            max_width: c.textimg_max_width,
            line_spacing: c.textimg_line_spacing,
            fg: c.textimg_fg,
            bg: c.textimg_bg,
        }
    }
}

/// 把文本渲染成 BGRA 自上而下缓冲，返回 `(w, h, bgra)`。
pub fn render_text_to_bgra(text: &str, opts: &TextOptions) -> Result<(i32, i32, Vec<u8>)> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.trim().is_empty() {
        return Err(AppError::Config("没有可渲染的文本".into()));
    }
    unsafe { render_impl(&text, opts) }
}

unsafe fn render_impl(text: &str, opts: &TextOptions) -> Result<(i32, i32, Vec<u8>)> {
    if !crate::icons::gdiplus_ready() {
        return Err(AppError::Config("GDI+ 未就绪".into()));
    }
    let font_px = opts.font_px.max(1);
    let margin = opts.margin.max(0);
    let max_width = opts.max_width.max(font_px);

    // 1) 用一个临时小表面建立量测上下文
    let scratch = DibSurface::new(8, 8).ok_or_else(|| AppError::Config("DIB 表面创建失败".into()))?;
    let gm = graphics_from(scratch.dc).ok_or_else(|| AppError::Config("GDI+ 上下文创建失败".into()))?;
    let measure_font = make_font(&opts.font_family, font_px as f32);
    if measure_font.is_null() {
        free_graphics(gm);
        return Err(AppError::Config("字体创建失败".into()));
    }

    let lines = wrap_lines(gm, measure_font, text, max_width);
    let line_h = (font_px as f32 * opts.line_spacing).round().max(1.0) as i32;
    let text_w = lines
        .iter()
        .map(|l| measure(gm, measure_font, l).0.ceil().max(1.0) as i32)
        .max()
        .unwrap_or(1)
        .max(1);

    let _ = GdipDeleteFont(measure_font);
    free_graphics(gm);
    drop(scratch);

    let w = (text_w + margin * 2).max(1);
    let h = (lines.len() as i32 * line_h + margin * 2).max(1);

    // 2) 目标表面：填底色 → 逐行绘制 → 读回
    let surf = DibSurface::new(w, h).ok_or_else(|| AppError::Config("目标表面创建失败".into()))?;
    fill_bg(surf.bits(), w, h, opts.bg);
    let g = graphics_from(surf.dc).ok_or_else(|| AppError::Config("GDI+ 上下文创建失败".into()))?;
    let font = make_font(&opts.font_family, font_px as f32);
    if font.is_null() {
        free_graphics(g);
        return Err(AppError::Config("字体创建失败".into()));
    }
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        draw_line(g, margin, margin + i as i32 * line_h, line, font, opts.fg);
    }
    let _ = GdipDeleteFont(font);
    free_graphics(g);

    let mut out = surf.read_rows();
    // 文本贴图是不透明内容：alpha 统一置满
    for px in out.chunks_exact_mut(4) {
        px[3] = 0xFF;
    }
    Ok((w, h, out))
}

/// 贪心换行（按字符切分，对中文友好；英文单词可能被从中间断开）。
unsafe fn wrap_lines(g: *mut GpGraphics, font: *mut GpFont, text: &str, max_width: i32) -> Vec<String> {
    let max_w = max_width as f32;
    let mut lines: Vec<String> = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for ch in para.chars() {
            cur.push(ch);
            let (w, _) = measure(g, font, &cur);
            if w > max_w && cur.chars().count() > 1 {
                let last = cur.pop().unwrap();
                lines.push(std::mem::take(&mut cur));
                cur.push(last);
            }
        }
        lines.push(cur);
    }
    lines
}

unsafe fn measure(g: *mut GpGraphics, font: *mut GpFont, text: &str) -> (f32, f32) {
    let mut sf: *mut GpStringFormat = null_mut();
    if GdipCreateStringFormat(0, 0, &mut sf).0 != 0 || sf.is_null() {
        return (0.0, 0.0);
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = RectF {
        X: 0.0,
        Y: 0.0,
        Width: 16384.0,
        Height: 16384.0,
    };
    let mut r = RectF::default();
    let _ = GdipMeasureString(
        g,
        PCWSTR::from_raw(wide.as_ptr()),
        wide.len() as i32,
        font,
        &layout,
        sf,
        &mut r,
        null_mut(),
        null_mut(),
    );
    let _ = GdipDeleteStringFormat(sf);
    (r.Width, r.Height)
}

unsafe fn draw_line(g: *mut GpGraphics, x: i32, y: i32, text: &str, font: *mut GpFont, color: Rgba) {
    let mut sf: *mut GpStringFormat = null_mut();
    if GdipCreateStringFormat(0, 0, &mut sf).0 != 0 || sf.is_null() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = RectF {
        X: x as f32,
        Y: y as f32,
        Width: 16384.0,
        Height: 16384.0,
    };
    let mut b: *mut GpSolidFill = null_mut();
    if GdipCreateSolidFill(color.to_argb(), &mut b).0 == 0 && !b.is_null() {
        let _ = GdipDrawString(
            g,
            PCWSTR::from_raw(wide.as_ptr()),
            wide.len() as i32,
            font,
            &layout,
            sf,
            b as *const GpBrush,
        );
        let _ = GdipDeleteBrush(b as *mut GpBrush);
    }
    let _ = GdipDeleteStringFormat(sf);
}

/// 底色填充（BGRA 字节序）。
unsafe fn fill_bg(bits: *mut u32, w: i32, h: i32, bg: Rgba) {
    let p = bits as *mut u8;
    for i in 0..(w as usize * h as usize) {
        let o = i * 4;
        *p.add(o) = bg.b;
        *p.add(o + 1) = bg.g;
        *p.add(o + 2) = bg.r;
        *p.add(o + 3) = bg.a;
    }
}

/// 按族名建字体（族名不存在时回退通用无衬线）。
unsafe fn make_font(name: &str, emsize: f32) -> *mut GpFont {
    let mut fam: *mut GpFontFamily = null_mut();
    let fam_name = crate::util::to_wide(name);
    let st = GdipCreateFontFamilyFromName(PCWSTR::from_raw(fam_name.as_ptr()), null_mut(), &mut fam);
    if st.0 != 0 || fam.is_null() {
        if windows::Win32::Graphics::GdiPlus::GdipGetGenericFontFamilySansSerif(&mut fam).0 != 0
            || fam.is_null()
        {
            return null_mut();
        }
    }
    let mut font: *mut GpFont = null_mut();
    let st = GdipCreateFont(fam, emsize.max(1.0), 0, UnitPixel, &mut font);
    let _ = GdipDeleteFontFamily(fam);
    if st.0 != 0 || font.is_null() {
        null_mut()
    } else {
        font
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 需要 GDI+，显式运行：渲染中文多行应得到非空、尺寸合理的缓冲。
    #[test]
    #[ignore = "需要 GDI+，供显式运行验证"]
    fn renders_chinese_text() {
        let opts = TextOptions::default();
        let (w, h, buf) = render_text_to_bgra("你好，世界\nhello world", &opts).expect("渲染应成功");
        assert!(w > 0 && h > 0);
        assert_eq!(buf.len(), (w * h * 4) as usize);
        // 至少有一个非底色像素（字形落下）
        assert!(buf.chunks_exact(4).any(|p| p[0] != 0xFF || p[1] != 0xFF || p[2] != 0xFF));
    }

    #[test]
    fn empty_text_is_rejected() {
        assert!(render_text_to_bgra("   \n  ", &TextOptions::default()).is_err());
    }
}
