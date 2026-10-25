//! 本地 ONNX 推理引擎：DB 文本检测 + CTC 文本识别（PaddleOCR PP-OCRv4）。
//!
//! 与系统 WinRT 引擎不同，本引擎**自带检测与识别模型**，可给出每行的真实置信度与文本框，
//! 因此向管线标记为「自包含」（[`OcrEngineImpl::self_contained`]）：管线不再上采样与多候选，
//! 直接取行结构做版面聚类与后处理。
//!
//! 模型与运行时随包分发：
//! - `onnxruntime.dll`：优先 `<exe目录>/onnxruntime.dll`，开发期回退 `vendor/onnxruntime/`；
//! - `model/{det,rec}.onnx`：优先 `<exe目录>/model/`，开发期回退仓库 `model/`；
//! - 字符字典：内嵌在 `rec.onnx` 的 ONNX metadata（`character` 键）里，无需单独词典文件。
//!
//! 模型来源：PaddleOCR PP-OCRv4（Apache-2.0），经 RapidOCR 转出 ONNX；见仓库 `NOTICE`。

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use ort::session::Session;
use ort::value::Tensor;

use super::engine::{OcrEngineImpl, TextLine};
use super::onnx_image as im;
use crate::error::{AppError, Result};

/// 检测输入长边上限（PaddleOCR 常用 960）。
const DET_LIMIT: usize = 960;
/// 概率图二值化阈值。
const DET_THRESH: f32 = 0.3;
/// 文本框平均概率下限。
const BOX_THRESH: f32 = 0.6;
/// DB unclip 扩张系数。
const UNCLIP_RATIO: f32 = 1.5;

// ─────────────────────────────────────────────────────────────────
// 路径解析与运行时初始化
// ─────────────────────────────────────────────────────────────────

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(|p| p.to_path_buf())
}

/// 依次在「exe 同目录」与「仓库根（开发期）」下查找文件。
fn find_file(rel_exe: &str, rel_repo: &str) -> Option<PathBuf> {
    find_file_in(exe_dir().as_deref(), rel_exe, rel_repo)
}

/// [`find_file`] 的可测内核：给定 exe 目录，优先命中其中的文件，否则回退仓库路径。
fn find_file_in(exe: Option<&std::path::Path>, rel_exe: &str, rel_repo: &str) -> Option<PathBuf> {
    if let Some(d) = exe {
        let p = d.join(rel_exe);
        if p.is_file() {
            return Some(p);
        }
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel_repo);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// 动态加载 onnxruntime.dll（进程内只允许一次）。
fn init_runtime() -> Result<()> {
    static ONCE: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let r = ONCE.get_or_init(|| {
        let dll = find_file("onnxruntime.dll", "vendor/onnxruntime/onnxruntime.dll").ok_or_else(|| {
            "未找到 onnxruntime.dll（应与 pixshot.exe 同目录，或位于仓库 vendor/onnxruntime/）".to_string()
        })?;
        ort::init_from(&dll)
            .map_err(|e| format!("加载 {} 失败: {e}", dll.display()))?
            .with_name("pixshot")
            .commit();
        Ok(())
    });
    r.as_ref().map(|_| ()).map_err(|e| AppError::Ocr(e.clone()))
}

// ─────────────────────────────────────────────────────────────────
// 模型缓存
// ─────────────────────────────────────────────────────────────────

struct Models {
    det: Mutex<Session>,
    rec: Mutex<Session>,
    dict: Vec<String>,
}

fn load_session(path: &PathBuf) -> std::result::Result<Session, String> {
    let mut b = Session::builder().map_err(|e| format!("创建会话失败: {e}"))?;
    b = b.with_intra_threads(num_cpus()).map_err(|e| format!("设置线程数失败: {e}"))?;
    b.commit_from_file(path).map_err(|e| format!("加载模型 {} 失败: {e}", path.display()))
}

/// 可用的 CPU 线程数（离线，取逻辑核心数，上限 8 以免大图过订阅）。
fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
}

