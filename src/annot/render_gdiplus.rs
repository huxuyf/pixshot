//! 标注引擎：GDI+ 绘制后端（Win32/GDI+，见设计文档 4.2–4.4）。
//!
//! 屏幕预览与导出合成**共用同一组绘制函数**（单一渲染路径，二者永不不一致）。
//! GDI+ 初始化复用 [`crate::icons`] 的进程级惰性初始化。

use std::ptr::null_mut;

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::{
    CombineModeReplace, DashCapRound, DashStyleDash, GpBrush, GpFont, GpFontFamily,
    GpGraphics, GpPen, GpSolidFill, GpStringFormat, GdipCreateFont, GdipCreateFontFamilyFromName,
    GdipCreateFromHDC, GdipCreatePen1, GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush,
    GdipDeleteFont, GdipDeleteFontFamily, GdipDeleteGraphics, GdipDeletePen,
    GdipDeleteStringFormat, GdipDrawEllipseI, GdipDrawLineI, GdipDrawLines, GdipDrawRectangleI,
    GdipDrawString, GdipFillEllipseI, GdipFillPolygonI, GdipFillRectangleI,
    GdipGetGenericFontFamilySansSerif, GdipMeasureString, GdipResetWorldTransform,
    GdipSetClipRectI, GdipSetPenDashStyle, GdipSetPenLineCap197819, GdipSetPenLineJoin,
    GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdipSetStringFormatAlign,
    GdipSetStringFormatLineAlign, GdipSetTextRenderingHint, GdipTranslateWorldTransform,
    FillModeAlternate, LineCapRound, LineJoinRound, MatrixOrderPrepend, PixelOffsetModeHighQuality,
    Point, PointF, RectF, SmoothingModeAntiAlias, StringAlignmentCenter, TextRenderingHintAntiAlias,
    UnitPixel,
};

use super::geometry::{Pt, Rect};
use super::hit_test::Handle;
use super::shape::{Shape, ARROW_HALF_ANGLE_TAN};
use super::style::{BadgeStyle, PenKind, Rgba, TextFrame, TextStyle};

// 渲染相关的可变项（字体族、爆炸云参数、选中态配色、渐隐起点透明度等）
// 由 `config::get()` 提供，见 `config.ini` 的 [annot] 段。

/// GDI+ `FontStyle`：常规 / 粗体 / 斜体（`FontStyleRegular=0`）。
const FS_REGULAR: i32 = 0;
const FS_BOLD: i32 = 1;
const FS_ITALIC: i32 = 2;

/// 从 HDC 建立已开启抗锯齿的 GDI+ 绘图上下文。
pub unsafe fn graphics_from(hdc: HDC) -> Option<*mut GpGraphics> {
    if !crate::icons::gdiplus_ready() {
        return None;
    }
    let mut g: *mut GpGraphics = null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 || g.is_null() {
        return None;
    }
    let _ = GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
    let _ = GdipSetPixelOffsetMode(g, PixelOffsetModeHighQuality);
    let _ = GdipSetTextRenderingHint(g, TextRenderingHintAntiAlias);
    Some(g)
}

/// 释放绘图上下文。
pub unsafe fn free_graphics(g: *mut GpGraphics) {
    if !g.is_null() {
        let _ = GdipDeleteGraphics(g);
    }
}

/// 平移原点（选区原点 → 局部坐标 0,0，见 4.1）。
pub unsafe fn set_origin(g: *mut GpGraphics, ox: f32, oy: f32) {
    let _ = GdipTranslateWorldTransform(g, ox, oy, MatrixOrderPrepend);
}

/// 清除原点平移。
pub unsafe fn reset_origin(g: *mut GpGraphics) {
    let _ = GdipResetWorldTransform(g);
}

/// 把绘制裁剪到选区（局部坐标），防止预览越出选区（见 4.2）。
pub unsafe fn clip(g: *mut GpGraphics, r: Rect) {
    let _ = GdipSetClipRectI(g, r.x, r.y, r.w, r.h, CombineModeReplace);
}

