//! OCR：识别管线 + 可插拔引擎（本地 ONNX 优先，系统 WinRT 回退）。
//!
//! 管线分两条路径，取决于引擎是否「自包含」（见 [`engine::OcrEngineImpl::self_contained`]）：
//! - **自包含引擎（内置 ONNX）**：引擎自带检测 + 识别 + 置信度，管线直接取行结构做版面聚类
//!   （[`layout::assemble`]），再按配置做标点全/半角与去空格；
//! - **非自包含引擎（系统 WinRT）**：引擎只给无置信度的纯文本，走上采样 → 多变体
//!   （原图 / Otsu 二值化 / 深色反相）→ 启发式评分取优的老路。
//!
//! 引擎挑选见 [`engine::create`]：内置 ONNX（`onnxruntime.dll` + `model/*.onnx` 齐备）优先，
//! 缺失时回退系统 OCR（该回退需 `winrt-ocr` feature，默认关闭）。
//!
//! 本模块不含 GUI，可在任意后台线程调用。

mod engine;
mod layout;
mod onnx;
mod onnx_image;
// 系统 WinRT 回退引擎：仅 `winrt-ocr` feature 开启时编译（默认关闭，避免进程
// 静态导入整条 WinRT 运行库，见 Cargo.toml 该 feature 的说明）。
#[cfg(feature = "winrt-ocr")]
mod winrt;

use engine::OcrEngineImpl;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{AppError, Result};
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// 识别进行中标志（并发触发去抖：连点 N 次只会排队跑 N 次完整推理，
/// 而只有最后一次的结果有意义）。
static OCR_BUSY: AtomicBool = AtomicBool::new(false);

/// 后台识别 → 文字写入剪贴板 → 交主窗口弹 Toast（**不弹结果窗口**）。
///
/// 工具条「复制识别到的文字」与贴图右键菜单「识别文字 (OCR)」**共用同一条路径**，
/// 两处 OCR 的功能与提示因此必然一致。
/// 识别必须离开 GUI 线程（WinRT 异步调用不可阻塞消息循环），故开后台线程，
/// 完成后用 `WM_APP_OCR_DONE` 把结果码投递回主窗口展示提示。
pub fn recognize_to_clipboard_async(image: Arc<[u8]>, w: i32, h: i32) {
    // 识别中去抖：忽略重复触发
    if OCR_BUSY.swap(true, Ordering::SeqCst) {
        crate::diag::log("[ocr] 上一次识别尚未完成，忽略本次触发");
        return;
    }
    // HWND 非 Send：跨线程以地址（isize）传递，线程内还原
    let main_addr = crate::tray::main_hwnd().0 as isize;
    std::thread::spawn(move || {
        let code = match recognize(&image, w, h) {
            Ok(text) if !text.trim().is_empty() => {
                if crate::clipboard::copy_text(&text).is_ok() {
                    crate::tray::OCR_DONE_COPIED
                } else {
                    crate::tray::OCR_DONE_FAIL
                }
            }
            Ok(_) => crate::tray::OCR_DONE_EMPTY,
            Err(e) => {
                crate::diag::log(&format!("[ocr] 识别失败: {e}"));
                crate::tray::OCR_DONE_FAIL
            }
        };
        OCR_BUSY.store(false, Ordering::SeqCst);
        if main_addr != 0 {
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
            unsafe {
                let main = HWND(main_addr as *mut core::ffi::c_void);
                let _ = PostMessageW(main, crate::tray::WM_APP_OCR_DONE, WPARAM(code), LPARAM(0));
            }
        }
    });
}

/// 对 BGRA（自上而下）图像执行 OCR，返回识别文本。
/// 须在非 GUI 线程调用（本函数自行初始化 COM）。
pub fn recognize(bgra: &[u8], w: i32, h: i32) -> Result<String> {
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // S_OK（本线程首次初始化）才配对 CoUninitialize；S_FALSE 表示调用方已初始化，
    // 此时 uninit 会抵消对方的计数，导致同线程后续 COM 调用意外失效
    if hr == S_OK {
        let r = recognize_inner(bgra, w, h);
        unsafe { CoUninitialize() };
        r
    } else if hr == S_FALSE {
        recognize_inner(bgra, w, h)
    } else {
        Err(AppError::Ocr(format!("CoInitializeEx 失败: {hr}")))
    }
}