/// 惰性加载并缓存两个会话与字典（进程内只加载一次）。
fn models() -> Result<&'static Models> {
    static MODELS: OnceLock<std::result::Result<Models, String>> = OnceLock::new();
    let r = MODELS.get_or_init(|| {
        init_runtime().map_err(|e| e.to_string())?;
        let det_path = find_file("model/det.onnx", "model/det.onnx")
            .ok_or_else(|| "未找到检测模型 model/det.onnx".to_string())?;
        let rec_path = find_file("model/rec.onnx", "model/rec.onnx")
            .ok_or_else(|| "未找到识别模型 model/rec.onnx".to_string())?;
        let det = load_session(&det_path)?;
        let rec = load_session(&rec_path)?;
        let dict_raw = rec
            .metadata()
            .ok()
            .and_then(|m| m.custom("character"))
            .ok_or_else(|| "识别模型缺少内嵌 character 字典".to_string())?;
        let mut dict: Vec<String> = dict_raw.split('\n').map(|s| s.to_string()).collect();
        // metadata 末尾常带一个空行，去掉以免空字符占位
        while dict.last().map_or(false, |s| s.is_empty()) {
            dict.pop();
        }
        Ok(Models { det: Mutex::new(det), rec: Mutex::new(rec), dict })
    });
    r.as_ref().map_err(|e| AppError::Ocr(e.clone()))
}

// ─────────────────────────────────────────────────────────────────
// 引擎
// ─────────────────────────────────────────────────────────────────

/// ONNX 引擎句柄（无状态；模型通过 [`models`] 全局缓存）。
pub struct OnnxEngine;

/// 探测模型与运行时是否齐备，齐备则返回引擎。
pub fn create() -> Result<OnnxEngine> {
    models().map(|_| OnnxEngine)
}

impl OcrEngineImpl for OnnxEngine {
    fn id(&self) -> &'static str {
        "onnx"
    }

    /// 自包含引擎不接受外部缩放；返回一个足够大的值以免管线误判上限。
    fn max_dimension(&self) -> i32 {
        100_000
    }

    fn recognize(&self, bgra: &[u8], w: i32, h: i32) -> Result<String> {
        let lines = self.recognize_lines(bgra, w, h)?;
        Ok(super::layout::assemble(&lines, crate::config::OcrLayout::Auto))
    }

    fn self_contained(&self) -> bool {
        true
    }

    fn recognize_lines(&self, bgra: &[u8], w: i32, h: i32) -> Result<Vec<TextLine>> {
        if w <= 0 || h <= 0 {
            return Err(AppError::Ocr("图像尺寸无效".into()));
        }
        let (w, h) = (w as usize, h as usize);
        if bgra.len() < w * h * 4 {
            return Err(AppError::Ocr("图像数据不完整".into()));
        }
        let m = models()?;
        let rgb = im::bgra_to_rgb(bgra, w, h);

        // ① 文本检测
        let det_in = im::det_input(&rgb, DET_LIMIT);
        let xt = Tensor::from_array((vec![1i64, 3, det_in.h as i64, det_in.w as i64], det_in.data))
            .map_err(|e| AppError::Ocr(format!("构造检测输入失败: {e}")))?;
        let boxes = {
            let mut det = m.det.lock().map_err(|_| AppError::Ocr("检测会话已损坏".into()))?;
            let outs = det
                .run(ort::inputs!["x" => xt])
                .map_err(|e| AppError::Ocr(format!("检测推理失败: {e}")))?;
            let (shape, prob) = outs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| AppError::Ocr(format!("读取检测输出失败: {e}")))?;
            let dh = shape[2] as usize;
            let dw = shape[3] as usize;
            im::det_boxes(prob, dw, dh, DET_THRESH, BOX_THRESH, UNCLIP_RATIO)
        };
        // 换算回原图坐标，并按行合并相邻框
        let mut rects: Vec<(i32, i32, i32, i32)> = boxes
            .into_iter()
            .filter_map(|b| im::scale_box(b, det_in.ratio, w as i32, h as i32))
            .collect();
        merge_line_boxes(&mut rects);

        // ② 逐框文本识别
        let mut lines = Vec::with_capacity(rects.len());
        for rb in rects {
            // 检测框贴边会切掉笔画（尤其汉字上下缘），裁剪时向外留一圈边距
            let pad = ((rb.3 as f32) * 0.2).max(2.0) as i32;
            let cx = (rb.0 - pad).max(0);
            let cy = (rb.1 - pad).max(0);
            let cw = (rb.2 + 2 * pad).min(w as i32 - cx).max(1);
            let ch = (rb.3 + 2 * pad).min(h as i32 - cy).max(1);
            let crop = im::crop_resize(&rgb, cx, cy, cw, ch, 48);
            let (data, rw, rh) = im::rec_input(&crop);
            let xt = Tensor::from_array((vec![1i64, 3, rh as i64, rw as i64], data))
                .map_err(|e| AppError::Ocr(format!("构造识别输入失败: {e}")))?;
            let (text, conf) = {
                let mut rec = m.rec.lock().map_err(|_| AppError::Ocr("识别会话已损坏".into()))?;
                let outs = rec
                    .run(ort::inputs!["x" => xt])
                    .map_err(|e| AppError::Ocr(format!("识别推理失败: {e}")))?;
                let (shape, logits) = outs[0]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| AppError::Ocr(format!("读取识别输出失败: {e}")))?;
                ctc_decode(logits, shape[1] as usize, shape[2] as usize, &m.dict)
            };
            let text = super::collapse_cjk_spaces(&text);
            if !text.trim().is_empty() {
                lines.push(TextLine { text: text.trim().to_string(), conf, rect: Some(rb) });
            }
        }
        Ok(lines)
    }
}

