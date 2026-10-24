//! 应用图标的纯几何定义（无外部依赖，可单元测试）。
//!
//! 被两处共用：
//! - `build.rs`：编译期渲染 .ico（通过 `#[path]` 引入）；
//! - `tests/icon_geometry.rs`：验证形状判定的边界行为。
//!
//! 这里的函数只接受「归一化到边长 s 的像素坐标」，不涉及任何 Windows API。

/// 圆角半径比例（占边长）。
pub const CORNER: f32 = 0.20;
/// 取景框内边距比例（括号外角距边）。
pub const MARGIN: f32 = 0.295;
/// 括号臂长比例。
pub const ARM: f32 = 0.165;
/// 括号线宽比例。
pub const STROKE: f32 = 0.078;
/// 焦点圆点半径比例。
pub const DOT: f32 = 0.062;

/// 线性插值。
pub fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// 点到线段的最短距离。
pub fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let l2 = dx * dx + dy * dy;
    let t = if l2 <= f32::EPSILON {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / l2).clamp(0.0, 1.0)
    };
    let cx = ax + dx * t;
    let cy = ay + dy * t;
    ((px - cx) * (px - cx) + (py - cy) * (py - cy)).sqrt()
}

/// 是否落在圆角方形内（全出血，边长 s，圆角半径 r）。
pub fn in_rounded(x: f32, y: f32, s: f32, r: f32) -> bool {
    if x < 0.0 || y < 0.0 || x > s || y > s {
        return false;
    }
    let cx = x.clamp(r, s - r);
    let cy = y.clamp(r, s - r);
    if (cx - x).abs() < f32::EPSILON && (cy - y).abs() < f32::EPSILON {
        return true; // 中央十字区域，必定在内
    }
    (cx - x) * (cx - x) + (cy - y) * (cy - y) <= r * r
}

/// 是否落在取景框四角括号或中心焦点圆点上（白色字形）。
pub fn on_glyph(x: f32, y: f32, s: f32) -> bool {
    let m = MARGIN * s;
    let a = ARM * s;
    let half = STROKE * s * 0.5;
    let near = |ax: f32, ay: f32, bx: f32, by: f32| dist_seg(x, y, ax, ay, bx, by) <= half;
    let e = s - m; // 内框右/下边
                   // 4 角 × 2 段
    if near(m, m, m + a, m) || near(m, m, m, m + a) {
        return true;
    }
    if near(e, m, e - a, m) || near(e, m, e, m + a) {
        return true;
    }
    if near(m, e, m + a, e) || near(m, e, m, e - a) {
        return true;
    }
    if near(e, e, e - a, e) || near(e, e, e, e - a) {
        return true;
    }
    // 中心焦点圆点
    let c = s * 0.5;
    let d = DOT * s;
    (x - c) * (x - c) + (y - c) * (y - c) <= d * d
}
