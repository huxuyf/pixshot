# PixShot —— 截图 / 贴图 / OCR（Rust · Windows 桌面工具）

仿 Snipaste 的 Windows 桌面小工具，纯 Rust 实现（Win32 GUI + Windows.Media.Ocr），零非 Rust
运行时依赖。技术路线沿用本仓库 `screen_recorder` 工程已使用的 `windows` crate 绑定。

## 功能

| 功能 | 说明 |
|------|------|
| **应用图标** | Fluent 风格自绘图标（蓝紫渐变圆角方形 + 白色取景框四角括号 + 焦点圆点），由 `build.rs` 内置纯 Rust 光栅化器生成多尺寸 .ico（16/24/32/48/64），经 windres 嵌入 exe 资源；托盘运行时 `LoadImage` 从自身模块加载同一图标（DPI 感知小图标尺寸） |
| **托盘常驻** | 启动后仅常驻任务栏托盘区域（无任何窗口弹出）；**F1 全局热键**（可在配置文件中改为 `Ctrl+Shift+S` 等）或**左键点击托盘**进入截图流程；右键托盘菜单仅含「退出」，点击后完全退出。具备：单实例保护、托盘注册失败重试（5 次）、`TaskbarCreated` 处理（Explorer 重启后自动恢复图标）、`NOTIFYICON_VERSION_4` 回调协议（按 SETVERSION 成败自动分派两种回调布局）、失败可见（日志 + 提示框） |
| **截图** | 遮罩暗化 → 拖动框选、单击选窗口、Enter 全屏、Esc 取消 → 操作工具栏（贴图 / 复制 / 保存 / OCR / 关闭）；**整个流程只截一次全屏**，后续裁剪都基于同一份快照，多显示器/高 DPI 下选区与结果严格一致。可保存为 **PNG / JPEG / BMP**（系统保存对话框，记住上次目录），或复制为 `CF_DIB` 位图 |
| **贴图** | 截图以无边框小窗钉在桌面**置顶**显示（Snipaste 式）；按住左键拖动、滚轮以光标为锚点缩放（0.05×–8×）、右键菜单（复制图像 / 保存为当前格式 / OCR / 重置缩放 / 关闭）、Esc 关闭。100% 缩放走 `StretchDIBits` 直出快路径，其余缩放走离屏合成 + `HALFTONE`（缩小）/ `COLORONCOLOR`（放大） |
| **OCR** | 调用系统自带 `Windows.Media.Ocr`（WinRT，支持中英文等多语言，无需安装任何 OCR 引擎）；后台线程识别，结果窗口展示识别文本，一键复制 `CF_UNICODETEXT`；从工具栏或贴图右键菜单均可发起。识别语言可在配置文件中固定（默认走系统语言 → zh-Hans-CN → en-US → 任意可用语言的回退链） |
| **提示与菜单** | 操作反馈使用非模态 Toast（右下角自动消失、点击即关），不再弹系统消息框打断操作；托盘与贴图右键共用同一套自绘 Fluent 菜单（支持多项目 / 分隔线 / 禁用态） |
| **配置** | `%APPDATA%\PixShot\config.ini` 纯文本配置：热键、保存格式、默认保存目录、OCR 语言、Toast 停留时长；手写解析器，非法值自动回退默认并写诊断日志 |

## 构建与运行

