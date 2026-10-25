//! build.rs —— 应用图标生成与嵌入（零新增依赖）。
//!
//! 1. 纯 Rust 光栅化器渲染 Fluent 风格应用图标（圆角方形 + 白色取景框
//!    四角括号 + 焦点圆点，蓝色→蓝紫渐变），4× 超采样抗锯齿；
//!    几何定义在 `src/icon_geometry.rs`（与单元测试共用同一份代码）；
//! 2. 输出多尺寸 .ico（16/24/32/48/64）；
//! 3. 编译 `1 ICON` 资源对象并链接进 exe ——
//!    GNU 工具链用 `windres`，MSVC 工具链用 `rc.exe`（均随工具链自带）。
//!
//! 失败策略（修复静默降级）：两种编译器都找不到时 **构建失败** 并给出解决方式；
//! 确需无图标构建可设 `PIXSHOT_NO_ICON=1` 显式跳过。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 图标几何（与 tests/icon_geometry.rs 共用同一份源码）。
#[path = "src/icon_geometry.rs"]
mod geo;

use geo::CORNER;

/// 渐变顶色（RGB）。
const GRAD_TOP: [f32; 3] = [100.0, 136.0, 255.0];
/// 渐变底色（RGB）。
const GRAD_BOT: [f32; 3] = [70.0, 90.0, 230.0];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_geometry.rs");

    // OCR 随包资产：把 onnxruntime.dll 与 model/*.onnx 摆到 exe 同目录，
    // 使 `target/<profile>/` 自带可运行（发布时整目录即为分发内容）。
    stage_ocr_assets();

    if env::var_os("PIXSHOT_NO_ICON").is_some() {
        println!("cargo:warning=PIXSHOT_NO_ICON 已设置 -> 跳过图标嵌入（托盘将回退系统默认图标）");
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out.join("pixshot.ico");

    // 256 尺寸的 32bpp 未压缩条目占 ~256KB，超体积预算余量；
    // 64 为最大尺寸已覆盖托盘/任务栏/Explorer 常用场景。
    let sizes: [u32; 5] = [16, 24, 32, 48, 64];
    write_ico(&ico_path, &sizes);

    match embed_icon(&out, &ico_path) {
        Ok(res) => println!("cargo:rustc-link-arg-bins={}", res.display()),
        Err(e) => {
            // 图标是产品的一部分：静默降级会导致"exe 没有图标"这种极难排查的问题，
            // 这里直接让构建失败，并在信息里给出两种解决方式。
            panic!(
                "\n\n应用图标嵌入失败：{e}\n\
                 \n\
                 解决方式（任选其一）：\n\
                 1) 使用任一自带资源编译器的工具链构建：\n\
                    · MSVC 工具链：从「x64 Native Tools Command Prompt for VS」启动（自带 rc.exe）\n\
                    · GNU 工具链：安装 MSYS2 / MinGW（自带 windres.exe）\n\
                 2) 明确不需要图标时，设环境变量 PIXSHOT_NO_ICON=1 后重新构建\n\
                 \n"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// OCR 资产布置：onnxruntime.dll + model/*.onnx → exe 同目录
// ─────────────────────────────────────────────────────────────────

/// 把内置 OCR 引擎需要的运行时与模型复制到 `target/<profile>/`（exe 所在目录）。
///
/// 仅在大小不同时覆盖，避免每次增量构建都重写 ~30MB。缺文件只告警不中断：
/// 允许在未随仓库放置模型的环境里构建（此时 OCR 会回退系统引擎）。
fn stage_ocr_assets() {
    let out = match env::var("OUT_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(_) => return,
    };
    // OUT_DIR = <target>/<profile>/build/pixshot-<hash>/out → 上溯三级即 exe 目录
    let Some(exe_dir) = out.ancestors().nth(3).map(Path::to_path_buf) else {
        return;
    };
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());

    let pairs: [(&str, PathBuf); 3] = [
        (
            "vendor/onnxruntime/onnxruntime.dll",
            exe_dir.join("onnxruntime.dll"),
        ),
        (
            "vendor/onnxruntime/onnxruntime_providers_shared.dll",
            exe_dir.join("onnxruntime_providers_shared.dll"),
        ),
        ("model/det.onnx", exe_dir.join("model").join("det.onnx")),
    ];
    for (rel, dst) in pairs {
        let src = root.join(rel);
        println!("cargo:rerun-if-changed={rel}");
        if !src.is_file() {
            println!("cargo:warning=缺少 OCR 资产 {rel}（内置引擎将不可用）");
            continue;
        }
        copy_if_changed(&src, &dst);
    }
    // rec.onnx 体积最大，单独处理也走同一逻辑
    let rec_src = root.join("model/rec.onnx");
    println!("cargo:rerun-if-changed=model/rec.onnx");
    if rec_src.is_file() {
        copy_if_changed(&rec_src, &exe_dir.join("model").join("rec.onnx"));
    } else {
        println!("cargo:warning=缺少 OCR 资产 model/rec.onnx（内置引擎将不可用）");
    }
}

/// 大小不同才复制（内容变化必然改变大小，够用且省事）。
fn copy_if_changed(src: &Path, dst: &Path) {
    let same = match (fs::metadata(src), fs::metadata(dst)) {
        (Ok(a), Ok(b)) => a.len() == b.len(),
        _ => false,
    };
    if same {
        return;
    }
    if let Some(dir) = dst.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(e) = fs::copy(src, dst) {
        println!("cargo:warning=复制 {} 失败: {e}", src.display());
    }
}

// ─────────────────────────────────────────────────────────────────
// 图标光栅化（纯 std，无 GDI 依赖）
// ─────────────────────────────────────────────────────────────────

/// 渲染单尺寸 RGBA（自上而下行序，straight alpha）。
fn render_rgba(size: u32) -> Vec<u8> {
    let s = size as usize;
    let ss = 4usize; // 4×4 超采样
    let mut out = vec![0u8; s * s * 4];
    let sf = s as f32;
    let r = CORNER * sf;

    for py in 0..s {
        for px in 0..s {
            let mut cov = 0usize; // 圆角方形覆盖
            let mut glyph = 0usize; // 白色字形覆盖
            for sy in 0..ss {
                for sx in 0..ss {
                    // 子采样中心落在像素内（+0.5/ss），避免整体偏移半像素
                    let x = px as f32 + (sx as f32 + 0.5) / ss as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / ss as f32;
                    if geo::in_rounded(x, y, sf, r) {
                        cov += 1;
                        if geo::on_glyph(x, y, sf) {
                            glyph += 1;
                        }
                    }
                }
            }
            let cov_f = cov as f32 / (ss * ss) as f32;
            let glyph_f = glyph as f32 / (ss * ss) as f32;
            let bg = geo::lerp3(GRAD_TOP, GRAD_BOT, py as f32 / (s - 1).max(1) as f32);
            // 白色字形叠加在渐变上
            let col = geo::lerp3(bg, [255.0, 255.0, 255.0], glyph_f / cov_f.max(f32::EPSILON));
            let i = (py * s + px) * 4;
            out[i] = col[0].round().clamp(0.0, 255.0) as u8; // R
            out[i + 1] = col[1].round().clamp(0.0, 255.0) as u8; // G
            out[i + 2] = col[2].round().clamp(0.0, 255.0) as u8; // B
            out[i + 3] = (cov_f * 255.0).round().clamp(0.0, 255.0) as u8; // A
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────
// .ico 容器（单文件多尺寸，32bpp + 全零 AND 掩码）
// ─────────────────────────────────────────────────────────────────

fn write_ico(path: &Path, sizes: &[u32]) {
    let mut images: Vec<(u32, Vec<u8>)> = Vec::new();
    for &s in sizes {
        let rgba = render_rgba(s);
        let (w, h) = (s as usize, s as usize);
        // BITMAPINFOHEADER（高 = 2×，像素 + AND 掩码自底向上）
        let mask_row = (w + 31) / 32 * 4;
        let mask_size = mask_row * h;
        let mut data = Vec::with_capacity(40 + w * h * 4 + mask_size);
        data.extend_from_slice(&40u32.to_le_bytes()); // biSize
        data.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
        data.extend_from_slice(&((h as i32) * 2).to_le_bytes()); // biHeight = 2×
        data.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        data.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        data.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        data.extend_from_slice(&((w * h * 4 + mask_size) as u32).to_le_bytes());
        data.extend_from_slice(&[0u8; 16]); // 其余字段全 0
                                            // 像素（自底向上，BGRA）
        for row in (0..h).rev() {
            for col in 0..w {
                let i = (row * w + col) * 4;
                data.push(rgba[i + 2]); // B
                data.push(rgba[i + 1]); // G
                data.push(rgba[i]); // R
                data.push(rgba[i + 3]); // A
            }
        }
        // AND 掩码（全 0，alpha 负责）
        data.extend(std::iter::repeat(0u8).take(mask_size));
        images.push((s, data));
    }

    let n = images.len();
    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&[0u8, 0]); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    ico.extend_from_slice(&(n as u16).to_le_bytes());
    let mut offset = 6 + n * 16;
    for (s, data) in &images {
        let dim = if *s >= 256 { 0u8 } else { *s as u8 };
        ico.push(dim); // width
        ico.push(dim); // height
        ico.push(0); // colors
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bitcount
        ico.extend_from_slice(&(data.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &images {
        ico.extend_from_slice(data);
    }
    fs::write(path, ico).expect("写入 ico");
}

// ─────────────────────────────────────────────────────────────────
// 资源嵌入：windres（GNU）/ rc.exe（MSVC）→ rustc-link-arg-bins
// ─────────────────────────────────────────────────────────────────

/// 可用的资源编译器。
enum RcTool {
    /// GNU windres：输出 COFF 对象（.o）
    Windres(String),
    /// MSVC rc.exe：输出 .res（link.exe 直接接受）
    MsvcRc(String),
}

fn embed_icon(out: &Path, ico: &Path) -> Result<PathBuf, String> {
    let rc = out.join("icon.rc");
    // 路径统一成正斜杠：windres / rc 均接受，且免去反斜杠转义歧义
    fs::write(
        &rc,
        format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/")),
    )
    .map_err(|e| format!("写 icon.rc 失败: {e}"))?;

    let tool = find_tool()
        .ok_or_else(|| "PATH 中既没有 windres.exe（MinGW）也没有 rc.exe（MSVC）".to_string())?;

    match tool {
        RcTool::Windres(exe) => {
            let obj = out.join("icon_res.o");
            let st = Command::new(&exe)
                .arg("--target=pe-x86-64")
                .arg("--input")
                .arg(&rc)
                .arg("--output")
                .arg(&obj)
                .status()
                .map_err(|e| format!("执行 ({exe}) 失败: {e}"))?;
            if !st.success() {
                return Err(format!("windres 退出码 {st}"));
            }
            Ok(obj)
        }
        RcTool::MsvcRc(exe) => {
            let res = out.join("icon_res.res");
            let st = Command::new(&exe)
                .arg("/nologo")
                .arg(format!("/fo{}", res.display()))
                .arg(&rc)
                .status()
                .map_err(|e| format!("执行 ({exe}) 失败: {e}"))?;
            if !st.success() {
                return Err(format!("rc.exe 退出码 {st}"));
            }
            Ok(res)
        }
    }
}

fn find_tool() -> Option<RcTool> {
    // 1) windres：PATH → MSYS2 常见安装位置
    for c in [
        "windres.exe",
        "windres",
        r"C:\msys64\mingw64\bin\windres.exe",
        r"C:\msys64\usr\bin\windres.exe",
    ] {
        if Command::new(c).arg("--version").output().is_ok() {
            return Some(RcTool::Windres(c.to_string()));
        }
    }
    // 2) rc.exe：PATH（VS 开发者命令行 / cargo 已继承的 MSVC 环境）
    for c in ["rc.exe", "rc"] {
        if Command::new(c).arg("/?").output().is_ok() {
            return Some(RcTool::MsvcRc(c.to_string()));
        }
    }
    None
}
