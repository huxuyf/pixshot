//! 智能选区：窗口 / UI 元素几何拾取（只读，不做自动化操作）。
//!
//! 纯 Win32 + UIA，无第三方依赖；所有接口在 GUI 线程调用。
//! UIA 不可用（Win7 精简版 / COM 初始化失败）时自动退化为纯窗口级拾取。

use windows::core::GUID;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetWindowLongPtrW, GetWindowRect, GetWindowTextW, SetWindowLongPtrW,
    SetWindowPos, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, WS_EX_TRANSPARENT,
};

/// `CLSID_CUIAutomation`：windows crate 未导出该常量，按系统 SDK 定义。
const CLSID_CUIAUTOMATION: GUID = GUID::from_u128(0xFF48_DBA4_60EF_4201_AA87_5410_3EEF_594E);

/// 拾取级别（对应配置 `pick_level`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickLevel {
    /// 不做智能拾取，完全沿用原有拖拽框选。
    None,
    /// 拖动为主；单击时自动给出光标处窗口矩形（"点击自动扩充"）。
    #[default]
    Region,
    /// 悬停高亮顶层窗口（含子窗口边界修正）。
    Window,
    /// 悬停高亮 UIA 元素（按 ControlView 树）。
    Element,
}

impl PickLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "region" => Some(Self::Region),
            "none" | "off" => Some(Self::None),
            "window" | "win" => Some(Self::Window),
            "element" | "elem" | "uia" => Some(Self::Element),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Region => "region",
            Self::Window => "window",
            Self::Element => "element",
        }
    }
}

/// 拾取到的候选框（屏幕物理像素，虚拟屏坐标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PickRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub const fn right(self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(self) -> i32 {
        self.y + self.h
    }

    #[allow(dead_code)]
    pub const fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Window,
    Element,
    /// UIA 不可用时的兜底（仅窗口矩形）。
    Fallback,
}

/// 候选目标。`pid`/`class`/`title` 只用于标签显示与日志，不参与业务判断。
#[derive(Debug, Clone)]
pub struct PickTarget {
    pub hwnd: isize,
    pub rect: PickRect,
    pub kind: TargetKind,
    /// 供父/子导航使用：UIA 运行时 id（当前实现保留为空）。
    #[allow(dead_code)]
    pub runtime_id: Option<Vec<i32>>,
    /// 层级深度（0 = 顶层窗口）。
    pub depth: u32,
    /// 展示用短标题（截断到 32 字符）。
    pub label: String,
}

/// 拾取引擎。构造失败（UIA 不可用）时 `uia_available() == false`，
/// 所有探测自动退化为纯窗口级。
pub struct PickEngine {
    uia: Option<IUIAutomation>,
    walker: Option<IUIAutomationTreeWalker>,
    /// 最近一次探测到的 UIA 元素（父/子导航的起点）。
    last_element: Option<IUIAutomationElement>,
    /// 上一次探针是否动过 overlay 样式（S1-a 方案用）。
    probe_dirty: bool,
    /// 当前候选（用于父子导航时校验）。
    last_target: Option<PickTarget>,
    /// 元素级最大下钻深度（配置 `pick_max_depth`）。
    max_depth: u32,
}

impl Default for PickEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PickEngine {
    /// 在 GUI 线程创建；内部完成 `CoCreateInstance(CUIAutomation)`。
    pub fn new() -> Self {
        unsafe {
            let uia: windows::core::Result<IUIAutomation> =
                CoCreateInstance(&CLSID_CUIAUTOMATION, None, CLSCTX_INPROC_SERVER);
            match uia {
                Ok(uia) => {
                    let walker = uia.ControlViewWalker().ok();
                    Self {
                        uia: Some(uia),
                        walker,
                        last_element: None,
                        probe_dirty: false,
                        last_target: None,
                        max_depth: 12,
                    }
                }
                Err(_) => Self {
                    uia: None,
                    walker: None,
                    last_element: None,
                    probe_dirty: false,
                    last_target: None,
                    max_depth: 12,
                },
            }
        }
    }

    /// 设置元素级最大下钻深度（`pick_max_depth`，1..=64）。
    pub fn set_max_depth(&mut self, depth: u32) {
        self.max_depth = depth.clamp(1, 64);
    }

    #[allow(dead_code)]
    pub fn uia_available(&self) -> bool {
        self.uia.is_some() && self.walker.is_some()
    }