fn recognize_inner(bgra: &[u8], w: i32, h: i32) -> Result<String> {
    if w <= 0 || h <= 0 {
        return Err(AppError::Ocr("图像尺寸无效".into()));
    }
    if bgra.len() < (w as usize) * (h as usize) * 4 {
        return Err(AppError::Ocr("图像数据不完整".into()));
    }
    let engine = engine::create().ok_or_else(|| {
        AppError::Ocr(
            "未找到可用的 OCR 引擎。内置引擎需要与 PixShot.exe 同目录的 onnxruntime.dll 与 \
             model/{det,rec}.onnx；回退引擎需要系统安装中文（简体）或英文语言包并包含\
             “文字识别”可选功能。"
                .into(),
        )
    })?;
    crate::diag::log(&format!(
        "[ocr] 引擎={} 上限={}px",
        engine.id(),
        engine.max_dimension()
    ));
    let cfg = crate::config::get();

    // 自包含引擎（ONNX）：自带检测+识别+版面，直接取行结构做聚类与后处理，不走多候选启发式。
    if engine.self_contained() {
        // 小图先整体上采样：DB 检测对 <10px 字高召回差（原 WinRT 路径同样上采样）。
        // 仅长边 < 阈值时触发、上限 3×；正常截图不受影响（超限由 det_input 按 det_limit 收缩）。
        const UPSAMPLE_FLOOR: i32 = 640;
        const UPSAMPLE_MAX: f32 = 3.0;
        let longest = w.max(h);
        let upscaled: Option<(Vec<u8>, i32, i32)> = if longest < UPSAMPLE_FLOOR {
            let scale = (UPSAMPLE_FLOOR as f32 / longest as f32).min(UPSAMPLE_MAX);
            let sw = ((w as f32 * scale).round() as i32).max(1);
            let sh = ((h as f32 * scale).round() as i32).max(1);
            crate::diag::log(&format!("[ocr] 小图上采样 {w}×{h} -> {sw}×{sh}"));
            Some((resize_bicubic(bgra, w as usize, h as usize, sw as usize, sh as usize), sw, sh))
        } else {
            None
        };
        let (data, w, h) = match &upscaled {
            Some((v, sw, sh)) => (v.as_slice(), *sw, *sh),
            None => (bgra, w, h),
        };
        let lines = engine.recognize_lines(data, w, h)?;
        let total = lines.len();
        let kept: Vec<engine::TextLine> = lines
            .into_iter()
            .filter(|l| l.conf >= cfg.ocr_confidence)
            .collect();
        let mean_conf = if kept.is_empty() {
            0.0
        } else {
            kept.iter().map(|l| l.conf).sum::<f32>() / kept.len() as f32
        };
        crate::diag::log(&format!(
            "[ocr] ONNX 检测行={total} 保留={} 平均置信度={mean_conf:.2} 阈值={:.2}",
            kept.len(),
            cfg.ocr_confidence
        ));
        if kept.is_empty() {
            return Ok(String::new());
        }
        let text = layout::assemble(&kept, cfg.ocr_layout);
        return Ok(finish(&text, &cfg));
    }

    // 引擎对 20–40px 的字高最准：小图先放大，超限大图再等比缩小。
    let max = engine.max_dimension();
    let (sw, sh) = target_size(w, h, max, cfg.ocr_upsample_target, cfg.ocr_max_upscale);
    let base: Vec<u8> = if sw != w || sh != h {
        resize_bicubic(bgra, w as usize, h as usize, sw as usize, sh as usize)
    } else {
        bgra.to_vec()
    };

    // 多变体跑分取优：引擎不暴露置信度，就用「有意义字符占比 × log(长度)」这套启发式
    // 在几幅候选图之间挑最优。base 已足够干净时跳过其余候选，省掉多余的一次识别。
    let dark = mean_luma(&base) < cfg.ocr_dark_luma;
    let mut best: Option<(f32, String)> = None;
    try_candidate(engine.as_ref(), "base", &base, sw, sh, &mut best);
    let confident = best.as_ref().map_or(false, |(_, t)| is_confident(t));
    if !confident {
        try_candidate(engine.as_ref(), "binarized", &binarize_otsu(&base), sw, sh, &mut best);
        if dark {
            try_candidate(engine.as_ref(), "inverted", &invert(&base), sw, sh, &mut best);
        }
    }
    best.map(|(_, t)| finish(&t, &cfg))
        .ok_or_else(|| AppError::Ocr("所有候选识别均失败".into()))
}

