//! ONNX 引擎的图像前/后处理（纯函数，无 ort 依赖，便于单测）。
//!
//! 覆盖：BGRA→RGB、双三次缩放（复用 [`super::cubic_kernel`]）、裁剪，
//! 以及 DB 文本检测的输入张量与后处理（概率图 → 二值 → 连通域 → 打分 → unclip 扩张）。

/// 自上而下、行距 = width×3 的 RGB 图像。
#[derive(Clone)]
pub struct Rgb {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
}

impl Rgb {
    pub fn new(w: usize, h: usize) -> Self {
        Rgb { w, h, data: vec![255u8; w * h * 3] }
    }

    #[inline]
    fn px(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.w + x) * 3;
        if i + 2 < self.data.len() {
            [self.data[i], self.data[i + 1], self.data[i + 2]]
        } else {
            [255, 255, 255]
        }
    }
}

/// BGRA（自上而下）→ RGB。
pub fn bgra_to_rgb(bgra: &[u8], w: usize, h: usize) -> Rgb {
    let mut data = vec![255u8; w * h * 3];
    let n = w * h;
    for i in 0..n {
        let s = i * 4;
        let d = i * 3;
        if s + 2 < bgra.len() {
            // BGRA 内存序：B,G,R,A → 取 R,G,B
            data[d] = bgra[s + 2];
            data[d + 1] = bgra[s + 1];
            data[d + 2] = bgra[s];
        }
    }
    Rgb { w, h, data }
}

