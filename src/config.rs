//! 配置：`config.ini`（与 pixshot.exe 同目录，`key = value` 纯文本，零依赖）。
//!
//! 设计约束：
//! - 手写解析，不引第三方 TOML/YAML 库（保持"零额外依赖"路线）；
//! - 解析失败一律回退默认值 —— 配置文件绝不能让程序起不来；
//! - 文件不存在时写入带注释的模板，用户可自行编辑。

use std::path::PathBuf;
use std::sync::Mutex;

// 修饰键位（与 Win32 `MOD_*` 数值一致）。此处用裸 u32，
// 使热键解析成为不依赖 Windows API 的纯函数，便于单元测试。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_NOREPEAT: u32 = 0x4000;

/// 保存格式（WIC 容器）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SaveFormat {
    Png,
    Jpeg,
    Bmp,
}

impl SaveFormat {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Bmp => "bmp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG 图片",
            Self::Jpeg => "JPEG 图片",
            Self::Bmp => "BMP 位图",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// 截图热键，如 `F1`、`Ctrl+Shift+S`、`Alt+PrintScreen`。
    pub hotkey: String,
    /// 默认保存格式。
    pub format: SaveFormat,
    /// 上次保存目录（下次对话框默认打开它）。
    pub save_dir: Option<String>,
    /// 首选 OCR 语言标签（如 `zh-Hans-CN`）；空 = 按系统语言自动回退。
    pub ocr_lang: Option<String>,
    /// 轻提示停留时长（毫秒）。
    pub toast_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "F1".into(),
            format: SaveFormat::Png,
            save_dir: None,
            ocr_lang: None,
            toast_ms: 1600,
        }
    }
}

impl Config {
    /// 界面展示用：热键文本（原样，用户在配置文件里写什么就显示什么）。
    pub fn hotkey_label(&self) -> &str {
        &self.hotkey
    }
}

static CACHE: Mutex<Option<Config>> = Mutex::new(None);

/// 配置文件路径：与 pixshot.exe 同目录的 `config.ini`；
/// 取不到 exe 路径时（极少见）回退当前工作目录。
pub fn path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("config.ini")))
        .unwrap_or_else(|| PathBuf::from("config.ini"))
}

/// 取当前配置（首次调用时加载并缓存）。
pub fn get() -> Config {
    let mut g = lock();
    if g.is_none() {
        *g = Some(load());
    }
    g.clone().unwrap_or_default()
}

/// 更新配置并写盘（写失败只记日志，不影响功能）。
pub fn update<F: FnOnce(&mut Config)>(f: F) {
    let mut g = lock();
    let mut c = g.clone().unwrap_or_else(|| load());
    f(&mut c);
    if let Err(e) = write(&c) {
        crate::diag::log(&format!("[config] 写配置失败: {e}"));
    }
    *g = Some(c);
}

/// 强制从磁盘重新加载（丢弃缓存）。
/// 预留：配置文件被外部改动后热重载（当前在启动时一次性读取）。
#[allow(dead_code)]
pub fn reload() -> Config {
    let c = load();
    *lock() = Some(c.clone());
    c
}

fn lock() -> std::sync::MutexGuard<'static, Option<Config>> {
    // 配置只在 GUI 线程访问；即便发生 panic 中毒也照常使用旧值
    match CACHE.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    }
}

fn load() -> Config {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(text) => {
            let (c, warnings) = parse_detailed(&text);
            for w in &warnings {
                crate::diag::log(&format!("[config] {w}"));
            }
            crate::diag::log(&format!(
                "[config] 已加载 {} -> hotkey={} format={} save_dir={:?}",
                p.display(),
                c.hotkey,
                c.format.ext(),
                c.save_dir
            ));
            c
        }
        Err(_) => {
            // 首次运行：落一份带注释的模板，让用户知道有这个文件
            let c = Config::default();
            if let Err(e) = write(&c) {
                crate::diag::log(&format!("[config] 创建默认配置失败: {e}"));
            } else {
                crate::diag::log(&format!("[config] 已创建默认配置 {}", p.display()));
            }
            c
        }
    }
}

fn write(c: &Config) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, render(c))
}