unsafe fn make_pen(color: Rgba, width: i32) -> *mut GpPen {
    let mut p: *mut GpPen = null_mut();
    if GdipCreatePen1(color.to_argb(), width.max(1) as f32, UnitPixel, &mut p).0 != 0 || p.is_null()
    {
        return null_mut();
    }
    let _ = GdipSetPenLineCap197819(p, LineCapRound, LineCapRound, DashCapRound);
    let _ = GdipSetPenLineJoin(p, LineJoinRound);
    p
}

unsafe fn make_dashed_pen(color: Rgba, width: i32) -> *mut GpPen {
    let p = make_pen(color, width);
    if !p.is_null() {
        let _ = GdipSetPenDashStyle(p, DashStyleDash);
    }
    p
}

unsafe fn make_brush(color: Rgba) -> *mut GpBrush {
    let mut b: *mut GpSolidFill = null_mut();
    if GdipCreateSolidFill(color.to_argb(), &mut b).0 != 0 || b.is_null() {
        return null_mut();
    }
    b as *mut GpBrush
}

/// 绘制一个已提交/预览形状（共用；无选中态）。
pub unsafe fn draw_shape(g: *mut GpGraphics, shape: &Shape) {
    match shape {
        Shape::Rect { bbox, style, fill } => {
            if let Some(f) = fill {
                let b = make_brush(*f);
                if !b.is_null() {
                    let _ = GdipFillRectangleI(g, b, bbox.x, bbox.y, bbox.w, bbox.h);
                    let _ = GdipDeleteBrush(b);
                }
            }
            let p = make_pen(style.color, style.width);
            if !p.is_null() {
                let _ = GdipDrawRectangleI(g, p, bbox.x, bbox.y, bbox.w, bbox.h);
                let _ = GdipDeletePen(p);
            }
        }
        Shape::Ellipse { bbox, style } => {
            let p = make_pen(style.color, style.width);
            if !p.is_null() {
                let _ = GdipDrawEllipseI(g, p, bbox.x, bbox.y, bbox.w, bbox.h);
                let _ = GdipDeletePen(p);
            }
        }
        Shape::Line { from, to, style } => {
            let p = make_pen(style.color, style.width);
            if !p.is_null() {
                let _ = GdipDrawLineI(g, p, from.x, from.y, to.x, to.y);
                let _ = GdipDeletePen(p);
            }
        }
        Shape::Pen { pts, style } => draw_pen(g, pts, style),
        Shape::Arrow {
            from,
            to,
            style,
            head_len,
        } => draw_arrow(g, *from, *to, style.color, style.width, *head_len),
        Shape::StepNo {
            center,
            radius,
            number,
            style,
        } => draw_stepno(g, *center, *radius, *number, style),
        Shape::Text {
            origin,
            text,
            bbox,
            style,
            frame,
            ..
        } => {
            // 外框先画在文字之下
            draw_text_frame(g, *bbox, *frame, style);
            draw_text(g, *origin, text, style);
        }
        // 马赛克/高亮不在此绘制：由 session 的 CPU 像素算子完成（见 session::draw_one）
        Shape::Mosaic { .. } | Shape::Highlight { .. } => {}
    }
}