要求：Windows 10 1809+ / Windows 11，已安装 [Rust](https://rustup.rs/)（`x86_64-pc-windows-msvc`
或 `x86_64-pc-windows-gnu` 工具链均可）。

```powershell
cd pixshot

# 开发构建
cargo build

# 发布构建（推荐）
cargo build --release

# 运行（GUI：常驻托盘，不弹窗；按 F1 或左键托盘图标开始截图）
cargo run --release

# 无 GUI 冒烟测试：全屏截图 → 保存 PNG → OCR 识别（验证核心链路）
cargo run --release -- --smoke

# 单元测试 + 集成测试（46 项，纯逻辑、不触碰桌面）
cargo test

# 本地 CI：check → test → build → release → 体积护栏 → E2E（加 -SkipE2E 跳过最后一步）
pwsh -File scripts\ci.ps1
```

产物：`target\release\pixshot.exe`（单文件，约 2.2 MB，无 DLL 依赖，可直接分发）。

### 图标嵌入与构建工具链

应用图标（含 16/24/32/48/64 五个尺寸）在 `build.rs` 中用纯 Rust 光栅化器生成，再交给
工具链自带的资源编译器嵌入 exe：

| 工具链 | 资源编译器 | 说明 |
|---|---|---|
| `x86_64-pc-windows-gnu` | `windres.exe`（MinGW/MSYS2） | 若 PATH 中没有，会依次尝试 `C:\msys64\mingw64\bin`、`C:\msys64\usr\bin` |
| `x86_64-pc-windows-msvc` | `rc.exe`（Windows SDK） | 需从「x64 Native Tools Command Prompt for VS」启动，或确保 SDK `bin` 在 PATH 中 |

两者都找不到时**构建直接失败**并打印解决方式（早期版本会静默跳过图标，导致托盘显示空白
方块，排查成本很高）。图标几何代码与 `tests/icon_geometry.rs` 共用同一份
`src/icon_geometry.rs`，不存在「测试一份、构建另一份」的漂移。

不需要图标时显式跳过：`PIXSHOT_NO_ICON=1 cargo build`（托盘回退系统默认图标）。

### ⚠ 若托盘图标不出现 / F1 无反应：检查 exe 的「完整性标签」

已定位的真实成因（实测复现并验证修复）：**exe 文件带有 `Low Mandatory Level`
强制完整性标签**时，从它启动的进程以**低完整性**运行，而 Windows 的 **UIPI** 机制会
拒绝低完整性进程访问资源管理器（中完整性）的托盘窗口 → `Shell_NotifyIcon` 返回
**Win32 错误 5（拒绝访问）**，全局热键也无法送达。

沙箱类工具（含本仓库开发环境）写入目录时会打标签，并被目录中的 exe 继承，因此
表现为「**双击运行也失败**」。实测对照：

| exe 位置 | 文件标签 | 进程完整性 | `NIM_ADD` |
|---|---|---|---|
| 被打标的工作区内 | `Low Mandatory Level` | Low (0x1000) | **失败（错误 5）** |
| 复制到普通目录后 | 无 | Medium (0x2000) | **成功（错误 0）** |

**两种修复方式（任选其一）**：

```powershell
# 方式 A（推荐，一次性修复整个目录，且后续新生成的文件自动继承 Medium）
pwsh -File scripts\fix_integrity.ps1
#  等价手写命令（必要时以管理员运行）：
#  icacls "<程序目录>" /setintegritylevel (OI)(CI)Medium /T

# 方式 B：把 pixshot.exe 复制到别处（如桌面新建文件夹）再运行
```

程序启动时会把自身完整性写入诊断日志并弹窗提示该原因，便于快速定位。

### 界面字体

界面字体按 **CJK 覆盖优先** 选择：`Microsoft YaHei UI` → `Microsoft YaHei` →
`Segoe UI Variable Text` → `Segoe UI` → `SimSun`。
原因：本程序界面文本为中文，而 Segoe UI / Segoe UI Variable **不含 CJK 字形**，
仅靠 GDI 字体链接在不同系统配置下不可靠（表现为汉字乱码/方框）。
选中的字体族会写入诊断日志（`[theme] UI 字体族 = …`）。

### 环境变量开关

| 变量 | 作用 |
|---|---|
| `PIXSHOT_FORCE_HOOK=1` | 强制使用**低级键盘钩子**监听热键（默认优先 `RegisterHotKey`，仅在其失败时自动回退到钩子）。适用于热键被其他程序占用、或热键机制异常的兼容性场景 |
| `PIXSHOT_NO_UI_ALERTS=1` | 屏蔽启动失败提示框（自动化测试用） |
| `PIXSHOT_NO_ICON=1` | 构建期跳过图标嵌入（见上一节；产物托盘图标回退为系统默认） |

### 配置文件

首次启动会在 `%APPDATA%\PixShot\config.ini` 生成带注释的默认配置（UTF-8）；无 `APPDATA`
时回退到 exe 同目录。**修改后重启生效**，删除该文件即恢复默认。

```ini
hotkey   = F1                ; 全局热键，支持 Ctrl / Alt / Shift / Win 组合，如 Ctrl+Shift+S
format   = png               ; 保存格式：png / jpg / bmp
save_dir =                   ; 默认保存目录，留空则每次打开对话框时从系统默认位置开始
ocr_lang =                   ; OCR 语言，留空则自动回退（系统语言 → zh-Hans-CN → en-US → 任意）
toast_ms = 1600              ; 操作提示的停留毫秒数
```

解析器为手写实现（不引入 ini/toml 依赖）：未知键忽略、非法值回退默认值，两类情况都会
写入诊断日志，不会因配置写坏而启动失败。保存成功后自动记住本次目录（回写 `save_dir`）。

> OCR 需要 Windows 语言包带"文字识别"可选功能（中文/英文系统一般已内置）。
> 若提示没有可用语言包：设置 → 时间和语言 → 语言和区域 → 添加语言（如"英语(美国)"或
> "中文(简体)"）→ 语言选项 → 勾选"文字识别"。

## 操作方式

- **启动**：程序常驻任务栏托盘，不弹出任何窗口
- **触发截图**：全局热键（默认 **F1**，可在配置文件中修改）；或左键点击托盘图标
- **框选**：拖动矩形；**单击 = 选中光标下的窗口**（自动取该窗口的顶层窗口区域）；Enter = 全屏；Esc = 取消
- **工具栏**：贴图 / 复制 / 保存 / OCR 识别 / 关闭（也可用键盘 Tab 与 Enter 操作）
- **贴图窗口**：左键拖动移动；滚轮缩放（以光标为锚点）；**双击左键退出该贴图**；右键菜单含 OCR；Esc 关闭
- **OCR 结果窗口**：只读多行文本 + 复制文本按钮；标题栏显示字符数；复制成功以右下角 Toast 提示
- **退出**：右键托盘图标 → 点「退出」（自绘 Fluent 弹窗）

## 故障排查

程序每次启动都会写一份诊断日志（**exe 同目录** `pixshot_diag.log`；该目录不可写时回退
`%TEMP%`），逐条记录 图标加载 / 托盘注册（含每次重试与 Win32 错误码）/ 热键注册 /
单实例判定 / **托盘回调原始 wParam 与 lParam** 的结果。遇到问题先看它。

### 托盘右键菜单的协议要点（实测，易踩坑）

`NIM_SETVERSION(NOTIFYICON_VERSION_4)` 之后，`uCallbackMessage` 的**实测**布局是：

| 字段 | 含义 |
|---|---|
| `wParam` | `MAKELONG(x, y)` —— 低字 x 坐标，高字 y 坐标 |
| `lParam` | `MAKELONG(事件, 图标ID)` —— 低字为鼠标消息/事件，高字为图标 ID |

（实测样本：在 (1492,922) 右键 → `wp=0x39A05D4`、`lp=0x1007B`，
即 x=1492、y=922、事件=0x7B=WM_CONTEXTMENU、uID=1。**注意与"事件在高字"的直觉相反**。）

另外两点实测结论：

1. **一次右键会先后投递两条回调**（`WM_RBUTTONUP` 0x205 与 `WM_CONTEXTMENU` 0x7B），
   必须去重，否则会嵌套弹出两层菜单（现象：点了一项菜单仍在、程序不退出）。
2. **不要用 `TrackPopupMenuEx` 做托盘菜单**：它要求宿主窗口能成为前台窗口，
   而托盘宿主是隐藏窗口，实测会立即返回 0 且**完全不显示菜单**。
   本程序改用自绘 Fluent 弹窗 + `SetCapture`，不依赖前台权限。

### 为什么"托盘图标没有右键菜单"特别难查

- 该协议**没有文档化的权威布局可依赖**，且失败**完全静默**（`Shell_NotifyIconW`
  只返回 BOOL、菜单 API 失败也只返回 0），没有任何错误码指向真正原因。
- **合成消息测试会自证错误**：用 `PostMessage` 按自己的理解构造回调，
  只能验证"代码符合自己的假设"，无法暴露协议理解错误。
  本项目正是在改用 **DPI 感知 + 先展开 `^` 溢出面板 + 注入真实鼠标事件** 之后，
  才从日志中读出真实布局（`wParam` 装坐标、`lParam` 装事件+ID）并修复。
- **图标默认在 Windows 11 的 `^` 溢出区**（注册表
  `HKCU\Control Panel\NotifyIconSettings` 中该 exe 的 `IsPromoted` 为缺失/0），
  若未展开 `^` 就右键，点到的是任务栏自身菜单（自然没有「退出」）。
  把图标从 `^` 拖到托盘区可固定显示（Windows 记忆该设置）。

一键真实点击回归：`pwsh -File scripts\verify_tray_realclick.ps1
（DPI 感知、自动展开 `^`、注入真实右键并验证「退出」生效）`

| 现象 | 原因 | 解决 |
|---|---|---|
| 进程在运行，**托盘没有图标** | ① 由受限/沙箱终端启动（`Shell_NotifyIcon` 返回错误 5「拒绝访问」）；② Windows 11 默认把新图标收进「^」溢出区；③ Explorer 重启导致图标丢失 | ① 改为在「文件资源管理器」中双击运行；② 点任务栏 `^` 找到图标并拖到托盘区固定；③ 本版本已监听 `TaskbarCreated` 自动恢复 |
| **按 F1 无反应** | ① F1 被其他程序占用（`RegisterHotKey` 失败）；② 受限桌面环境导致热键无法送达 | ① 本版本自动回退到低级键盘钩子，也可显式设 `PIXSHOT_FORCE_HOOK=1`；② 改用资源管理器启动 |
| 启动即退出并提示「已在运行」 | 单实例保护生效：已有实例持锁（旧实例可能仍占用 F1，导致新实例按键无效） | 先在任务管理器中结束所有 `pixshot.exe`，再启动 |
| 托盘图标在 Explorer 重启后消失 | 外壳重启会丢弃全部托盘图标 | 本版本自动重新注册，无需操作 |

一键采集诊断信息：`pwsh -File scripts\diag_tray_hotkey.ps1`

## 项目结构

```
pixshot/
├── Cargo.toml            # windows 0.58 绑定（Win32 + WinRT OCR），无其他依赖
├── build.rs             # 应用图标：纯 Rust 光栅化器生成 .ico + windres/rc.exe 嵌入 exe 资源
├── src/
│   ├── main.rs           # 入口：进程级初始化、截图流程编排（含 panic 兜底与重入保护）、--smoke
│   ├── tray.rs           # 托盘常驻 + 全局热键（RegisterHotKey → 低级钩子回退）+ 单实例互斥
│   ├── capture.rs        # GDI BitBlt 截屏（CreateDIBSection 直读、虚拟屏幕/多显示器、crop 裁剪）
│   ├── overlay.rs        # 全屏框选遮罩（一次捕获的快照还原选区，Snipaste 式暗化）
│   ├── toolbar.rs        # 截图后的操作工具栏（贴图/复制/保存/OCR/关闭，owner-draw）
│   ├── pin.rs            # 置顶贴图窗口（拖动/滚轮缩放/离屏合成/右键菜单，像素级完整呈现）
│   ├── ocr.rs            # Windows.Media.Ocr 封装（语言回退链、超尺寸降采样）
│   ├── results.rs        # OCR 结果窗口（无边框标题栏 + 后台线程 + 结果槽回填）
│   ├── clipboard.rs      # Win32 剪贴板：CF_DIB 图像 / CF_UNICODETEXT 文本（RAII 守卫）
│   ├── imageio.rs        # WIC 编码 PNG/JPEG/BMP + 系统保存对话框（记住上次目录）
│   ├── config.rs         # config.ini 读写与解析（热键/格式/目录/OCR 语言/Toast 时长）
│   ├── menu.rs           # 自绘 Fluent 弹出菜单（托盘与贴图共用，支持分隔线与禁用态）
│   ├── toast.rs          # 非模态 Toast 提示（替代 MessageBoxW，不打断操作）
│   ├── theme.rs          # Fluent 视觉基建：深/浅色板、系统主题跟随、DPI 字体、DWM 特效
│   ├── util.rs           # 窗口类注册/owner-draw 按钮/悬停动画引擎/DPI 换算/重入与退出标志
│   ├── icon_geometry.rs  # 图标的纯几何定义（被 build.rs 与 tests/ 共用，无 Windows API）
│   ├── diag.rs           # 轻量诊断日志（托盘/热键/图标/配置等易失败调用均记录）
│   └── error.rs          # 统一错误类型（沿用 screen_recorder 模式）
├── tests/
│   └── icon_geometry.rs  # 图标几何集成测试（14 项：端点/对称性/越界/常量约束）
├── scripts/
│   ├── ci.ps1                # 本地 CI 流水线：check→test→build→release→体积护栏→E2E
│   ├── gui_e2e_msg_test.ps1  # GUI 端到端（消息驱动版，自带 OCR 文字靶标）
│   ├── gui_e2e_test.ps1      # GUI 端到端（物理鼠标/键盘版）
│   ├── diag_tray_hotkey.ps1  # 托盘/热键诊断采集
│   ├── fix_integrity.ps1     # 修复目录完整性标签（托盘注册失败错误 5 的根因）
│   ├── verify_tray_realclick.ps1  # 真实鼠标点击托盘菜单回归（DPI 感知 + 展开 ^ 溢出区）
│   ├── verify_fluent.ps1     # Fluent 视觉像素级验证
│   ├── verify_fix5b.ps1      # 托盘回调协议 + 菜单弹出的定点回归
│   ├── measure_baseline.ps1  # 性能基线测量（体积/启动/内存/CPU）
│   └── final_accept.ps1      # 交付验收汇总（逐项汇总上述结果）
└── e2e_artifacts/        # E2E 测试输出（transcript 与截图存档）
```

## 端到端自动化测试（GUI E2E）

提供两个版本，覆盖同一核心流程并逐项断言：

| 脚本 | 驱动方式 | 适用环境 | 额外覆盖 |
|------|----------|----------|----------|
| `scripts/gui_e2e_test.ps1` | 真实鼠标/键盘注入 | 用户自己的终端（需可注入输入） | 贴图拖动、右键菜单、保存对话框 |
| `scripts/gui_e2e_msg_test.ps1` | `SendMessage/PostMessage` 直接驱动窗口过程 | 任意环境（含禁止输入注入的受限沙箱） | 自带确定性 OCR 文字靶标 |

两个脚本各自维护一份**独立的断言清单**，逐项打印 `PASS/FAIL`，退出码 0 表示全过：
消息驱动版 **16 项**、物理驱动版 **15 项**（后者把「窗口拖动」「保存对话框」拆成独立断言，
不重复校验剪贴板图像）。覆盖的能力点如下（同一能力点可能对应多条断言，例如「置顶」同时
校验扩展样式与实际 `WS_EX_TOPMOST` 位）：

1. 启动 → **托盘常驻（无遮罩/主窗口弹出）**
2. **热键路径（WM_HOTKEY）触发截图流程** → 遮罩出现
3. 框选（拖动）完成 → 遮罩按选区销毁 → 工具栏弹出且**实际选区与预期一致**（600×400）
4. 工具栏含 贴图/复制/OCR 按钮
5. 复制 → 剪贴板含 **CF_DIB 位图**
6. OCR 结果窗口打开 → 后台识别完成 → **识别文本与靶标一致**（中/英/数字）
7. 贴图窗口创建于**原截图位置**、初始 100% 缩放、**WS_EX_TOPMOST 置顶**
8. **滚轮缩放**（两次放大精确 ×1.21）
9. 「复制文本」→ **剪贴板文本与窗口一致**
10. **Esc 关闭贴图**

用法（约 30~60 秒，运行期间接管鼠标键盘/桌面窗口，请勿操作）：

```powershell
cd pixshot
pwsh -File scripts\ci.ps1                                 # 推荐：CI 流水线（含下列 E2E）
pwsh -File scripts\ci.ps1 -SkipE2E                        # 只跑静态检查与构建
pwsh -File scripts\gui_e2e_msg_test.ps1                   # 消息驱动版（默认 release exe）
pwsh -File scripts\gui_e2e_test.ps1                       # 物理驱动版（默认 debug exe）
pwsh -File scripts\gui_e2e_test.ps1 -Exe target\release\pixshot.exe
```

输出：PASS/FAIL 清单 + 退出码（0=全部通过）；详细记录与 `pin_visible.png`
（贴图置顶显示存档）见 `e2e_artifacts\`。消息驱动版还会在选区内自绘一个
确定性文字窗口（"PixShot OCR E2E / 识别测试文本 ABC 12345"）供 OCR 断言，
不依赖桌面上恰好有文字。

> 测试结果示例（本仓库开发机，Windows 11 26300，125% DPI）：
> `[消息驱动版] 测试结束：16 项通过，0 项失败`，OCR 识别输出
> `PixShot OCR E2E 识 别 测 试 文 本 ABC 12345`。
>
> 说明：物理驱动版依赖向系统注入鼠标/键盘输入，在禁止输入注入的受限宿主
> （如某些自动化沙箱）中会被拦截——此时请使用消息驱动版。

## 设计说明

- **界面（Windows 11 Fluent，零新依赖）**：不换框架、纯 Win32 实现——8px 圆角
  （`DWMWA_WINDOW_CORNER_PREFERENCE`）、自绘 1px 窗口轮廓（半透明观感）、Mica 背景
  （`DWMWA_SYSTEMBACKDROP_TYPE` + `DwmExtendFrameIntoClientArea` 黑色显影，不支持时
  自动降级为 Mica 同色实色）、深/浅色实时跟随系统（`AppsUseLightTheme` 注册表 +
  `WM_SETTINGCHANGE`）、Segoe UI Variable 字体（探测回退 Segoe UI，DPI 感知）、
  无边框自绘标题栏（关闭/最小化按钮，悬停 0.15s 过渡动画——动画定时器仅在鼠标
  位于窗口内时存在，空闲 CPU=0）、圆角 owner-draw 按钮与 OCR 结果窗口深色滚动条
  （`SetWindowTheme`）。**截图工具栏固定深色皮肤**（不随系统浅色变化：遮罩本为
  暗色、视觉连续，且浅色 Mica 会采样壁纸颜色导致白壁纸上几乎隐形；同类截图
  工具 Snipaste/ShareX 的工具栏亦为深色）。贴图窗口保持直角且无 `WS_THICKFRAME`，
  确保图像像素级完整呈现（该组合实测在贴图失去前台后会被 DWM 绘制 ~8px 框架
  视觉带，详见 pin.rs 注释）。
- **截屏管线**：整个截图流程**只做一次全屏 `BitBlt`**，结果以 `Arc<[u8]>`（BGRA、自上而下）
  持有；遮罩还原、选区裁剪、贴图、OCR 全部基于同一份快照的切片，避免「截一次 → 等待 →
  再截一次」带来的闪烁与选区漂移。位图用 `CreateDIBSection` 直接作为 `BitBlt` 目标，
  像素落在自持内存里（省掉一次位图创建 + `GetDIBits` 全量拷贝），alpha 在捕获阶段统一置 255。
  （录屏用的 DXGI 重复在 `screen_recorder` 工程中。）
- **并发与生命周期**：OCR 后台线程通过 `Arc<Mutex<Option<_>>>` 结果槽回填，并以
  `AtomicBool` 存活标志决定是否 `PostMessage` —— 窗口销毁后不再向已释放的 `Box<Data>`
  投递消息（早期版本的悬垂写入隐患）。剪贴板用 RAII 守卫，任何提前 return 都会
  `CloseClipboard()`；截图流程用 `BusyGuard`（`Drop` 自动复位）防重入，并在
  `extern "system"` 边界内 `catch_unwind`，panic 会写入诊断日志而不是静默消失。
- **菜单与提示**：托盘右键与贴图右键共用 `menu.rs` 一套自绘 Fluent 菜单
  （`SetCapture` + `GWLP_USERDATA`，支持多项目/分隔线/禁用态）。不用
  `TrackPopupMenuEx`：它要求宿主能成为前台窗口，而托盘宿主是隐藏窗口，实测静默失败。
  操作反馈统一走 `toast.rs` 的非模态 Toast，不再用 `MessageBoxW` 打断操作流。
- **配置**：`config.ini` 由手写解析器读取（零依赖），非法值回退默认并记日志，
  配置损坏不会导致启动失败。
- **OCR**：`Windows.Media.Ocr` 系统自带、离线、开箱即用（回退链：用户语言 → zh-Hans-CN →
  en-US → 任意可用语言），超过 `MaxImageDimension` 自动等比降采样。
- **PNG**：WIC 编码（`IWICImagingFactory`），无第三方图像库。
- **DPI**：进程级 Per-Monitor-V2 感知，所有坐标/截图均为物理像素；各窗口布局按
  `GetDpiForWindow` 换算并响应 `WM_DPICHANGED`（跨不同缩放率显示器拖动时不会错位），
  125% 缩放下贴图与框选均精确。
- **防误触**：截图流程用 `BusyGuard` 原子标志拒绝重入（异常路径由 `Drop` 自动释放）；
  托盘退出经原子标志传播出嵌套消息循环，避免 `WM_QUIT` 被内层循环吞掉。
- **热键的两条路径**（按可靠性排序）：① `RegisterHotKey` 主路径 —— 只在本进程收到
  `WM_HOTKEY`，无隐私面；② `WH_KEYBOARD_LL` 低级钩子兜底 —— **仅在主路径失败或显式设置
  `PIXSHOT_FORCE_HOOK=1` 时启用**。钩子有两点固有风险，故不作默认：回调若超过系统
  `LowLevelHooksTimeout`（默认约 300ms）会被**静默摘除**；且它能看到全系统按键，
  部分安全软件会直接拦截。钩子路径下修饰键需自行用 `GetAsyncKeyState` 校验
  （`KBDLLHOOKSTRUCT` 的 `flags` 不反映 Ctrl/Alt/Shift 状态），否则把 `Ctrl+F1` 误判为 F1。
- **性能**：启动时线程级禁用 IME（`ImmDisableIME`，本应用无文本输入），阻止输入法 DLL
  注入——实测稳态内存从 ~58MB 降至 ~19MB、空闲 CPU 从 ~1% 降至 0；贴图外框矩形缓存
  避免高频 `GetWindowRect` 内核调用。

## 性能预算（硬性约束验收）

| 指标 | 改造前基线 | Fluent 版实测 | 预算 | 判定 |
|---|---|---|---|---|
| EXE 体积 | 1.97 MB | **2.17 MB（+10.2%，含嵌入图标资源、托盘/热键加固、配置与自绘菜单）** | ≤ +20%（≤2.37 MB） | ✓ |
| 冷启动（→托盘就绪） | 63–78 ms | **78–141 ms**（含测量轮询粒度 ~60ms） | ≤ 300 ms | ✓ |
| 稳态内存（托盘常驻） | 50.6–58.6 MB | **10.29 MB** | ≤ 50 MB | ✓ |
| 稳态内存（全屏贴图） | 72.38 MB | **27.98 MB** | ≤ 50 MB | ✓ |
| 空闲 CPU（无输入） | 0.78–0.94% | **0.000%**（3×10s 采样窗口） | ≈ 0 | ✓ |
| 第三方依赖 | 0 crate | **0 新增**（图标为 build.rs 纯 Rust 生成，仅调用工具链自带 windres） | 零/极轻 | ✓ |

（实测脚本：`scripts\measure_baseline.ps1` → `perf_baseline.txt`；
视觉像素级验证：`scripts\verify_fluent.ps1` → `fluent_verify.txt`。
**体积回归护栏**已固化进 CI：`scripts\ci.ps1` 在 release 构建后断言
`pixshot.exe ≤ 2.6 MB`，超出即失败并提示检查新增依赖/未裁剪的 crate feature。）

## 已知限制

- 仅支持 Windows（任务要求；GDI/WinRT/WIC 均为 Windows API）
- OCR 语言取决于系统语言包；无可用语言包时给出中文引导提示
- 多显示器：框选/贴图坐标基于虚拟屏幕，支持负坐标副屏；贴图初始缩放以主屏 85% 为上限
- 贴图尺寸调整仅支持滚轮缩放（不提供边缘拖拽调整——为保图像像素级完整，
  窗口不再使用 WS_THICKFRAME，见设计说明）
- Mica/圆角需 Windows 11 22H2+；Win10 自动降级为现代深浅色平面（深色标题栏仍生效）
- 配置文件修改后需**重启生效**（当前不做热重载；`config::reload()` 已预留但未接 UI）
- 低级键盘钩子兜底模式（`PIXSHOT_FORCE_HOOK=1` 或 `RegisterHotKey` 失败时）有固有风险：
  回调超时会被系统静默摘除，部分安全软件会拦截钩子安装。默认路径不依赖钩子
- 单实例保护为「第二个实例直接退出并提示」，不做「唤起已有实例的截图」（无 IPC 通道）
- 发布的 exe 未签名，分发到其他机器会遇到 Windows 安全警告——完整解决方案见 [SIGNING.md](SIGNING.md)（验证/解除锁定/SmartScreen/代码签名全流程）