/// CTC 贪心解码：逐时刻取 argmax，跳过 blank(0) 与重复，映射回字典。
///
/// 返回 `(文本, 置信度)`；置信度取各有效字符处 argmax 概率的均值。
/// 类别排布：`0=blank`，`1..=len(dict)` 为字典字符，`len(dict)+1` 为空格（PaddleOCR 约定）。
fn ctc_decode(logits: &[f32], t: usize, c: usize, dict: &[String]) -> (String, f32) {
    let mut out = String::new();
    let mut prev = 0usize;
    let mut sum = 0f32;
    let mut n = 0usize;
    for ti in 0..t {
        let base = ti * c;
        if base + c > logits.len() {
            break;
        }
        let row = &logits[base..base + c];
        let mut best = 0usize;
        let mut bv = f32::MIN;
        for (i, &v) in row.iter().enumerate() {
            if v > bv {
                bv = v;
                best = i;
            }
        }
        if best != 0 && best != prev {
            if let Some(ch) = class_char(best, dict) {
                out.push(ch);
                sum += bv;
                n += 1;
            }
        }
        prev = best;
    }
    let conf = if n > 0 { sum / n as f32 } else { 0.0 };
    (out, conf)
}

/// 类别索引 → 字符。
fn class_char(idx: usize, dict: &[String]) -> Option<char> {
    if idx >= 1 && idx <= dict.len() {
        dict[idx - 1].chars().next()
    } else if idx == dict.len() + 1 {
        Some(' ')
    } else {
        None
    }
}

