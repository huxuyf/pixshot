//! SVG path（`d` 属性）→ 折线子路径的展平器（纯 Rust，无 Windows 依赖）。
//!
//! 用途：工具条图标改用 Lucide 图标集后，图标以 Lucide 官方的 **24×24 viewBox 路径
//! 数据**（`stroke-width: 2`）为唯一来源，避免两套坐标/两处维护。本模块只负责把
//! 路径字符串解析成一组折线（曲线按足够密度采样），绘制交给 [`crate::icons`] 用 GDI+
//! 抗锯齿描边完成 —— 与仓库「零图片资源、矢量自绘」的一贯做法一致。
//!
//! 支持的命令覆盖 Lucide 图标实际用到的子集：`M/m L/l H/h V/v C/c A/a Z/z`
//! （含命令的隐式重复，如 `M` 后跟多组坐标等价于先 `M` 再若干 `L`）。
//! 圆弧按 SVG 规范 F.6.5 做「端点参数 → 圆心参数」换算后按角度采样；三次贝塞尔
//! 按固定段数采样。采样密度按 24 单位坐标系取值，渲染到 ~20px 时已足够平滑。

use std::f32::consts::PI;

/// 令牌：绘图命令字母或坐标数值。
#[derive(Clone, Copy, Debug)]
enum Tok {
    Cmd(char),
    Num(f32),
}

/// 把 `d` 字符串切成命令 / 数值令牌。数值支持前导符号、小数点、科学计数法，
/// 以及 `".9-2"` 这类紧挨着写（无分隔符）的连写形式。
fn tokenize(d: &str) -> Vec<Tok> {
    let b: Vec<char> = d.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() || c == ',' {
            i += 1;
        } else if c.is_ascii_alphabetic() {
            out.push(Tok::Cmd(c));
            i += 1;
        } else {
            let start = i;
            if c == '-' || c == '+' {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == '.' {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == 'e' || b[i] == 'E') {
                i += 1;
                if i < b.len() && (b[i] == '-' || b[i] == '+') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i == start {
                i += 1; // 无法识别的字符：跳过，避免死循环
                continue;
            }
            let s: String = b[start..i].iter().collect();
            if let Ok(v) = s.parse::<f32>() {
                out.push(Tok::Num(v));
            }
        }
    }
    out
}

/// 解析游标：按需取出下一个数值。
struct Cursor<'a> {
    toks: &'a [Tok],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn num(&mut self) -> Option<f32> {
        if let Some(Tok::Num(v)) = self.toks.get(self.i) {
            self.i += 1;
            Some(*v)
        } else {
            None
        }
    }
    fn cmd(&self) -> Option<char> {
        match self.toks.get(self.i) {
            Some(Tok::Cmd(c)) => Some(*c),
            _ => None,
        }
    }
}

/// 把剩余的坐标点抽成 `(x, y)` 序列（数量不足则停止）。
fn nums<const N: usize>(cur: &mut Cursor) -> Option<[f32; N]> {
    let mut a = [0.0f32; N];
    for slot in a.iter_mut() {
        *slot = cur.num()?;
    }
    Some(a)
}

/// 三次贝塞尔采样（`p0` 起点，`p1/p2` 控制点，`p3` 终点），追加除起点外的采样点。
fn sample_cubic(out: &mut Vec<(f32, f32)>, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)) {
    const N: usize = 20;
    for k in 1..=N {
        let t = k as f32 / N as f32;
        let u = 1.0 - t;
        let x = u * u * u * p0.0 + 3.0 * u * u * t * p1.0 + 3.0 * u * t * t * p2.0 + t * t * t * p3.0;
        let y = u * u * u * p0.1 + 3.0 * u * u * t * p1.1 + 3.0 * u * t * t * p2.1 + t * t * t * p3.1;
        out.push((x, y));
    }
}