    /// 探测 `(x, y)` 处候选框。
    ///
    /// - `self_hwnd`：overlay 自身窗口，用于排除自遮挡（临时 `WS_EX_TRANSPARENT`）。
    /// - `padding`：逻辑像素外扩量，内部按当前 DPI 换算为物理像素。
    /// - `expand`：true 时把"客户区"矩形扩充到窗口矩形（Auto expand selection）。
    pub fn probe(
        &mut self,
        self_hwnd: HWND,
        x: i32,
        y: i32,
        level: PickLevel,
        padding_logical: i32,
        expand: bool,
    ) -> Option<PickTarget> {
        if level == PickLevel::None {
            return None;
        }
        // S1-a：临时让 overlay 不参与命中测试，探测完立即恢复。
        let dirty = unsafe { set_transparent(self_hwnd, true) };
        self.probe_dirty = dirty;

        let target = match level {
            PickLevel::None => None,
            PickLevel::Region | PickLevel::Window => {
                unsafe { window_target(self_hwnd, x, y, padding_logical, expand) }
            }
            PickLevel::Element => {
                let elem = unsafe { self.element_at(self_hwnd, x, y, padding_logical) };
                match elem {
                    // 元素级始终返回元素矩形；auto-expand 仅对区域/窗口级生效。
                    Some(t) => Some(t),
                    None => unsafe { window_target(self_hwnd, x, y, padding_logical, true) }
                        .map(|mut t| {
                            t.kind = TargetKind::Fallback;
                            t
                        }),
                }
            }
        };

        self.restore_overlay(self_hwnd);
        self.last_target = target.clone();
        target
    }

    /// 父元素 / 父窗口（`depth - 1`）；到达根返回 None。
    pub fn parent(&mut self, t: &PickTarget) -> Option<PickTarget> {
        if t.depth == 0 {
            return None;
        }
        let walker = self.walker.as_ref()?;
        let elem = self.last_element.as_ref()?;
        unsafe {
            let parent = walker.GetParentElement(elem).ok()?;
            if !is_self_element(&parent, HWND::default()) {
                self.last_element = Some(parent.clone());
                let mut p = element_target(&parent, t.depth - 1, 0)?;
                p.hwnd = t.hwnd;
                return Some(p);
            }
        }
        None
    }

    /// 子元素：取包含 `(x, y)` 的最深一层子元素（`depth + 1`）。
    pub fn child(&mut self, t: &PickTarget, x: i32, y: i32) -> Option<PickTarget> {
        let walker = self.walker.as_ref()?;
        let elem = self.last_element.as_ref()?;
        unsafe {
            let child = deepest_child(walker, elem, x, y, t.depth + 1, self.max_depth)?;
            let mut p = element_target(&child, 0, 0)?;
            p.hwnd = t.hwnd;
            p.depth = t.depth + 1;
            self.last_element = Some(child);
            Some(p)
        }
    }

    /// 供 S1 收尾：若探针改过 overlay 样式，恢复原样（幂等）。
    pub fn restore_overlay(&mut self, self_hwnd: HWND) {
        if self.probe_dirty {
            unsafe { set_transparent(self_hwnd, false) };
            self.probe_dirty = false;
        }
    }

    unsafe fn element_at(
        &mut self,
        self_hwnd: HWND,
        x: i32,
        y: i32,
        padding_logical: i32,
    ) -> Option<PickTarget> {
        let uia = self.uia.as_ref()?;
        let walker = self.walker.as_ref()?;
        let elem = uia.ElementFromPoint(POINT { x, y }).ok()?;
        // ElementFromPoint 仍可能返回 overlay 自身（自定义 HWND 的默认 provider）：
        // 此时改用窗口级；若下层是 UIA 元素，再深挖一层。
        if is_self_element(&elem, self_hwnd) {
            self.last_element = None;
            return None;
        }
        // 从窗口根向下找最深包含点 (x,y) 的元素，避免返回整窗元素。
        let deepest = deepest_child(walker, &elem, x, y, 0, self.max_depth);
        let elem = deepest.unwrap_or(elem);
        self.last_element = Some(elem.clone());
        element_target(&elem, 0, padding_logical)
    }
}

/// 纯函数：把窗口矩形裁到可见区域并去除零/负尺寸。
pub fn normalize(rect: PickRect, virtual_screen: PickRect) -> Option<PickRect> {
    let x0 = rect.x.max(virtual_screen.x);
    let y0 = rect.y.max(virtual_screen.y);
    let x1 = rect.right().min(virtual_screen.right());
    let y1 = rect.bottom().min(virtual_screen.bottom());
    let (w, h) = (x1 - x0, y1 - y0);
    // 0 宽 / 1px 宽视为无效候选（避免点到边界时产出不可见选区）
    (w >= 2 && h >= 2).then_some(PickRect::new(x0, y0, w, h))
}