/// 可分离双三次缩放（先横后纵），与 [`super::resize_bicubic`] 同一套核。RGB 三通道。
pub fn resize(src: &Rgb, nw: usize, nh: usize) -> Rgb {
    if src.w == 0 || src.h == 0 || nw == 0 || nh == 0 {
        return Rgb::new(nw.max(1), nh.max(1));
    }
    if nw == src.w && nh == src.h {
        return src.clone();
    }
    let (w, h) = (src.w, src.h);
    let read = |x: usize, y: usize, c: usize| -> f32 {
        let i = (y * w + x) * 3 + c;
        if i < src.data.len() {
            src.data[i] as f32
        } else {
            255.0
        }
    };

    // 横向
    let mut tmp = vec![0f32; nw * h * 3];
    let rx = w as f32 / nw as f32;
    let fx = rx.max(1.0);
    let support_x = 2.0 * fx;
    for y in 0..h {
        for x in 0..nw {
            let s = (x as f32 + 0.5) * rx - 0.5;
            let lo = (s - support_x).ceil() as i32;
            let hi = (s + support_x).floor() as i32;
            for c in 0..3 {
                let mut acc = 0f32;
                let mut wsum = 0f32;
                for idx in lo..=hi {
                    let wt = super::cubic_kernel((s - idx as f32) / fx);
                    let cx = idx.clamp(0, w as i32 - 1) as usize;
                    acc += wt * read(cx, y, c);
                    wsum += wt;
                }
                tmp[(y * nw + x) * 3 + c] = if wsum != 0.0 { acc / wsum } else { 0.0 };
            }
        }
    }

    // 纵向
    let mut out = vec![0u8; nw * nh * 3];
    let ry = h as f32 / nh as f32;
    let fy = ry.max(1.0);
    let support_y = 2.0 * fy;
    for y in 0..nh {
        let s = (y as f32 + 0.5) * ry - 0.5;
        let lo = (s - support_y).ceil() as i32;
        let hi = (s + support_y).floor() as i32;
        for x in 0..nw {
            for c in 0..3 {
                let mut acc = 0f32;
                let mut wsum = 0f32;
                for idx in lo..=hi {
                    let wt = super::cubic_kernel((s - idx as f32) / fy);
                    let cy = idx.clamp(0, h as i32 - 1) as usize;
                    acc += wt * tmp[(cy * nw + x) * 3 + c];
                    wsum += wt;
                }
                let v = if wsum != 0.0 { acc / wsum } else { 0.0 };
                out[(y * nw + x) * 3 + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Rgb { w: nw, h: nh, data: out }
}

/// 裁剪矩形区域并缩放到 `target_h` 高（保持宽高比，宽向上取整）。越界以白填充。
pub fn crop_resize(src: &Rgb, x: i32, y: i32, w: i32, h: i32, target_h: usize) -> Rgb {
    let w = w.max(1) as usize;
    let h = h.max(1) as usize;
    let mut crop = Rgb::new(w, h);
    for yy in 0..h {
        for xx in 0..w {
            let sx = x + xx as i32;
            let sy = y + yy as i32;
            if sx >= 0 && sy >= 0 && (sx as usize) < src.w && (sy as usize) < src.h {
                let p = src.px(sx as usize, sy as usize);
                let i = (yy * w + xx) * 3;
                crop.data[i..i + 3].copy_from_slice(&p);
            }
        }
    }
    let nw = ((target_h as f32) * (w as f32) / (h as f32)).round().max(1.0) as usize;
    resize(&crop, nw.clamp(1, 4096), target_h)
}

/// 检测模型输入：尺寸对齐到 32 的倍数（长边不超过 `limit`），并归一化到 CHW f32。
///
/// 返回 `(数据, 宽, 高, ratio)`，`ratio = 网络宽 / 原图宽`，用于把检测框换算回原图坐标。
pub struct DetInput {
    pub data: Vec<f32>,
    pub w: usize,
    pub h: usize,
    pub ratio: f32,
}

pub fn det_input(img: &Rgb, limit: usize) -> DetInput {
    let max_side = img.w.max(img.h) as f32;
    let scale = if max_side > limit as f32 { limit as f32 / max_side } else { 1.0 };
    let mut rw = ((img.w as f32 * scale / 32.0).round() * 32.0).max(32.0) as usize;
    let mut rh = ((img.h as f32 * scale / 32.0).round() * 32.0).max(32.0) as usize;
    // 极端长条：保证对齐后仍覆盖原图
    if rw < 32 {
        rw = 32;
    }
    if rh < 32 {
        rh = 32;
    }
    let resized = resize(img, rw, rh);
    let ratio = rw as f32 / img.w.max(1) as f32;

    // PaddleOCR det 归一化：x/255，mean=[0.485,0.456,0.406]，std=[0.229,0.224,0.225]
    const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
    const STD: [f32; 3] = [0.229, 0.224, 0.225];
    let n = rw * rh;
    let mut data = vec![0f32; 3 * n];
    for i in 0..n {
        for c in 0..3 {
            let v = resized.data[i * 3 + c] as f32 / 255.0;
            data[c * n + i] = (v - MEAN[c]) / STD[c];
        }
    }
    DetInput { data, w: rw, h: rh, ratio }
}

/// 识别模型输入：固定高 48、按比宽，归一化到 CHW f32（mean=0.5, std=0.5）。
pub fn rec_input(img: &Rgb) -> (Vec<f32>, usize, usize) {
    const H: usize = 48;
    let w = img.w.max(1);
    let h = img.h.max(1);
    let nw = ((H as f32) * (w as f32) / (h as f32)).round().max(1.0) as usize;
    let resized = resize(img, nw.min(4096), H);
    let (rw, rh) = (resized.w, resized.h);
    let n = rw * rh;
    let mut data = vec![0f32; 3 * n];
    for i in 0..n {
        for c in 0..3 {
            let v = resized.data[i * 3 + c] as f32 / 255.0;
            data[c * n + i] = (v - 0.5) / 0.5;
        }
    }
    (data, rw, rh)
}

/// DB 检测后处理：概率图 → 阈值二值 → 8 连通域 → 打分过滤 → unclip 扩张。
///
/// 返回检测网络坐标系下的框 `(x, y, w, h)`（左上角 + 宽高，轴对齐）。
/// `thresh` 二值化阈值、`box_thresh` 框平均概率下限、`unclip_ratio` 扩张系数。
pub fn det_boxes(prob: &[f32], w: usize, h: usize, thresh: f32, box_thresh: f32, unclip_ratio: f32) -> Vec<(i32, i32, i32, i32)> {
    if w == 0 || h == 0 || prob.len() < w * h {
        return Vec::new();
    }
    let mask: Vec<bool> = (0..w * h).map(|i| prob[i] > thresh).collect();
    let comps = connected_components(&mask, w, h);
    let mut out = Vec::new();
    for (x0, y0, x1, y1) in comps {
        let bw = (x1 - x0 + 1) as i32;
        let bh = (y1 - y0 + 1) as i32;
        if bw < 3 || bh < 3 {
            continue; // 噪声/锯齿
        }
        // 框内平均概率作为置信度
        let mut sum = 0f32;
        let mut cnt = 0usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                sum += prob[y * w + x];
                cnt += 1;
            }
        }
        let score = if cnt > 0 { sum / cnt as f32 } else { 0.0 };
        if score < box_thresh {
            continue;
        }
        // unclip：按 DB 常用近似 delta = 面积 × ratio / 周长，向外扩张
        let area = (bw * bh) as f32;
        let perim = 2.0 * (bw + bh) as f32;
        let delta = if perim > 0.0 { area * unclip_ratio / perim } else { 0.0 };
        let nx = (x0 as f32 - delta).max(0.0);
        let ny = (y0 as f32 - delta).max(0.0);
        let nx1 = (x1 as f32 + delta).min((w - 1) as f32);
        let ny1 = (y1 as f32 + delta).min((h - 1) as f32);
        out.push((nx as i32, ny as i32, (nx1 - nx) as i32, (ny1 - ny) as i32));
    }
    out
}

/// 8 连通域标记，返回每个连通域的包围盒 `(x0, y0, x1, y1)`（含端点）。
fn connected_components(mask: &[bool], w: usize, h: usize) -> Vec<(usize, usize, usize, usize)> {
    let mut seen = vec![false; w * h];
    let mut boxes = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..w * h {
        if !mask[start] || seen[start] {
            continue;
        }
        seen[start] = true;
        stack.push(start);
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let ni = ny as usize * w + nx as usize;
                    if mask[ni] && !seen[ni] {
                        seen[ni] = true;
                        stack.push(ni);
                    }
                }
            }
        }
        boxes.push((x0, y0, x1, y1));
    }
    boxes
}