/// 椭圆弧（SVG F.6.5）：端点参数 `(rx, ry, phi, large_arc, sweep)` + 终点 → 采样点。
#[allow(clippy::too_many_arguments)]
fn sample_arc(
    out: &mut Vec<(f32, f32)>,
    x0: f32,
    y0: f32,
    mut rx: f32,
    mut ry: f32,
    phi_deg: f32,
    large_arc: bool,
    sweep: bool,
    x1: f32,
    y1: f32,
) {
    // 起终点重合：按规范不画任何东西
    if (x0 - x1).abs() < 1e-6 && (y0 - y1).abs() < 1e-6 {
        return;
    }
    rx = rx.abs();
    ry = ry.abs();
    if rx < 1e-6 || ry < 1e-6 {
        out.push((x1, y1)); // 半径为 0 → 退化为直线
        return;
    }
    let phi = phi_deg.to_radians();
    let (sin_p, cos_p) = (phi.sin(), phi.cos());

    let dx2 = (x0 - x1) / 2.0;
    let dy2 = (y0 - y1) / 2.0;
    let x1p = cos_p * dx2 + sin_p * dy2;
    let y1p = -sin_p * dx2 + cos_p * dy2;

    // 半径不足以连接两点时按规范等比放大
    let lam = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }

    let sign = if large_arc != sweep { 1.0 } else { -1.0 };
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let co = sign * (num / den).max(0.0).sqrt();
    let cxp = co * (rx * y1p / ry);
    let cyp = co * (-ry * x1p / rx);
    let cx = cos_p * cxp - sin_p * cyp + (x0 + x1) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y0 + y1) / 2.0;

    let angle = |ux: f32, uy: f32, vx: f32, vy: f32| -> f32 {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let theta0 = angle(1.0, 0.0, ux, uy);
    let mut dtheta = angle(ux, uy, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dtheta > 0.0 {
        dtheta -= 2.0 * PI;
    } else if sweep && dtheta < 0.0 {
        dtheta += 2.0 * PI;
    }

    // 约 7.5° 一段，最少 2 段
    let n = ((dtheta.abs() / (PI / 24.0)).ceil() as usize).max(2);
    for k in 1..=n {
        let t = theta0 + dtheta * (k as f32 / n as f32);
        let xt = rx * t.cos();
        let yt = ry * t.sin();
        out.push((cos_p * xt - sin_p * yt + cx, sin_p * xt + cos_p * yt + cy));
    }
}

/// 解析 SVG path 的 `d` 字符串为若干折线子路径（坐标沿用原始单位，不缩放）。
pub fn flatten(d: &str) -> Vec<Vec<(f32, f32)>> {
    let toks = tokenize(d);
    let mut cur = Cursor { toks: &toks, i: 0 };

    let mut subs: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut sub: Vec<(f32, f32)> = Vec::new();
    let mut cur_pt = (0.0f32, 0.0f32);
    let mut start_pt = (0.0f32, 0.0f32);
    let mut cmd = '\0';

    // 结束当前子路径：至少两点才算一条线
    macro_rules! flush {
        () => {
            if sub.len() >= 2 {
                subs.push(std::mem::take(&mut sub));
            } else {
                sub.clear();
            }
        };
    }

    loop {
        // 取命令：显式命令直接消费；否则隐式复用上一条（M/m 的隐式重复等价 L/l）
        if let Some(c) = cur.cmd() {
            cur.i += 1;
            cmd = c;
        } else if matches!(cur.toks.get(cur.i), Some(Tok::Num(_))) {
            cmd = match cmd {
                'M' => 'L',
                'm' => 'l',
                '\0' | 'Z' | 'z' => break,
                other => other,
            };
        } else {
            break;
        }
        let rel = cmd.is_ascii_lowercase();

        match cmd.to_ascii_uppercase() {
            'M' => {
                let Some([x, y]) = nums::<2>(&mut cur) else { break };
                let p = if rel { (cur_pt.0 + x, cur_pt.1 + y) } else { (x, y) };
                flush!();
                cur_pt = p;
                start_pt = p;
                sub.push(p);
            }
            'L' => {
                let Some([x, y]) = nums::<2>(&mut cur) else { break };
                let p = if rel { (cur_pt.0 + x, cur_pt.1 + y) } else { (x, y) };
                cur_pt = p;
                sub.push(p);
            }
            'H' => {
                let Some(x) = cur.num() else { break };
                let p = (if rel { cur_pt.0 + x } else { x }, cur_pt.1);
                cur_pt = p;
                sub.push(p);
            }
            'V' => {
                let Some(y) = cur.num() else { break };
                let p = (cur_pt.0, if rel { cur_pt.1 + y } else { y });
                cur_pt = p;
                sub.push(p);
            }
            'C' => {
                let Some([x1, y1, x2, y2, x, y]) = nums::<6>(&mut cur) else { break };
                let (ox, oy) = if rel { cur_pt } else { (0.0, 0.0) };
                let p1 = (ox + x1, oy + y1);
                let p2 = (ox + x2, oy + y2);
                let p3 = (ox + x, oy + y);
                sample_cubic(&mut sub, cur_pt, p1, p2, p3);
                cur_pt = p3;
            }
            'A' => {
                let Some([rx, ry, rot, large, sweep, x, y]) = nums::<7>(&mut cur) else { break };
                let p = if rel { (cur_pt.0 + x, cur_pt.1 + y) } else { (x, y) };
                sample_arc(&mut sub, cur_pt.0, cur_pt.1, rx, ry, rot, large != 0.0, sweep != 0.0, p.0, p.1);
                cur_pt = p;
            }
            'Z' => {
                cur_pt = start_pt;
                if sub.len() >= 2 {
                    sub.push(start_pt);
                }
            }
            _ => break,
        }
    }
    flush!();
    subs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 直线：`M18 6 6 18` = 移到 (18,6) 再画线到 (6,18)。
    #[test]
    fn moverel_line() {
        let s = flatten("M18 6 6 18");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].len(), 2);
        assert!((s[0][0].0 - 18.0).abs() < 1e-3 && (s[0][0].1 - 6.0).abs() < 1e-3);
        assert!((s[0][1].0 - 6.0).abs() < 1e-3 && (s[0][1].1 - 18.0).abs() < 1e-3);
    }

    /// 相对 moveto + 隐式 lineto：`m6 6 12 12` 从 (0,0) 出发 → (6,6) → (18,18)。
    #[test]
    fn relative_moveto_starts_at_origin() {
        let s = flatten("m6 6 12 12");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].len(), 2);
        assert!((s[0][0].0 - 6.0).abs() < 1e-3 && (s[0][0].1 - 6.0).abs() < 1e-3);
        assert!((s[0][1].0 - 18.0).abs() < 1e-3 && (s[0][1].1 - 18.0).abs() < 1e-3);
    }

    /// 闭合路径 `z` 会补回起点，形成首尾相接的折线。
    #[test]
    fn close_returns_to_start() {
        let s = flatten("M0 0 L10 0 L10 10 Z");
        assert_eq!(s.len(), 1);
        let last = *s[0].last().unwrap();
        assert!(last.0.abs() < 1e-3 && last.1.abs() < 1e-3);
    }

    /// 圆弧 (a) 会被采样为多点平滑折线，且终点精确落在指定坐标。
    #[test]
    fn arc_is_sampled_to_endpoint() {
        // 半径 2 的四分之一圆：从 (3,5) 到 (5,3)
        let s = flatten("M3 5a2 2 0 0 1 2-2");
        assert_eq!(s.len(), 1);
        assert!(s[0].len() > 3, "圆弧应被采样为多点折线");
        let last = *s[0].last().unwrap();
        assert!((last.0 - 5.0).abs() < 1e-3 && (last.1 - 3.0).abs() < 1e-3);
    }

    /// 多点数值连写（无分隔符，如 `.9-2`）也能正确切分。
    #[test]
    fn tightly_packed_numbers() {
        let s = flatten("M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2");
        assert_eq!(s.len(), 1);
        // 起点 (4,16)，终点应为 (16,4)
        assert!((s[0][0].0 - 4.0).abs() < 1e-3 && (s[0][0].1 - 16.0).abs() < 1e-3);
        let last = *s[0].last().unwrap();
        assert!((last.0 - 16.0).abs() < 1e-3 && (last.1 - 4.0).abs() < 1e-3);
    }
}
