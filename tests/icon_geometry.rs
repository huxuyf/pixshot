//! 应用图标几何的集成测试。
//!
//! `src/icon_geometry.rs` 同时被 `build.rs`（编译期渲染 .ico）和本测试以
//! `#[path]` 方式引入，因此这里验证的就是实际打入二进制的同一份几何代码
//! ——不存在「测试一份、构建另一份」的漂移。

#[path = "../src/icon_geometry.rs"]
mod geo;

use geo::{dist_seg, in_rounded, lerp3, on_glyph, ARM, CORNER, DOT, MARGIN, STROKE};

/// 测试用边长：取 256 便于心算，几何本身与尺寸无关。
const S: f32 = 256.0;

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

// ---------------------------------------------------------------- lerp3

#[test]
fn lerp3_hits_both_endpoints() {
    let a = [0.0, 0.5, 1.0];
    let b = [1.0, 0.25, 0.0];
    assert_eq!(lerp3(a, b, 0.0), a);
    assert_eq!(lerp3(a, b, 1.0), b);
}

#[test]
fn lerp3_midpoint_is_average() {
    let m = lerp3([0.0, 0.0, 0.0], [1.0, 0.5, 0.2], 0.5);
    assert!(approx(m[0], 0.5) && approx(m[1], 0.25) && approx(m[2], 0.1));
}

/// lerp3 不做外推保护（与 theme::lerp 的 clamp 语义不同）：锁定该约定，
/// 防止误把它当钳制插值用。
#[test]
fn lerp3_extrapolates_beyond_unit_interval() {
    let v = lerp3([0.0, 0.0, 0.0], [1.0, 1.0, 1.0], 2.0);
    assert_eq!(v, [2.0, 2.0, 2.0], "t>1 应外插而非钳制");
    let v = lerp3([1.0, 1.0, 1.0], [2.0, 2.0, 2.0], -1.0);
    assert_eq!(v, [0.0, 0.0, 0.0], "t<0 应外插而非钳制");
}

// ------------------------------------------------------------- dist_seg

#[test]
fn dist_seg_perpendicular_distance() {
    // 线段沿 x 轴，点在正上方 3 单位
    assert!(approx(dist_seg(5.0, 3.0, 0.0, 0.0, 10.0, 0.0), 3.0));
}

#[test]
fn dist_seg_clamps_beyond_endpoints() {
    // 落在延长线上时，距离取到最近端点的距离
    assert!(approx(dist_seg(-4.0, 0.0, 0.0, 0.0, 10.0, 0.0), 4.0));
    assert!(approx(dist_seg(14.0, 0.0, 0.0, 0.0, 10.0, 0.0), 4.0));
}

#[test]
fn dist_seg_degenerate_segment_is_point_distance() {
    // 零长度线段不能除零，应退化为到点的距离
    assert!(approx(dist_seg(3.0, 4.0, 0.0, 0.0, 0.0, 0.0), 5.0));
}

// ----------------------------------------------------------- in_rounded

#[test]
fn in_rounded_centre_is_inside() {
    assert!(in_rounded(S / 2.0, S / 2.0, S, CORNER * S));
}

#[test]
fn in_rounded_corners_are_outside() {
    let r = CORNER * S;
    for (x, y) in [(0.0, 0.0), (S, 0.0), (0.0, S), (S, S)] {
        assert!(!in_rounded(x, y, S, r), "corner ({x}, {y}) should be cut");
    }
}

#[test]
fn in_rounded_edge_midpoints_are_inside() {
    let r = CORNER * S;
    // 四条边的中点必然在圆角方形内（圆角只削角、不削边）
    assert!(in_rounded(S / 2.0, 0.0, S, r));
    assert!(in_rounded(S / 2.0, S, S, r));
    assert!(in_rounded(0.0, S / 2.0, S, r));
    assert!(in_rounded(S, S / 2.0, S, r));
}