/// 合并同一行上被检测拆开的相邻框：纵向重叠 ≥50% 且横向间隙 ≤ 0.8×行高时并为一个。
fn merge_line_boxes(rects: &mut Vec<(i32, i32, i32, i32)>) {
    let mut changed = true;
    while changed {
        changed = false;
        'outer: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let a = rects[i];
                let b = rects[j];
                let ov = (a.1.max(b.1), (a.1 + a.3).min(b.1 + b.3));
                let ov_h = ov.1 - ov.0;
                let min_h = a.3.min(b.3);
                if ov_h > 0 && ov_h * 2 >= min_h {
                    let (l, r) = if a.0 <= b.0 { (a, b) } else { (b, a) };
                    let gap = r.0 - (l.0 + l.2);
                    if gap <= (min_h as f32 * 0.8) as i32 {
                        let x0 = a.0.min(b.0);
                        let y0 = a.1.min(b.1);
                        let x1 = (a.0 + a.2).max(b.0 + b.2);
                        let y1 = (a.1 + a.3).max(b.1 + b.3);
                        rects[i] = (x0, y0, x1 - x0, y1 - y0);
                        rects.remove(j);
                        changed = true;
                        break 'outer;
                    }
                }
            }
        }
    }
    // 阅读顺序：先上后下、再左到右
    rects.sort_by_key(|r| (r.1, r.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict() -> Vec<String> {
        ["a", "b", "c"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ctc_decodes_and_collapses_repeats() {
        // 类别数 = dict(3) + blank + space = 5；构造概率矩阵 [T=6, C=5]
        let mut logits = vec![0f32; 6 * 5];
        let mut set = |t: usize, k: usize| logits[t * 5 + k] = 1.0;
        set(0, 0); // blank
        set(1, 1); // 'a'
        set(2, 1); // 'a' 重复 → 折叠
        set(3, 0); // blank
        set(4, 2); // 'b'
        set(5, 4); // 空格
        let (text, conf) = ctc_decode(&logits, 6, 5, &dict());
        assert_eq!(text, "ab ");
        assert!((conf - 1.0).abs() < 1e-6);
    }

    #[test]
    fn ctc_empty_when_all_blank() {
        let logits = vec![0f32; 4 * 5];
        let mut l = logits.clone();
        for t in 0..4 {
            l[t * 5] = 1.0; // 全 blank
        }
        let (text, conf) = ctc_decode(&l, 4, 5, &dict());
        assert_eq!(text, "");
        assert_eq!(conf, 0.0);
    }

    #[test]
    fn class_char_maps_blank_dict_and_space() {
        let d = dict();
        assert_eq!(class_char(0, &d), None); // blank
        assert_eq!(class_char(1, &d), Some('a'));
        assert_eq!(class_char(3, &d), Some('c'));
        assert_eq!(class_char(4, &d), Some(' ')); // len+1 → 空格
        assert_eq!(class_char(99, &d), None);
    }

    #[test]
    fn merge_joins_same_row_boxes() {
        let mut r = vec![(0, 10, 30, 20), (35, 11, 30, 20), (0, 40, 20, 20)];
        merge_line_boxes(&mut r);
        // 前两个（同一行、间隙小）应合并；第三个单独
        assert_eq!(r.len(), 2, "实际 {r:?}");
        let merged = r.iter().find(|b| b.1 < 20).unwrap();
        assert!(merged.2 >= 65, "合并后宽度应覆盖两段，实际 {merged:?}");
    }

    #[test]
    fn merge_keeps_distant_boxes_separate() {
        let mut r = vec![(0, 0, 10, 10), (100, 0, 10, 10)];
        merge_line_boxes(&mut r);
        assert_eq!(r.len(), 2);
    }

    /// 模型齐备时，管线的引擎挑选点应选中内置 ONNX 引擎。
    #[test]
    fn engine_selection_prefers_onnx() {
        match super::super::engine::create() {
            Some(e) => assert_eq!(e.id(), "onnx", "应优先选用内置 ONNX 引擎"),
            None => eprintln!("跳过：模型/运行时不可用，create() 已回退系统引擎"),
        }
    }

    /// 优先命中 exe 同目录的文件，缺失时回退仓库路径。
    #[test]
    fn find_file_prefers_exe_dir_then_repo() {
        let tmp = std::env::temp_dir().join("pixshot_findfile_test");
        let _ = std::fs::create_dir_all(&tmp);
        let probe = tmp.join("probe_marker.onnx");
        std::fs::write(&probe, b"x").unwrap();
        // exe 目录里有 → 命中它
        let got = find_file_in(Some(&tmp), "probe_marker.onnx", "model/det.onnx").unwrap();
        assert_eq!(got, probe);
        // exe 目录里没有 → 回退仓库路径（model/det.onnx 已随仓库分发）
        let got = find_file_in(Some(&tmp), "nope.onnx", "model/det.onnx").unwrap();
        assert!(got.ends_with("det.onnx"));
        // exe 目录未知 → 仍回退仓库路径
        assert!(find_file_in(None, "nope.onnx", "model/det.onnx").is_some());
        let _ = std::fs::remove_file(&probe);
    }

    /// 端到端：GDI 离屏渲染一行中英混排文字 → 检测 + 识别 → 断言关键字与置信度。
    #[test]
    fn onnx_recognizes_rendered_text() {
        if models().is_err() {
            eprintln!("跳过：ONNX 模型/运行时不可用");
            return;
        }
        let (w, h) = (600i32, 110i32);
        let bgra = render_text_bgra(w, h, "PixShot OCR 识别测试 AB 12345");
        let lines = OnnxEngine.recognize_lines(&bgra, w, h).expect("识别失败");
        let text: String = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
        let confs: Vec<f32> = lines.iter().map(|l| l.conf).collect();
        eprintln!("OCR 结果: {text:?} 行置信度 {confs:?}");
        assert!(!lines.is_empty(), "应至少检出并识别出一行");
        assert!(text.contains("12345"), "应识别出数字，实际 {text:?}");
        assert!(text.contains("识别测试"), "应识别出中文，实际 {text:?}");
        assert!(text.contains("PixShot"), "应识别出英文，实际 {text:?}");
        assert!(confs.iter().all(|c| *c > 0.8), "行置信度应偏高，实际 {confs:?}");
    }

    /// GDI 离屏渲染一行文字为 BGRA（白底黑字、自上而下），供测试构造确定性输入。
    fn render_text_bgra(w: i32, h: i32, text: &str) -> Vec<u8> {
        use windows::core::w;
        use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
            GdiFlush, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
            BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
            DEFAULT_PITCH, DIB_RGB_COLORS, DT_LEFT, DT_SINGLELINE, DT_TOP, FW_NORMAL, HGDIOBJ,
            OUT_DEFAULT_PRECIS, TRANSPARENT,
        };
        unsafe {
            let screen = GetDC(HWND::default());
            let mem = CreateCompatibleDC(screen);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // 负高 → 自上而下
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0)
                .expect("CreateDIBSection");
            assert!(!bits.is_null());
            let old = SelectObject(mem, HGDIOBJ(dib.0));
            // 白底
            std::ptr::write_bytes(bits as *mut u8, 0xFF, (w * h * 4) as usize);
            let font = CreateFontW(
                44,
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32,
                w!("Microsoft YaHei"),
            );
            let oldf = SelectObject(mem, HGDIOBJ(font.0));
            SetBkMode(mem, TRANSPARENT);
            SetTextColor(mem, COLORREF(0));
            let mut buf: Vec<u16> = text.encode_utf16().collect();
            let mut rc = RECT { left: 12, top: 24, right: w - 12, bottom: h - 12 };
            DrawTextW(mem, &mut buf, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE);
            let _ = GdiFlush();

            let mut data = vec![0u8; (w * h * 4) as usize];
            std::ptr::copy_nonoverlapping(bits as *const u8, data.as_mut_ptr(), data.len());
            for px in data.chunks_exact_mut(4) {
                px[3] = 0xFF;
            }

            SelectObject(mem, oldf);
            let _ = DeleteObject(HGDIOBJ(font.0));
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(mem);
            let _ = ReleaseDC(HWND::default(), screen);
            data
        }
    }
}