/// 纯函数：命中测试——点是否落在矩形内（左闭右开）。
#[allow(dead_code)]
pub fn contains(r: PickRect, x: i32, y: i32) -> bool {
    r.contains(x, y)
}

/// 短标题截断：≤ `max` 字符，超出时保留头尾（尾部用于区分同类窗口）。
pub fn truncate_label(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 4 {
        return s.chars().take(max).collect();
    }
    let head = max / 2;
    let tail = max - head - 1;
    let mut out: String = s.chars().take(head).collect();
    out.push('…');
    out.extend(s.chars().skip(n - tail));
    out
}

unsafe fn set_transparent(hwnd: HWND, on: bool) -> bool {
    if hwnd.is_invalid() {
        return false;
    }
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let flag = WS_EX_TRANSPARENT.0 as isize;
    let cur = ex & flag != 0;
    if cur == on {
        return false;
    }
    let next = if on { ex | flag } else { ex & !flag };
    let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
    );
    true
}

unsafe fn window_target(
    self_hwnd: HWND,
    x: i32,
    y: i32,
    padding_logical: i32,
    _expand: bool,
) -> Option<PickTarget> {
    let mut hwnd = WindowFromPoint(POINT { x, y });
    if hwnd.is_invalid() {
        return None;
    }
    if hwnd == self_hwnd {
        return None;
    }
    // 跳过本程序自己的其它窗口（工具条/贴图/菜单）
    if is_own_class(hwnd) {
        return None;
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    if !root.is_invalid() {
        hwnd = root;
    }
    if hwnd == self_hwnd || is_own_class(hwnd) {
        return None;
    }
    let mut r = RECT::default();
    let mut got = false;
    // DWM 扩展边框能剔除不可见的窗口投影，命中更贴近视觉边界
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut _ as *mut core::ffi::c_void,
        std::mem::size_of::<RECT>() as u32,
    )
    .map(|_| {
        if r.right > r.left && r.bottom > r.top {
            got = true;
        }
    });
    if !got {
        if GetWindowRect(hwnd, &mut r).is_err() || r.right <= r.left || r.bottom <= r.top {
            return None;
        }
    }
    let pad = scale_padding(self_hwnd, padding_logical);
    let mut rect = PickRect::new(
        r.left - pad,
        r.top - pad,
        r.right - r.left + pad * 2,
        r.bottom - r.top + pad * 2,
    );
    // 裁到虚拟屏（调用方可通过 normalize 再裁；这里先裁一次避免负宽高）
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    if let Some(n) = normalize(rect, PickRect::new(vx, vy, vw, vh)) {
        rect = n;
    } else {
        return None;
    }
    Some(PickTarget {
        hwnd: hwnd.0 as isize,
        rect,
        kind: TargetKind::Window,
        runtime_id: None,
        depth: 0,
        label: window_label(hwnd),
    })
}

unsafe fn element_target(
    elem: &IUIAutomationElement,
    depth: u32,
    padding_logical: i32,
) -> Option<PickTarget> {
    let r = elem.CurrentBoundingRectangle().ok()?;
    if r.right <= r.left || r.bottom <= r.top {
        return None;
    }
    let pad = scale_padding(HWND::default(), padding_logical);
    let mut rect = PickRect::new(
        r.left - pad,
        r.top - pad,
        r.right - r.left + pad * 2,
        r.bottom - r.top + pad * 2,
    );
    let (vx, vy, vw, vh) = crate::capture::virtual_screen_rect();
    rect = normalize(rect, PickRect::new(vx, vy, vw, vh))?;
    let name = elem
        .CurrentName()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let label = if name.trim().is_empty() {
        "元素".to_string()
    } else {
        format!("元素 · {}", truncate_label(name.trim(), 28))
    };
    let hwnd = elem.CurrentNativeWindowHandle().unwrap_or_default();
    Some(PickTarget {
        hwnd: hwnd.0 as isize,
        rect,
        kind: TargetKind::Element,
        runtime_id: None,
        depth,
        label,
    })
}