/// 自由画笔：按段绘制。硬笔等宽；软笔按点距渐细；渐隐笔沿轨迹升透明度。
/// 相邻点距离过短会创建过多笔刷，故已由控制器按最小间距抽稀。
unsafe fn draw_pen(g: *mut GpGraphics, pts: &[Pt], style: &super::style::PenStyle) {
    if pts.len() < 2 {
        return;
    }
    let n = pts.len();
    // 软笔：点距（≈笔速）归一化到 [0.35, 1]，慢处粗、快处细
    let step_len: Vec<f32> = pts
        .windows(2)
        .map(|w| {
            let dx = (w[1].x - w[0].x) as f32;
            let dy = (w[1].y - w[0].y) as f32;
            (dx * dx + dy * dy).sqrt().max(0.5)
        })
        .collect();
    let avg = (step_len.iter().sum::<f32>() / step_len.len() as f32).max(0.5);
    for i in 0..n - 1 {
        let t = (i as f32 + 0.5) / (n - 1) as f32; // 轨迹进度 0→1
        let (color, width) = match style.kind {
            PenKind::Solid => (style.color, style.width),
            PenKind::Soft => {
                let ratio = (avg / step_len[i]).clamp(0.45, 1.0);
                (style.color, ((style.width as f32) * ratio).round().max(1.0) as i32)
            }
            PenKind::Fade => {
                let min_a = crate::config::get().fade_min_alpha;
                let a = min_a + (1.0 - min_a) * t;
                let mut c = style.color;
                c.a = (c.a as f32 * a).round().clamp(0.0, 255.0) as u8;
                (c, style.width)
            }
        };
        let p = make_pen(color, width);
        if !p.is_null() {
            let _ = GdipDrawLineI(g, p, pts[i].x, pts[i].y, pts[i + 1].x, pts[i + 1].y);
            let _ = GdipDeletePen(p);
        }
    }
}

/// 文字外框：气泡（圆角矩形 + 尾巴）/ 爆炸云（波浪轮廓），描边色取文字色。
unsafe fn draw_text_frame(g: *mut GpGraphics, bbox: Rect, frame: TextFrame, style: &TextStyle) {
    if frame == TextFrame::None || bbox.w <= 0 || bbox.h <= 0 {
        return;
    }
    let pad = (style.font_px / 2).max(6);
    let outer = bbox.inflate(pad);
    let pts: Vec<PointF> = match frame {
        TextFrame::Bubble => bubble_outline(outer, pad),
        TextFrame::Burst => burst_outline(outer),
        TextFrame::None => return,
    };
    if pts.len() < 2 {
        return;
    }
    let p = make_pen(style.color, (style.font_px / 8).max(2));
    if !p.is_null() {
        let _ = GdipDrawLines(g, p, pts.as_ptr(), pts.len() as i32);
        let _ = GdipDeletePen(p);
    }
}