/// 生成配置文件内容（带说明注释）。
pub fn render(c: &Config) -> String {
    let mut s = String::new();
    s.push_str("# PixShot 配置文件（UTF-8）\n");
    s.push_str("# 修改后重启 PixShot 生效；删除本文件即恢复默认。\n\n");
    s.push_str("# 截图热键：F1..F24 / 字母 / 数字 / PrintScreen / Home / End 等，\n");
    s.push_str("# 可加修饰键 Ctrl、Alt、Shift、Win，用 + 连接。\n");
    s.push_str("# 例：hotkey = Ctrl+Shift+S\n");
    s.push_str(&format!("hotkey = {}\n\n", c.hotkey));
    s.push_str("# 默认保存格式：png / jpg / bmp\n");
    s.push_str(&format!("format = {}\n\n", c.format.ext()));
    s.push_str("# 上次保存目录（程序自动维护，也可手工指定）\n");
    s.push_str(&format!("save_dir = {}\n\n", c.save_dir.as_deref().unwrap_or("")));
    s.push_str("# OCR 首选语言（留空则按系统语言自动回退），例：zh-Hans-CN\n");
    s.push_str(&format!("ocr_lang = {}\n\n", c.ocr_lang.as_deref().unwrap_or("")));
    s.push_str("# 成功提示停留时长（毫秒，300-10000）\n");
    s.push_str(&format!("toast_ms = {}\n", c.toast_ms));
    s
}

/// 解析配置文本（纯函数，可单测）。任何非法项都静默回退默认值。
/// 只取解析结果、丢弃告警的简化入口（供单测使用）。
#[allow(dead_code)]
pub fn parse(text: &str) -> Config {
    parse_detailed(text).0
}

/// 解析并返回被忽略的非法项说明（供日志输出；解析本身无副作用，便于单测）。
pub fn parse_detailed(text: &str) -> (Config, Vec<String>) {
    let mut c = Config::default();
    let mut warnings: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || line.starts_with('[')
        {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let val = v.trim();
        match key.as_str() {
            "hotkey" => {
                if parse_hotkey(val).is_some() {
                    c.hotkey = val.to_string();
                } else {
                    warnings.push(format!("无法识别的热键 {val:?}，沿用默认 F1"));
                }
            }
            "format" => match SaveFormat::parse(val) {
                Some(f) => c.format = f,
                None => warnings.push(format!("无法识别的保存格式 {val:?}，沿用默认 png")),
            },
            "save_dir" => {
                if !val.is_empty() {
                    c.save_dir = Some(val.to_string());
                }
            }
            "ocr_lang" => {
                if !val.is_empty() {
                    c.ocr_lang = Some(val.to_string());
                }
            }
            "toast_ms" => {
                if let Ok(n) = val.parse::<u32>() {
                    c.toast_ms = n.clamp(300, 10_000);
                }
            }
            _ => {}
        }
    }
    (c, warnings)
}

/// 解析热键字符串 → (修饰键位, 虚拟键码)。
/// 非法输入返回 None（调用方应回退默认热键并记录日志）。
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut mods = MOD_NOREPEAT;
    let mut vk: Option<u32> = None;
    for part in s.split('+') {
        let k = part.trim().to_ascii_lowercase();
        if k.is_empty() {
            return None;
        }
        match k.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            other => {
                // 只允许一个主键；出现两个即视为非法
                if vk.is_some() {
                    return None;
                }
                vk = vk_from_name(other);
                vk?;
            }
        }
    }
    vk.map(|v| (mods, v))
}

