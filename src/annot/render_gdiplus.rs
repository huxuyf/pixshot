//! 标注引擎：GDI+ 绘制后端（Win32/GDI+，见设计文档 4.2–4.4）。
//!
//! 屏幕预览与导出合成**共用同一组绘制函数**（单一渲染路径，二者永不不一致）。
//! GDI+ 初始化复用 [`crate::icons`] 的进程级惰性初始化。

use std::ptr::null_mut;

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::{
    CombineModeReplace, DashCapRound, DashStyleDash, FontStyleBold, GpBrush, GpFont, GpFontFamily,
    GpGraphics, GpPen, GpSolidFill, GpStringFormat, GdipCreateFont, GdipCreateFontFamilyFromName,
    GdipCreateFromHDC, GdipCreatePen1, GdipCreateSolidFill, GdipCreateStringFormat, GdipDeleteBrush,
    GdipDeleteFont, GdipDeleteFontFamily, GdipDeleteGraphics, GdipDeletePen,
    GdipDeleteStringFormat, GdipDrawEllipseI, GdipDrawLineI, GdipDrawRectangleI, GdipDrawString,
    GdipFillEllipseI, GdipFillPolygonI, GdipFillRectangleI, GdipGetGenericFontFamilySansSerif,
    GdipResetWorldTransform, GdipSetClipRectI, GdipSetPenDashStyle, GdipSetPenLineCap197819,
    GdipSetPenLineJoin, GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdipSetStringFormatAlign,
    GdipSetStringFormatLineAlign, GdipSetTextRenderingHint, GdipTranslateWorldTransform,
    FillModeAlternate, LineCapRound, LineJoinRound, MatrixOrderPrepend, PixelOffsetModeHighQuality,
    Point, RectF, SmoothingModeAntiAlias, StringAlignmentCenter, TextRenderingHintAntiAlias,
    UnitPixel,
};

use super::geometry::{Pt, Rect};
use super::hit_test::{Handle, HANDLE_SIZE_LOGICAL};
use super::shape::{Shape, ARROW_HALF_ANGLE_TAN};
use super::style::{BadgeStyle, Rgba};

/// 选中态虚线框颜色（浅灰）。
const CHROME_DASH: Rgba = Rgba::new(0x88, 0x88, 0x88, 0xFF);
/// 控制柄填充色（白）。
const HANDLE_FILL: Rgba = Rgba::new(0xFF, 0xFF, 0xFF, 0xFF);
/// 控制柄描边色（深灰）。
const HANDLE_STROKE: Rgba = Rgba::new(0x55, 0x55, 0x55, 0xFF);

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

/// 绘制一个已提交/预览形状（三类共用；无选中态）。
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
    }
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
    let mut fam: *mut GpFontFamily = null_mut();
    let name = crate::util::to_wide("Segoe UI");
    let st = GdipCreateFontFamilyFromName(
        PCWSTR::from_raw(name.as_ptr()),
        null_mut(),
        &mut fam,
    );
    if st.0 != 0 || fam.is_null() {
        if GdipGetGenericFontFamilySansSerif(&mut fam).0 != 0 || fam.is_null() {
            return null_mut();
        }
    }
    let mut font: *mut GpFont = null_mut();
    let st = GdipCreateFont(fam, emsize.max(1.0), FontStyleBold.0, UnitPixel, &mut font);
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
    match shape {
        Shape::StepNo {
            center, radius, ..
        } => {
            let rr = radius + s(4);
            let p = make_dashed_pen(CHROME_DASH, 1);
            if !p.is_null() {
                let _ = GdipDrawEllipseI(g, p, center.x - rr, center.y - rr, rr * 2, rr * 2);
                let _ = GdipDeletePen(p);
            }
        }
        _ => {
            let bbox = shape.bbox();
            let box_ = bbox.inflate(s(2));
            let p = make_dashed_pen(CHROME_DASH, 1);
            if !p.is_null() {
                let _ = GdipDrawRectangleI(g, p, box_.x, box_.y, box_.w, box_.h);
                let _ = GdipDeletePen(p);
            }
            let size = s(HANDLE_SIZE_LOGICAL).max(3);
            let half = size / 2;
            let fill = make_brush(HANDLE_FILL);
            let pen = make_pen(HANDLE_STROKE, 1);
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