/// 把检测网络坐标系下的框换算回原图坐标并裁剪到边界。
pub fn scale_box(b: (i32, i32, i32, i32), ratio: f32, ow: i32, oh: i32) -> Option<(i32, i32, i32, i32)> {
    if ratio <= 0.0 {
        return None;
    }
    let x = (b.0 as f32 / ratio).floor() as i32;
    let y = (b.1 as f32 / ratio).floor() as i32;
    let w = ((b.2 as f32 / ratio).ceil() as i32).max(1);
    let h = ((b.3 as f32 / ratio).ceil() as i32).max(1);
    let x = x.clamp(0, ow.max(1) - 1);
    let y = y.clamp(0, oh.max(1) - 1);
    let x1 = (x + w).min(ow);
    let y1 = (y + h).min(oh);
    let (w, h) = (x1 - x, y1 - y);
    if w < 2 || h < 2 {
        return None;
    }
    Some((x, y, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgb(w: usize, h: usize, px: [u8; 3]) -> Rgb {
        let mut data = vec![0u8; w * h * 3];
        for c in data.chunks_exact_mut(3) {
            c.copy_from_slice(&px);
        }
        Rgb { w, h, data }
    }

    #[test]
    fn bgra_to_rgb_swaps_channels() {
        let bgra = vec![30u8, 20, 10, 255]; // B,G,R,A
        let rgb = bgra_to_rgb(&bgra, 1, 1);
        assert_eq!(rgb.data, vec![10, 20, 30]);
    }

    #[test]
    fn det_input_aligned_to_32_and_normalized_range() {
        let img = solid_rgb(100, 50, [255, 255, 255]);
        let d = det_input(&img, 960);
        assert_eq!(d.w % 32, 0);
        assert_eq!(d.h % 32, 0);
        assert_eq!(d.data.len(), 3 * d.w * d.h);
        // 纯白归一化后应落在 (1-mean)/std 附近，量级个位
        assert!(d.data.iter().all(|v| v.abs() < 5.0));
    }

    #[test]
    fn rec_input_height_is_48() {
        let img = solid_rgb(200, 50, [0, 0, 0]);
        let (data, w, h) = rec_input(&img);
        assert_eq!(h, 48);
        assert_eq!(w, (48.0 * 200.0 / 50.0f32).round() as usize);
        assert_eq!(data.len(), 3 * w * h);
    }

    #[test]
    fn det_boxes_finds_single_blob() {
        // 64×64 概率图，中间 20×10 的矩形置高概率
        let (w, h) = (64usize, 64usize);
        let mut prob = vec![0f32; w * h];
        for y in 20..30 {
            for x in 20..40 {
                prob[y * w + x] = 0.9;
            }
        }
        let boxes = det_boxes(&prob, w, h, 0.3, 0.6, 1.5);
        assert_eq!(boxes.len(), 1, "应只检出一个框，实际 {boxes:?}");
        let (bx, by, bw, bh) = boxes[0];
        // 扩张后应覆盖原区域且略大
        assert!(bx <= 20 && by <= 20);
        assert!(bx + bw >= 40 && by + bh >= 30);
    }

    #[test]
    fn det_boxes_drops_low_score_and_small() {
        let (w, h) = (32usize, 32usize);
        let mut prob = vec![0f32; w * h];
        // 单像素噪声：太小
        prob[5 * w + 5] = 1.0;
        // 大但低分区域
        for y in 10..28 {
            for x in 10..28 {
                prob[y * w + x] = 0.35; // 高于 thresh(0.3) 但低于 box_thresh(0.6)
            }
        }
        let boxes = det_boxes(&prob, w, h, 0.3, 0.6, 1.5);
        assert!(boxes.is_empty(), "低分/过小框应被过滤，实际 {boxes:?}");
    }

    #[test]
    fn scale_box_maps_back_and_clamps() {
        // ratio=2（网络坐标翻倍），框 (20,10,40,20) → 原图 (10,5,20,10)
        assert_eq!(scale_box((20, 10, 40, 20), 2.0, 100, 100), Some((10, 5, 20, 10)));
        // 越界应被裁剪
        let b = scale_box((90, 90, 40, 40), 1.0, 100, 100).unwrap();
        assert!(b.0 + b.2 <= 100 && b.1 + b.3 <= 100);
        // ratio 非法 → None
        assert_eq!(scale_box((1, 1, 5, 5), 0.0, 10, 10), None);
    }
}