fn vk_from_name(k: &str) -> Option<u32> {
    // F1..F24 → 0x70..0x87
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then(|| 0x70 + n - 1);
    }
    // 单个字母/数字 → 其 ASCII 大写（VK_A..VK_Z == 'A'..'Z'，VK_0..VK_9 == '0'..'9'）
    let b = k.as_bytes();
    if b.len() == 1 {
        let ch = b[0].to_ascii_uppercase();
        if ch.is_ascii_alphanumeric() {
            return Some(ch as u32);
        }
    }
    Some(match k {
        "printscreen" | "prtsc" | "snapshot" => 0x2C,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "space" => 0x20,
        "tab" => 0x09,
        "backspace" | "back" => 0x08,
        "pause" => 0x13,
        "scrolllock" | "scroll" => 0x91,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_single_key() {
        assert_eq!(parse_hotkey("F1"), Some((MOD_NOREPEAT, 0x70)));
        assert_eq!(parse_hotkey("f12"), Some((MOD_NOREPEAT, 0x7B)));
        assert_eq!(parse_hotkey("PrintScreen"), Some((MOD_NOREPEAT, 0x2C)));
        assert_eq!(parse_hotkey("A"), Some((MOD_NOREPEAT, 0x41)));
        assert_eq!(parse_hotkey("7"), Some((MOD_NOREPEAT, 0x37)));
    }

    #[test]
    fn hotkey_with_modifiers() {
        assert_eq!(
            parse_hotkey("Ctrl+Shift+S"),
            Some((MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT, 0x53))
        );
        // 顺序无关、大小写无关
        assert_eq!(parse_hotkey("shift+ctrl+s"), parse_hotkey("Ctrl+Shift+S"));
        assert_eq!(
            parse_hotkey("Alt+PrintScreen"),
            Some((MOD_NOREPEAT | MOD_ALT, 0x2C))
        );
    }

    #[test]
    fn hotkey_rejects_invalid() {
        assert_eq!(parse_hotkey(""), None);
        assert_eq!(parse_hotkey("F25"), None);
        assert_eq!(parse_hotkey("Ctrl+"), None);
        assert_eq!(parse_hotkey("Ctrl+A+B"), None); // 两个主键
        assert_eq!(parse_hotkey("NoSuchKey"), None);
    }

    /// 越界与非法项必须回退默认值，绝不能让程序起不来。
    #[test]
    fn parse_falls_back_on_bad_values() {
        let c = parse("hotkey = NotAKey\nformat = tiff\ntoast_ms = 999999\n");
        assert_eq!(c.hotkey, "F1");
        assert_eq!(c.format, SaveFormat::Png);
        assert_eq!(c.toast_ms, 10_000); // 被钳制到上限
    }

    #[test]
    fn parse_reads_valid_values() {
        let c = parse(
            "# 注释\n\
             hotkey = Ctrl+Alt+F5\n\
             format = jpg\n\
             save_dir = D:\\shots\n\
             ocr_lang = zh-Hans-CN\n\
             toast_ms = 900\n",
        );
        assert_eq!(c.hotkey, "Ctrl+Alt+F5");
        assert_eq!(c.format, SaveFormat::Jpeg);
        assert_eq!(c.save_dir.as_deref(), Some(r"D:\shots"));
        assert_eq!(c.ocr_lang.as_deref(), Some("zh-Hans-CN"));
        assert_eq!(c.toast_ms, 900);
    }

    /// 渲染 → 解析 必须幂等（否则每次退出都会改写用户配置）。
    #[test]
    fn render_parse_roundtrip() {
        let mut c = Config::default();
        c.hotkey = "Ctrl+Shift+A".into();
        c.format = SaveFormat::Bmp;
        c.save_dir = Some(r"C:\temp".into());
        c.ocr_lang = Some("en-US".into());
        c.toast_ms = 2000;
        let back = parse(&render(&c));
        assert_eq!(back.hotkey, c.hotkey);
        assert_eq!(back.format, c.format);
        assert_eq!(back.save_dir, c.save_dir);
        assert_eq!(back.ocr_lang, c.ocr_lang);
        assert_eq!(back.toast_ms, c.toast_ms);
    }

    #[test]
    fn format_parse_aliases() {
        assert_eq!(SaveFormat::parse("PNG"), Some(SaveFormat::Png));
        assert_eq!(SaveFormat::parse("jpeg"), Some(SaveFormat::Jpeg));
        assert_eq!(SaveFormat::parse(" jpg "), Some(SaveFormat::Jpeg));
        assert_eq!(SaveFormat::parse("bmp"), Some(SaveFormat::Bmp));
        assert_eq!(SaveFormat::parse("gif"), None);
    }
}