unsafe fn deepest_child(
    walker: &IUIAutomationTreeWalker,
    parent: &IUIAutomationElement,
    x: i32,
    y: i32,
    depth: u32,
    max_depth: u32,
) -> Option<IUIAutomationElement> {
    if depth >= max_depth {
        return None;
    }
    let mut best: Option<IUIAutomationElement> = None;
    let mut child = walker.GetFirstChildElement(parent).ok();
    let mut guard = 0;
    while let Some(c) = child {
        guard += 1;
        if guard > 256 {
            break;
        }
        let r = c.CurrentBoundingRectangle().unwrap_or_default();
        if r.right > r.left
            && r.bottom > r.top
            && x >= r.left
            && x < r.right
            && y >= r.top
            && y < r.bottom
        {
            // 继续下钻：返回更深层，若没有更深层则返回当前子元素
            best = deepest_child(walker, &c, x, y, depth + 1, max_depth).or(Some(c.clone()));
        }
        child = walker.GetNextSiblingElement(&c).ok();
    }
    best
}

unsafe fn is_self_element(elem: &IUIAutomationElement, self_hwnd: HWND) -> bool {
    if self_hwnd.is_invalid() {
        return false;
    }
    elem.CurrentNativeWindowHandle()
        .map(|h| h == self_hwnd)
        .unwrap_or(false)
}

unsafe fn is_own_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 128];
    let n = GetClassNameW(hwnd, &mut buf) as usize;
    if n == 0 {
        return false;
    }
    let cls = String::from_utf16_lossy(&buf[..n]);
    cls.starts_with("PixShot")
}

unsafe fn window_label(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let n = GetWindowTextW(hwnd, &mut buf) as usize;
    let title = String::from_utf16_lossy(&buf[..n]);
    let title = title.trim();
    if !title.is_empty() {
        return format!("窗口 · {}", truncate_label(title, 28));
    }
    let mut cbuf = [0u16; 128];
    let cn = GetClassNameW(hwnd, &mut cbuf) as usize;
    let cls = String::from_utf16_lossy(&cbuf[..cn]);
    if cls.is_empty() {
        "窗口".into()
    } else {
        format!("窗口 · {}", truncate_label(&cls, 28))
    }
}

/// 逻辑像素外扩 → 物理像素（按 overlay 窗口 DPI；拿不到用系统 DPI）。
fn scale_padding(hwnd: HWND, logical: i32) -> i32 {
    if logical <= 0 {
        return 0;
    }
    let dpi = unsafe {
        if hwnd.is_invalid() {
            windows::Win32::UI::HiDpi::GetDpiForSystem()
        } else {
            windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd)
        }
    }
    .max(96) as i32;
    (logical * dpi / 96).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_clips() {
        let screen = PickRect::new(0, 0, 1920, 1080);
        let n = normalize(PickRect::new(-200, -100, 300, 200), screen).unwrap();
        assert_eq!((n.x, n.y, n.w, n.h), (0, 0, 100, 100));
        // 负坐标虚拟屏（副屏在左/上）
        let screen = PickRect::new(-1920, -200, 3840, 1280);
        let n = normalize(PickRect::new(-2000, -300, 200, 200), screen).unwrap();
        assert_eq!((n.x, n.y), (-1920, -200));
    }

    #[test]
    fn normalize_rejects_empty() {
        let screen = PickRect::new(0, 0, 1920, 1080);
        assert_eq!(normalize(PickRect::new(100, 100, 0, 50), screen), None);
        assert_eq!(normalize(PickRect::new(100, 100, 1, 50), screen), None);
        assert_eq!(normalize(PickRect::new(3000, 3000, 10, 10), screen), None);
    }

    #[test]
    fn contains_edges() {
        let r = PickRect::new(2, 2, 4, 4);
        assert!(contains(r, 2, 2));
        assert!(contains(r, 5, 5));
        assert!(!contains(r, 6, 6), "右/下边界必须排除（左闭右开）");
        assert!(!contains(r, 1, 3));
    }

    #[test]
    fn label_truncates() {
        let s = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let out = truncate_label(s, 32);
        assert!(out.chars().count() <= 32);
        assert!(out.ends_with("ABCD"), "必须保留尾部：{out}");
    }

    #[test]
    fn pick_level_parse_and_fallback() {
        assert_eq!(PickLevel::parse("bogus"), None);
        assert_eq!(PickLevel::parse(""), Some(PickLevel::Region));
        assert_eq!(PickLevel::default(), PickLevel::Region);
        assert_eq!(PickLevel::parse("element"), Some(PickLevel::Element));
    }
}