/// 统一的文本后处理：按配置做标点全/半角转换与去空格，并去掉首尾空白。
fn finish(text: &str, cfg: &crate::config::Config) -> String {
    let t = layout::apply_punctuation(text, cfg.ocr_punctuation);
    let t = if cfg.ocr_remove_spaces {
        layout::remove_spaces(&t)
    } else {
        t
    };
    t.trim().to_string()
}

/// 对一幅候选图跑一次识别，并把结果按启发式评分并入 `best`（更高分才替换）。
///
/// 单个候选失败只记日志，不影响其它候选——候选中任何一幅成功即可返回结果。
fn try_candidate(
    engine: &dyn OcrEngineImpl,
    name: &str,
    data: &[u8],
    w: i32,
    h: i32,
    best: &mut Option<(f32, String)>,
) {
    match engine.recognize(data, w, h) {
        Ok(text) => {
            let score = score_text(&text);
            crate::diag::log(&format!(
                "[ocr] 候选 {name}: score={score:.2} 字符数={}",
                text.chars().count()
            ));
            if best.as_ref().map_or(true, |(b, _)| score > *b) {
                *best = Some((score, text));
            }
        }
        Err(e) => crate::diag::log(&format!("[ocr] 候选 {name} 识别失败: {e}")),
    }
}

/// 小图放大的目标最长边与最大放大倍数取自配置
/// （config.ini [ocr] upsample_target / max_upscale）。
///
/// 依据引擎上限决定 OCR 前的目标尺寸：小图放大、超限大图等比缩小、其余原样。
///
/// 放大分支的缩放系数被 `upsample_target / longest` 与 `max_upscale` 双重约束，
/// 因此结果最长边不超过目标值；仅在引擎上限低于目标值时才需要兜底收缩。
fn target_size(w: i32, h: i32, max: i32, upsample_target: i32, max_upscale: f32) -> (i32, i32) {
    if w <= 0 || h <= 0 {
        return (w.max(1), h.max(1));
    }
    let longest = w.max(h) as f32;
    let max = max.max(1) as f32;
    let target = upsample_target.max(1) as f32;
    let mut scale = if longest > max {
        max / longest
    } else if longest < target {
        (target / longest).min(max_upscale.max(1.0))
    } else {
        1.0
    };
    if longest * scale > max {
        scale = max / longest;
    }
    let nw = ((w as f32 * scale).round() as i32).max(1);
    let nh = ((h as f32 * scale).round() as i32).max(1);
    (nw, nh)
}