/// 圆角矩形（顺时针采样）+ 底部尾巴，作为闭合折线点集。
fn bubble_outline(b: Rect, radius: i32) -> Vec<PointF> {
    let r = radius.clamp(2, (b.w.min(b.h) / 2).max(2)) as f32;
    let (l, t, rr, bo) = (b.x as f32, b.y as f32, b.right() as f32, b.bottom() as f32);
    let step = 6;
    let arc = |cx: f32, cy: f32, a0: f32, a1: f32| -> Vec<(f32, f32)> {
        (0..=step)
            .map(|k| {
                let a = (a0 + (a1 - a0) * k as f32 / step as f32).to_radians();
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect()
    };
    let mut pts: Vec<(f32, f32)> = Vec::new();
    pts.push((l + r, t));
    pts.push((rr - r, t));
    pts.extend(arc(rr - r, t + r, -90.0, 0.0)); // 右上
    pts.push((rr, bo - r));
    pts.extend(arc(rr - r, bo - r, 0.0, 90.0)); // 右下
    // 底边（右→左），中间插入向下的尾巴尖角
    let cx = (l + rr) / 2.0;
    let span = (b.w as f32 / 6.0).max(8.0);
    pts.push((cx + span, bo));
    pts.push((cx, bo + (b.h as f32 / 4.0).max(10.0)));
    pts.push((cx - span, bo));
    pts.push((l + r, bo));
    pts.extend(arc(l + r, bo - r, 90.0, 180.0)); // 左下
    pts.push((l, t + r));
    pts.extend(arc(l + r, t + r, 180.0, 270.0)); // 左上
    if let Some(first) = pts.first().copied() {
        pts.push(first);
    }
    pts.into_iter().map(|(x, y)| PointF { X: x, Y: y }).collect()
}

/// 爆炸云：以椭圆为基，半径按 `1 + amp·sin(bumps·θ)` 起伏，细采样成波浪闭合轮廓。
fn burst_outline(b: Rect) -> Vec<PointF> {
    let cfg = crate::config::get();
    let (cx, cy) = (b.x as f32 + b.w as f32 / 2.0, b.y as f32 + b.h as f32 / 2.0);
    let (rx, ry) = (b.w as f32 / 2.0, b.h as f32 / 2.0);
    let amp = cfg.burst_amp * rx.min(ry).max(6.0);
    let steps = 120;
    let mut pts = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let th = std::f32::consts::TAU * i as f32 / steps as f32;
        let bump = 1.0 + (cfg.burst_bumps * th).sin() * (amp / rx.max(1.0));
        let x = cx + rx * bump * th.cos();
        let y = cy + ry * bump * th.sin();
        pts.push(PointF { X: x, Y: y });
    }
    pts
}

/// 文字的 GDI+ `FontStyle` 位（常规 / 粗体 / 斜体可叠加）。
fn fs_of(style: &TextStyle) -> i32 {
    let mut f = FS_REGULAR;
    if style.bold {
        f |= FS_BOLD;
    }
    if style.italic {
        f |= FS_ITALIC;
    }
    f
}

/// 文字：YaHei，多行按 `\n`，从 `origin` 左上角排布（真实字形尺寸，
/// 不走估算 bbox）。粗斜体取自 [`TextStyle`]。
unsafe fn draw_text(g: *mut GpGraphics, origin: Pt, text: &str, style: &TextStyle) {
    if text.trim().is_empty() {
        return;
    }
    let font = make_font_family(&crate::config::get().text_font, style.font_px.max(1) as f32, fs_of(style));
    if font.is_null() {
        return;
    }
    let mut sf: *mut GpStringFormat = null_mut();
    let ok = GdipCreateStringFormat(0, 0, &mut sf).0 == 0 && !sf.is_null();
    if !ok {
        let _ = GdipDeleteFont(font);
        return;
    }
    // 默认 StringFormat = 左上对齐：从 layout 原点直接排布
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = RectF {
        X: origin.x as f32,
        Y: origin.y as f32,
        Width: 16384.0,
        Height: 16384.0,
    };
    let b = make_brush(style.color);
    if !b.is_null() {
        let _ = GdipDrawString(
            g,
            PCWSTR::from_raw(wide.as_ptr()),
            wide.len() as i32,
            font,
            &layout,
            sf,
            b as *const GpBrush,
        );
        let _ = GdipDeleteBrush(b);
    }
    let _ = GdipDeleteStringFormat(sf);
    let _ = GdipDeleteFont(font);
}

/// 文字输入光标：正文末尾 2px 竖线（GdipMeasureString 量测末行宽高定位）。
pub unsafe fn draw_caret(g: *mut GpGraphics, origin: Pt, text: &str, style: &TextStyle) {
    let font_px = style.font_px;
    let font = make_font_family(&crate::config::get().text_font, font_px.max(1) as f32, fs_of(style));
    if font.is_null() {
        return;
    }
    let mut sf: *mut GpStringFormat = null_mut();
    if GdipCreateStringFormat(0, 0, &mut sf).0 != 0 || sf.is_null() {
        let _ = GdipDeleteFont(font);
        return;
    }
    let measure = |s: &str| -> RectF {
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut r = RectF::default();
        let layout = RectF {
            X: 0.0,
            Y: 0.0,
            Width: 16384.0,
            Height: 16384.0,
        };
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
        r
    };
    let lines = text.split('\n').count().max(1);
    let full = measure(text);
    // 行高：整体高度 / 行数（量测兜底为字号的 1.4 倍）
    let line_h = if full.Height > 0.5 {
        full.Height / lines as f32
    } else {
        font_px as f32 * 1.4
    };
    let last_line = text.rsplit('\n').next().unwrap_or("");
    let last = measure(last_line);
    let _ = GdipDeleteStringFormat(sf);
    let _ = GdipDeleteFont(font);

    let cx = origin.x as f32 + last.Width.round();
    let cy = origin.y as f32 + (lines - 1) as f32 * line_h;
    let b = make_brush(style.color);
    if !b.is_null() {
        let _ = GdipFillRectangleI(
            g,
            b,
            cx.round() as i32,
            cy.round() as i32,
            2,
            line_h.round().max(4.0) as i32,
        );
        let _ = GdipDeleteBrush(b);
    }
}

/// 在 (x,y) 左上角绘制一行文字（选区尺寸标签用；Segoe UI Bold，给定色）。
pub unsafe fn draw_label_string(
    g: *mut GpGraphics,
    x: f32,
    y: f32,
    text: &str,
    font_px: f32,
    color: Rgba,
) {
    if text.trim().is_empty() {
        return;
    }
    let font = make_font_family("Segoe UI", font_px.max(1.0), FS_BOLD);
    if font.is_null() {
        return;
    }
    let mut sf: *mut GpStringFormat = null_mut();
    if GdipCreateStringFormat(0, 0, &mut sf).0 != 0 || sf.is_null() {
        let _ = GdipDeleteFont(font);
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = RectF {
        X: x,
        Y: y,
        Width: 16384.0,
        Height: 16384.0,
    };
    let b = make_brush(color);
    if !b.is_null() {
        let _ = GdipDrawString(
            g,
            PCWSTR::from_raw(wide.as_ptr()),
            wide.len() as i32,
            font,
            &layout,
            sf,
            b as *const GpBrush,
        );
        let _ = GdipDeleteBrush(b);
    }
    let _ = GdipDeleteStringFormat(sf);
    let _ = GdipDeleteFont(font);
}

/// 箭头：线段 + 实心三角头（头部几何见 4.3）。
unsafe fn draw_arrow(g: *mut GpGraphics, from: Pt, to: Pt, color: Rgba, width: i32, head_len: i32) {
    let dx = (to.x - from.x) as f32;
    let dy = (to.y - from.y) as f32;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return; // 退化保护
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = head_len as f32;
    let half = head * ARROW_HALF_ANGLE_TAN;
    // 头部底边中点 = 尖端沿反方向退 head
    let bx = to.x as f32 - ux * head;
    let by = to.y as f32 - uy * head;
    let (px, py) = (-uy, ux); // 单位法向
    let b1 = Point {
        X: (bx + px * half).round() as i32,
        Y: (by + py * half).round() as i32,
    };
    let b2 = Point {
        X: (bx - px * half).round() as i32,
        Y: (by - py * half).round() as i32,
    };

    // 线段止于头部底边（避免线穿透头部）
    let p = make_pen(color, width);
    if !p.is_null() {
        let _ = GdipDrawLineI(g, p, from.x, from.y, bx.round() as i32, by.round() as i32);
        let _ = GdipDeletePen(p);
    }
    // 三角头填充
    let b = make_brush(color);
    if !b.is_null() {
        let pts = [
            Point {
                X: to.x,
                Y: to.y,
            },
            b1,
            b2,
        ];
        let _ = GdipFillPolygonI(g, b, pts.as_ptr(), 3, FillModeAlternate);
        let _ = GdipDeleteBrush(b);
    }
}

unsafe fn draw_stepno(
    g: *mut GpGraphics,
    center: Pt,
    radius: i32,
    number: u32,
    style: &BadgeStyle,
) {
    let x = center.x - radius;
    let y = center.y - radius;
    let d = radius * 2;

    let b = make_brush(style.fill);
    if !b.is_null() {
        let _ = GdipFillEllipseI(g, b, x, y, d, d);
        let _ = GdipDeleteBrush(b);
    }
    if let Some(outline) = style.outline {
        let p = make_pen(outline, 2);
        if !p.is_null() {
            let _ = GdipDrawEllipseI(g, p, x, y, d, d);
            let _ = GdipDeletePen(p);
        }
    }

    // 数字：Segoe UI Bold，字号 = radius × 1.15，圆内居中（见 4.3）
    let text = number.to_string();
    let wide: Vec<u16> = text.encode_utf16().collect();
    let emsize = radius as f32 * 1.15;
    let font = make_font(emsize);
    if font.is_null() {
        return;
    }
    let mut sf: *mut GpStringFormat = null_mut();
    if GdipCreateStringFormat(0, 0, &mut sf).0 != 0 || sf.is_null() {
        let _ = GdipDeleteFont(font);
        return;
    }
    let _ = GdipSetStringFormatAlign(sf, StringAlignmentCenter);
    let _ = GdipSetStringFormatLineAlign(sf, StringAlignmentCenter);
    let tb = make_brush(style.text);
    if !tb.is_null() {
        let layout = RectF {
            X: x as f32,
            Y: y as f32,
            Width: d as f32,
            Height: d as f32,
        };
        let _ = GdipDrawString(
            g,
            PCWSTR::from_raw(wide.as_ptr()),
            wide.len() as i32,
            font,
            &layout,
            sf,
            tb as *const GpBrush,
        );
        let _ = GdipDeleteBrush(tb);
    }
    let _ = GdipDeleteStringFormat(sf);
    let _ = GdipDeleteFont(font);
}

unsafe fn make_font(emsize: f32) -> *mut GpFont {
    make_font_family("Segoe UI", emsize, FS_BOLD)
}

/// 按字体族名建字体（族名不存在时回退通用无衬线）。`style` 为 GDI+ `FontStyle` 位。
unsafe fn make_font_family(name: &str, emsize: f32, style: i32) -> *mut GpFont {
    let mut fam: *mut GpFontFamily = null_mut();
    let fam_name = crate::util::to_wide(name);
    let st = GdipCreateFontFamilyFromName(
        PCWSTR::from_raw(fam_name.as_ptr()),
        null_mut(),
        &mut fam,
    );
    if st.0 != 0 || fam.is_null() {
        if GdipGetGenericFontFamilySansSerif(&mut fam).0 != 0 || fam.is_null() {
            return null_mut();
        }
    }
    let mut font: *mut GpFont = null_mut();
    let st = GdipCreateFont(fam, emsize.max(1.0), style, UnitPixel, &mut font);
    let _ = GdipDeleteFontFamily(fam);
    if st.0 != 0 || font.is_null() {
        null_mut()
    } else {
        font
    }
}

/// 绘制选中态（虚线框 + 8 控制柄 / 序号虚线圆，见 4.4）。
pub unsafe fn draw_chrome(g: *mut GpGraphics, shape: &Shape, dpi: i32) {
    let s = |v: i32| crate::util::scaled(v, dpi);
    let cfg = crate::config::get();
    let dash = cfg.chrome_color;
    match shape {
        Shape::StepNo {
            center, radius, ..
        } => {
            let rr = radius + s(4);
            let p = make_dashed_pen(dash, 1);
            if !p.is_null() {
                let _ = GdipDrawEllipseI(g, p, center.x - rr, center.y - rr, rr * 2, rr * 2);
                let _ = GdipDeletePen(p);
            }
        }
        _ => {
            let bbox = shape.bbox();
            let box_ = bbox.inflate(s(2));
            let p = make_dashed_pen(dash, 1);
            if !p.is_null() {
                let _ = GdipDrawRectangleI(g, p, box_.x, box_.y, box_.w, box_.h);
                let _ = GdipDeletePen(p);
            }
            let size = s(cfg.handle_size).max(3);
            let half = size / 2;
            let fill = make_brush(cfg.handle_fill);
            let pen = make_pen(cfg.handle_stroke, 1);
            for h in Handle::ALL {
                let c = h.center(bbox);
                let (hx, hy) = (c.x - half, c.y - half);
                if !fill.is_null() {
                    let _ = GdipFillRectangleI(g, fill, hx, hy, size, size);
                }
                if !pen.is_null() {
                    let _ = GdipDrawRectangleI(g, pen, hx, hy, size, size);
                }
            }
            if !fill.is_null() {
                let _ = GdipDeleteBrush(fill);
            }
            if !pen.is_null() {
                let _ = GdipDeletePen(pen);
            }
        }
    }
}