#[test]
fn in_rounded_out_of_bounds_is_false() {
    let r = CORNER * S;
    assert!(!in_rounded(-0.5, S / 2.0, S, r));
    assert!(!in_rounded(S / 2.0, S + 0.5, S, r));
}

/// 边界包含语义：x == s / y == s 仍在内（右/下边缘闭合），超出一律在外。
#[test]
fn in_rounded_boundary_is_inclusive() {
    let r = CORNER * S;
    assert!(in_rounded(S, S / 2.0, S, r), "右边缘中点在内");
    assert!(in_rounded(S / 2.0, S, S, r), "下边缘中点在内");
    assert!(in_rounded(0.0, S / 2.0, S, r), "左边缘中点在内");
    // 角弧上的点：圆心 (r,r) 半径 r 处恰在边界上
    assert!(in_rounded(r * 2.0, r, S, r), "圆弧过渡点在内");
    assert!(!in_rounded(S + f32::EPSILON * S, S / 2.0, S, r));
}

// -------------------------------------------------------------- on_glyph

#[test]
fn on_glyph_hits_all_four_corners() {
    let m = MARGIN * S;
    let a = ARM * S;
    // 每个角取「横臂」的起点与「竖臂」的起点，二者都在描边中心线上
    let pts = [
        (m, m),
        (m + a * 0.5, m),
        (m, m + a * 0.5),
        (S - m, m),
        (S - m, m + a * 0.5),
        (m, S - m),
        (m + a * 0.5, S - m),
        (S - m, S - m),
    ];
    for (x, y) in pts {
        assert!(on_glyph(x, y, S), "({x}, {y}) should be on glyph");
    }
}

#[test]
fn on_glyph_hits_centre_dot() {
    assert!(on_glyph(S / 2.0, S / 2.0, S));
    // 圆点半径内侧仍命中，外侧不命中
    let d = DOT * S;
    assert!(on_glyph(S / 2.0 + d * 0.5, S / 2.0, S));
    assert!(!on_glyph(S / 2.0 + d * 1.5, S / 2.0, S));
}

#[test]
fn on_glyph_misses_empty_regions() {
    // 取景框内部应为空白：这是截屏工具「透明取景窗」的语义。
    // 注意采样点必须避开中心圆点（半径 DOT*S ≈ 15.9），否则会误判为命中。
    let m = MARGIN * S;
    let dot_r = DOT * S;
    assert!(S / 2.0 - (m + STROKE * S) > dot_r * 2.0, "采样点离圆点太近");
    assert!(!on_glyph(S * 0.5, m + STROKE * S, S), "框内上边缘下方应为空");
    assert!(
        !on_glyph(m + ARM * S * 0.5, S * 0.5, S),
        "左侧括号与中心点之间应为空"
    );
    assert!(
        !on_glyph(S * 0.5, S * 0.5 - dot_r * 2.5, S),
        "圆点正上方（圆外）应为空"
    );
}

#[test]
fn on_glyph_is_symmetric() {
    // 四角括号对称：镜像点应得到相同判定
    let m = MARGIN * S;
    let a = ARM * S;
    for t in [0.0, 0.25, 0.5, 1.0] {
        let x = m + a * t;
        assert_eq!(on_glyph(x, m, S), on_glyph(S - x, m, S));
        assert_eq!(on_glyph(x, m, S), on_glyph(x, S - m, S));
    }
}

// --------------------------------------------------------------- 常量约束

#[test]
fn constants_leave_room_for_glyph() {
    // 括号必须完整落在圆角方形内，否则图标边缘会被裁掉
    assert!(MARGIN > 0.0 && MARGIN < 0.5);
    assert!(MARGIN + ARM <= 0.5 + 1e-6, "arm must not cross the centre");
    assert!(STROKE > 0.0 && STROKE < ARM, "stroke must be thinner than arm");
    assert!(CORNER > 0.0 && CORNER < 0.5);
    assert!(DOT > 0.0 && DOT < 0.5);
}