/// Catmull-Rom 双三次核：|x|≤1 为 1.5|x|³-2.5|x|²+1，1<|x|<2 为 -0.5|x|³+2.5|x|²-4|x|+2。
fn cubic_kernel(x: f32) -> f32 {
    let x = x.abs();
    if x <= 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

/// 高质量可分离双三次缩放（先横向再纵向），用于 OCR 前的上/下采样。
///
/// 放大时双三次能生成真实的中间灰度（最近邻只会复制像素，放大成马赛克）；
/// 缩小时按缩放比**加宽核的支撑域**做低通，等价于带权重的区域平均，避免抽点采样
/// 把细笔画整根丢掉。两者都减少锯齿对字符笔画的破坏。输入/输出均为 BGRA、自上而下、
/// 行距 = width×4；四个通道（含 alpha）一并插值，以像素中心对齐并钳制采样坐标。
fn resize_bicubic(bgra: &[u8], w: usize, h: usize, nw: usize, nh: usize) -> Vec<u8> {
    resize_bicubic_ch::<4>(bgra, w, h, nw, nh, 0)
}

/// 可分离双三次缩放内核（通道数泛型：BGRA 4 通道与 RGB 3 通道共用，
/// 见 `onnx_image::resize`）。越界采样回填 `fill`（BGRA 用 0，RGB 用 255）。
pub(crate) fn resize_bicubic_ch<const C: usize>(
    data: &[u8],
    w: usize,
    h: usize,
    nw: usize,
    nh: usize,
    fill: u8,
) -> Vec<u8> {
    if w == 0 || h == 0 || nw == 0 || nh == 0 {
        return vec![fill; nw * nh * C];
    }
    if nw == w && nh == h {
        let mut out = vec![fill; w * h * C];
        let n = out.len().min(data.len());
        out[..n].copy_from_slice(&data[..n]);
        return out;
    }

    let read = |x: usize, y: usize, c: usize| -> f32 {
        let i = (y * w + x) * C + c;
        if i < data.len() {
            data[i] as f32
        } else {
            fill as f32
        }
    };

    // 横向：w → nw，中间结果用 f32 承接，避免纵向再做一次 8 位量化
    let mut tmp = vec![0f32; nw * h * C];
    let rx = w as f32 / nw as f32;
    let fx = rx.max(1.0); // 缩小 → 核支撑域按此加宽
    let support_x = 2.0 * fx;
    for y in 0..h {
        for x in 0..nw {
            let src = (x as f32 + 0.5) * rx - 0.5;
            let d = (y * nw + x) * C;
            let lo = (src - support_x).ceil() as i32;
            let hi = (src + support_x).floor() as i32;
            for c in 0..C {
                let mut acc = 0f32;
                let mut wsum = 0f32;
                for idx in lo..=hi {
                    let wt = cubic_kernel((src - idx as f32) / fx);
                    let cx = idx.clamp(0, w as i32 - 1) as usize;
                    acc += wt * read(cx, y, c);
                    wsum += wt;
                }
                tmp[d + c] = if wsum != 0.0 { acc / wsum } else { 0.0 };
            }
        }
    }

    // 纵向：h → nh，量化回 8 位
    let mut out = vec![fill; nw * nh * C];
    let ry = h as f32 / nh as f32;
    let fy = ry.max(1.0);
    let support_y = 2.0 * fy;
    for y in 0..nh {
        let src = (y as f32 + 0.5) * ry - 0.5;
        let lo = (src - support_y).ceil() as i32;
        let hi = (src + support_y).floor() as i32;
        for x in 0..nw {
            let d = (y * nw + x) * C;
            for c in 0..C {
                let mut acc = 0f32;
                let mut wsum = 0f32;
                for idx in lo..=hi {
                    let wt = cubic_kernel((src - idx as f32) / fy);
                    let cy = idx.clamp(0, h as i32 - 1) as usize;
                    acc += wt * tmp[(cy * nw + x) * C + c];
                    wsum += wt;
                }
                let v = if wsum != 0.0 { acc / wsum } else { 0.0 };
                out[d + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// 判定「深色背景」的平均亮度阈值（0–255）。深色主题（IDE/终端）常见均值 30–60，
/// 白底文档 200 以上，取中间偏暗处作为分界。

/// 按 Rec.601 计算整幅图平均亮度（0–255）。输入 BGRA。
fn mean_luma(bgra: &[u8]) -> f32 {
    let mut sum = 0f64;
    let mut n = 0u64;
    for px in bgra.chunks_exact(4) {
        sum += luma(px[2], px[1], px[0]);
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64) as f32
    }
}

/// 单像素亮度（R/G/B 顺序，Rec.601）。
fn luma(r: u8, g: u8, b: u8) -> f64 {
    0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64
}

/// 反相 RGB（alpha 保持不透明）。用于白字黑底的深色截图——WinRT 对这类图识别较差。
fn invert(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        px[0] = 255 - px[0];
        px[1] = 255 - px[1];
        px[2] = 255 - px[2];
        px[3] = 255;
    }
    out
}

/// Otsu 全局阈值：在直方图上寻找类间方差最大的分割点。
fn otsu_threshold(hist: &[u32; 256], total: u32) -> u8 {
    if total == 0 {
        return 127;
    }
    let sum_all: f64 = (0..256).map(|i| i as f64 * hist[i] as f64).sum();
    let mut sum_b = 0f64;
    let mut w_b = 0u32;
    let mut max_var = -1f64;
    let mut thr = 127u8;
    for t in 0..256 {
        w_b += hist[t];
        if w_b == 0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0 {
            break;
        }
        sum_b += t as f64 * hist[t] as f64;
        let m_b = sum_b / w_b as f64;
        let m_f = (sum_all - sum_b) / w_f as f64;
        let var = w_b as f64 * w_f as f64 * (m_b - m_f) * (m_b - m_f);
        if var > max_var {
            max_var = var;
            thr = t as u8;
        }
    }
    thr
}

/// 灰度化 + Otsu 二值化，并把背景统一成白色（深色背景自动反相）。
///
/// 对低对比度、彩色/渐变背景的截图，二值化能把文字从背景里「拔」出来；统一成白底黑字
/// 也顺应引擎偏好。输出 BGRA，只有 0/255 两种灰度。
fn binarize_otsu(bgra: &[u8]) -> Vec<u8> {
    let mut hist = [0u32; 256];
    let mut total = 0u32;
    for px in bgra.chunks_exact(4) {
        let y = luma(px[2], px[1], px[0]).round() as usize;
        hist[y.min(255)] += 1;
        total += 1;
    }
    let thr = otsu_threshold(&hist, total) as usize;
    // 暗色像素占多数 ⇒ 背景是暗的（白字黑底），需反相输出为白底黑字
    let dark_count: u32 = hist[..=thr].iter().sum();
    let dark_is_bg = dark_count * 2 > total;

    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        let y = luma(px[2], px[1], px[0]).round() as usize;
        let mut v: u8 = if y.min(255) <= thr { 0 } else { 255 };
        if dark_is_bg {
            v = 255 - v;
        }
        px[0] = v;
        px[1] = v;
        px[2] = v;
        px[3] = 255;
    }
    out
}

/// 是否为 CJK（中日韩）表意文字/假名/谚文。用于清理引擎在汉字间插入的假空格。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // 平假名 / 片假名
        | 0x3400..=0x4DBF // 汉字扩展 A
        | 0x4E00..=0x9FFF // 汉字基本区
        | 0xF900..=0xFAFF // 兼容汉字
        | 0xAC00..=0xD7AF // 谚文音节
        | 0x20000..=0x2FA1F // 汉字扩展 B 及以后
    )
}

/// 删除夹在两个 CJK 字符之间的空格/制表符（引擎常把中文按「词」拆开并加空格）。
/// 拉丁文之间的空格、以及换行一律保留。
fn collapse_cjk_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut pending_space = false;
    for c in text.chars() {
        if c == ' ' || c == '\t' {
            pending_space = true;
            continue;
        }
        if pending_space {
            let drop = c == '\n' || (prev.map_or(false, is_cjk) && is_cjk(c));
            if !drop {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// 该字符是否算「有意义的正文内容」——字母/数字（含 CJK）与常见标点。
/// 用来把乱码符号从评分里剔除。
fn is_meaningful(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '.' | ',' | ':' | ';' | '?' | '!' | '-' | '_' | '/' | '\\' | '+' | '=' | '%'
                | '@' | '#' | '&' | '(' | ')' | '[' | ']' | '{' | '}' | '\'' | '"'
                | '、' | '。' | '，' | '：' | '；' | '？' | '！' | '（' | '）'
                | '《' | '》' | '“' | '”' | '‘' | '’' | '—' | '…' | '·'
        )
}

/// 对识别结果打分：有意义字符占比 × log(1 + 有意义字符数)。
///
/// 既惩罚乱码（占比低），又用对数压缩长度分量——避免「又长又脏」的结果仅靠数量取胜。
/// 候选之间只比较相对高低，绝对值无意义。
fn score_text(text: &str) -> f32 {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.is_empty() {
        return 0.0;
    }
    let total = chars.len() as f32;
    let good = chars.iter().filter(|c| is_meaningful(**c)).count() as f32;
    if good == 0.0 {
        return 0.0;
    }
    (good / total) * (1.0 + good.ln())
}

/// base 候选是否已经「足够可信」，可信则不再跑其余候选。
/// 判据：至少 8 个有意义字符，且有意义字符占比 ≥ 0.95。
fn is_confident(text: &str) -> bool {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 8 {
        return false;
    }
    let good = chars.iter().filter(|c| is_meaningful(**c)).count();
    good as f32 / chars.len() as f32 >= 0.95
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯色 BGRA 图（每像素同值），用于验证插值不引入颜色偏差。
    fn solid(w: usize, h: usize, px: [u8; 4]) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for c in v.chunks_exact_mut(4) {
            c.copy_from_slice(&px);
        }
        v
    }

    // ---- target_size：缩放决策 ----

    #[test]
    fn target_size_upscales_small_images_to_target() {
        // 1000×800：放大到最长边 2400（2.4×，未触及 3× 上限）
        let (nw, nh) = target_size(1000, 800, 4096, 2400, 3.0);
        assert_eq!((nw, nh), (2400, 1920));
    }

    #[test]
    fn target_size_caps_upscale_factor() {
        // 200×150：3× 上限生效，而不是拉到 2400（那会是 12×）
        let (nw, nh) = target_size(200, 150, 4096, 2400, 3.0);
        assert_eq!((nw, nh), (600, 450));
    }

    #[test]
    fn target_size_shrinks_oversized() {
        // 8000×4000 超上限：等比缩到最长边 4096
        let (nw, nh) = target_size(8000, 4000, 4096, 2400, 3.0);
        assert_eq!((nw, nh), (4096, 2048));
    }

    #[test]
    fn target_size_keeps_mid_size_unchanged() {
        // 3000×2000 已落在 [目标, 上限] 区间：不缩放
        assert_eq!(target_size(3000, 2000, 4096, 2400, 3.0), (3000, 2000));
    }

    #[test]
    fn target_size_never_exceeds_max() {
        for (w, h) in [(10, 10), (500, 2000), (1365, 1365), (4000, 100), (9000, 9000)] {
            let (nw, nh) = target_size(w, h, 4096, 2400, 3.0);
            assert!(nw <= 4096 && nh <= 4096, "{w}×{h} → {nw}×{nh} 超出上限");
            assert!(nw >= 1 && nh >= 1);
        }
    }

    #[test]
    fn target_size_is_robust_on_degenerate_input() {
        assert_eq!(target_size(0, 0, 4096, 2400, 3.0), (1, 1));
        assert_eq!(target_size(-5, 10, 4096, 2400, 3.0), (1, 10));
        // 引擎上限异常（0/负）时不得 panic
        let (nw, nh) = target_size(100, 50, 0, 2400, 3.0);
        assert!(nw >= 1 && nh >= 1);
    }

    // ---- resize_bicubic：缩放本身 ----

    #[test]
    fn resize_same_size_is_copy() {
        let src = solid(4, 4, [10, 20, 30, 255]);
        assert_eq!(resize_bicubic(&src, 4, 4, 4, 4), src);
    }

    #[test]
    fn resize_output_length_is_exact() {
        let src = solid(8, 8, [1, 2, 3, 255]);
        assert_eq!(resize_bicubic(&src, 8, 8, 20, 6).len(), 20 * 6 * 4);
    }

    #[test]
    fn resize_upscale_preserves_flat_color() {
        // 纯色放大：加权插值后仍应为同一颜色（含 alpha），即不产生伪彩/暗边
        let src = solid(4, 4, [120, 60, 200, 255]);
        let out = resize_bicubic(&src, 4, 4, 16, 16);
        for c in out.chunks_exact(4) {
            assert_eq!(c, [120, 60, 200, 255], "纯色放大出现了颜色偏差");
        }
    }

    #[test]
    fn resize_downscale_preserves_flat_color() {
        let src = solid(16, 16, [7, 8, 9, 255]);
        let out = resize_bicubic(&src, 16, 16, 4, 4);
        for c in out.chunks_exact(4) {
            assert_eq!(c, [7, 8, 9, 255], "纯色缩小出现了颜色偏差");
        }
    }

    #[test]
    fn resize_upscale_interpolates_between_pixels() {
        // 左黑右白的 2×1 图放大到 8 宽：应出现介于两者之间的灰度像素
        // （最近邻只会给出 0/255，这正是我们要避免的锯齿）
        let mut src = vec![0u8; 2 * 1 * 4];
        src[0..4].copy_from_slice(&[0, 0, 0, 255]); // 左：黑
        src[4..8].copy_from_slice(&[255, 255, 255, 255]); // 右：白
        let out = resize_bicubic(&src, 2, 1, 8, 1);
        let vals: Vec<u8> = out.chunks_exact(4).map(|c| c[0]).collect();
        assert!(
            vals.iter().any(|&v| v > 0 && v < 255),
            "双三次放大应产生中间灰度，实际 {vals:?}"
        );
        assert!(vals[0] < *vals.last().unwrap(), "左端应比右端暗，方向正确");
    }

    #[test]
    fn resize_downscale_averages_step_edge() {
        // 上半黑下半白的 4×4 缩到 2×2：跨越阶跃的像素应被均值化成中间灰，
        // 而不是整块取黑或白
        let mut src = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                let v = if y < 2 { 0 } else { 255 };
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let out = resize_bicubic(&src, 4, 4, 2, 2);
        let vals: Vec<u8> = out.chunks_exact(4).map(|c| c[0]).collect();
        assert!(
            vals.iter().any(|&v| v > 0 && v < 255),
            "缩小的阶跃边缘应被均值化，实际 {vals:?}"
        );
    }

    #[test]
    fn resize_never_panics_on_degenerate_input() {
        // 0 尺寸 / 数据不足时不得 panic（OCR 链路的边界保险）
        assert!(resize_bicubic(&[], 0, 0, 0, 0).is_empty());
        assert_eq!(resize_bicubic(&[0u8; 16], 2, 2, 1, 1).len(), 4);
        // 声明尺寸大于实际数据：越界读应返回 0 而非 panic
        assert_eq!(resize_bicubic(&[0u8; 4], 2, 2, 8, 8).len(), 8 * 8 * 4);
    }

    #[test]
    fn cubic_kernel_is_an_interpolating_filter() {
        // 核函数在整数点应为 1（原样保留）、在 ±1 处为 0、|x|≥2 处为 0
        assert!((cubic_kernel(0.0) - 1.0).abs() < 1e-6);
        assert!(cubic_kernel(1.0).abs() < 1e-6);
        assert!(cubic_kernel(-1.0).abs() < 1e-6);
        assert_eq!(cubic_kernel(2.0), 0.0);
        assert_eq!(cubic_kernel(-3.0), 0.0);
    }

    // ---- 预处理：亮度 / 反相 / 二值化 ----

    #[test]
    fn mean_luma_covers_extremes() {
        assert!((mean_luma(&solid(2, 2, [0, 0, 0, 255])) - 0.0).abs() < 0.5);
        assert!((mean_luma(&solid(2, 2, [255, 255, 255, 255])) - 255.0).abs() < 0.5);
        // 空输入不得除零 panic
        assert_eq!(mean_luma(&[]), 0.0);
    }

    #[test]
    fn invert_flips_rgb_keeps_opaque_alpha() {
        let src = solid(1, 1, [10, 20, 30, 255]);
        let out = invert(&src);
        assert_eq!(&out[..], &[245, 235, 225, 255]);
        // 反相两次应还原
        assert_eq!(invert(&out), src);
    }

    #[test]
    fn otsu_threshold_splits_bimodal_histogram() {
        // 双峰：100 个像素在 20，100 个在 220 → 阈值应落在两峰之间
        let mut hist = [0u32; 256];
        hist[20] = 100;
        hist[220] = 100;
        let thr = otsu_threshold(&hist, 200);
        assert!(thr >= 20 && thr < 220, "阈值 {thr} 未落在两峰之间");
    }

    #[test]
    fn otsu_threshold_handles_empty() {
        assert_eq!(otsu_threshold(&[0u32; 256], 0), 127);
    }

    #[test]
    fn binarize_turns_light_background_into_white() {
        // 白底（灰 240）黑字（灰 10）
        let mut src = solid(4, 1, [240, 240, 240, 255]);
        src[0..4].copy_from_slice(&[10, 10, 10, 255]);
        let out = binarize_otsu(&src);
        assert_eq!(out[0], 0, "暗色文字应变为黑");
        assert_eq!(out[4], 255, "亮色背景应变为白");
    }

    #[test]
    fn binarize_unifies_dark_background_to_white_too() {
        // 黑底（灰 15）白字（灰 230）：输出应反相为白底黑字
        let mut src = solid(4, 1, [15, 15, 15, 255]);
        src[0..4].copy_from_slice(&[230, 230, 230, 255]);
        let out = binarize_otsu(&src);
        assert_eq!(out[0], 0, "亮色文字在暗底上应转为黑");
        assert_eq!(out[4], 255, "暗色背景应转为白");
    }

    #[test]
    fn binarize_output_is_pure_black_or_white() {
        let mut src = vec![0u8; 6 * 1 * 4];
        for (i, v) in [0u8, 40, 90, 150, 200, 255].iter().enumerate() {
            src[i * 4..i * 4 + 4].copy_from_slice(&[*v, *v, *v, 255]);
        }
        for c in binarize_otsu(&src).chunks_exact(4) {
            assert!(c[0] == 0 || c[0] == 255, "二值化输出应只有 0/255，实际 {}", c[0]);
            assert_eq!(c[3], 255);
        }
    }

    // ---- 文本清理与评分 ----

    #[test]
    fn is_cjk_recognizes_scripts() {
        assert!(is_cjk('中'));
        assert!(is_cjk('あ'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('A'));
        assert!(!is_cjk('1'));
        assert!(!is_cjk(','));
    }

    #[test]
    fn collapse_drops_spaces_between_cjk_chars() {
        assert_eq!(collapse_cjk_spaces("你 好 世 界"), "你好世界");
        assert_eq!(collapse_cjk_spaces("中 文 混 排"), "中文混排");
    }

    #[test]
    fn collapse_keeps_latin_and_cjk_latin_spaces() {
        assert_eq!(collapse_cjk_spaces("hello world"), "hello world");
        // CJK 与拉丁之间、以及拉丁与 CJK 之间的空格保留
        assert_eq!(collapse_cjk_spaces("中 A 文"), "中 A 文");
    }

    #[test]
    fn collapse_keeps_newlines_and_drops_trailing_space() {
        assert_eq!(collapse_cjk_spaces("第一行\n第二行"), "第一行\n第二行");
        // 换行前的假空格应被丢弃
        assert_eq!(collapse_cjk_spaces("你好 \n世界"), "你好\n世界");
        assert_eq!(collapse_cjk_spaces("尾部 "), "尾部");
    }

    #[test]
    fn score_prefers_clean_text_over_noise() {
        let clean = "这是一段正常的识别结果";
        let noisy = "这◊§一段※正‖常◊的识※别结§果";
        assert!(
            score_text(clean) > score_text(noisy),
            "干净文本应比同长度的乱码得分高"
        );
        assert_eq!(score_text(""), 0.0);
        assert_eq!(score_text("   \n\t "), 0.0);
        // 纯乱码没有有意义字符 → 0 分
        assert_eq!(score_text("◊§※‖"), 0.0);
    }

    #[test]
    fn score_rewards_more_content_all_else_equal() {
        assert!(score_text("识别结果很长的一段文字") > score_text("识别结果"));
    }

    #[test]
    fn is_confident_gates_on_length_and_purity() {
        assert!(is_confident("这是一段足够长的干净中文识别结果"));
        assert!(!is_confident("短"), "过短不算可信");
        assert!(!is_confident("这◊§一段※正‖常◊的识※别结§果"), "含乱码不算可信");
    }
}
