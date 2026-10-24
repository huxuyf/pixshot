# PixShot 项目评估报告

> 评估对象：`C:\Users\user\Desktop\pixpin\pixshot`（v0.1.0）
> 评估方式：全量源码通读（13 个模块 / 4327 行）+ 构建验证（`cargo check` 通过，零警告）+ 测试与存档交叉核对
> 评估日期：2026-10-24

---

## 一、结论速读

**这是一个完成度相当高的个人级 Windows 桌面工具**：核心链路（热键 → 框选 → 截图 → 贴图/复制/保存/OCR）闭环可用，工程纪律明显高于同类个人项目——有诊断日志、有性能预算、有双版本 GUI E2E、有真实协议实测记录。

**它的主要风险不在功能，而在三处**：

1. **交付形态风险**：项目不是 git 仓库、零单元测试、无 CI，所有质量证据依赖手工 PowerShell 脚本和几份 txt 存档；
2. **架构演进瓶颈**：`static mut` 全局状态 + 固定像素布局 + 主流程单文件 987 行，功能一旦继续叠加会迅速失控；
3. **若干真实缺陷**：剪贴板失败路径会留下未关闭的剪贴板句柄（影响全系统）、托盘旧协议分支是死代码且坐标解析错误、OCR 结果窗口先关后回填会漏内存。

按"能量产的玩具 → 可信赖的工具"这条标尺衡量，它已完成约 **70%**；剩下 30% 几乎全部是**可靠性与可维护性**，不是功能。

---

## 二、项目定位

### 2.1 它是什么

| 维度 | 内容 |
|---|---|
| 产品形态 | Windows 托盘常驻小工具，仿 Snipaste / PixPin 的极简子集 |
| 代码定位 | 单人可维护的 Win32 直写实验 + 可交付的日常截图工具，双重身份 |
| 技术路线 | 纯 Rust + `windows` crate 直调 Win32/WinRT，**不使用任何 GUI 框架**（无 egui/tauri/iced/slint） |
| 依赖策略 | 零第三方运行时依赖（直接依赖仅 `windows 0.58`，锁文件共 20 个包，全部为 windows 生态） |
| 分发形态 | 单 exe（实测 2.06 MB），无 DLL、无安装器、无运行时 |
| 明确不做 | 跨平台、录屏、GIF、标注编辑、云同步 |

### 2.2 定位判断

代码里有一条清晰的价值主张：**用系统原生能力换取极致轻量与零依赖**。三个技术选型印证了这一点——

- OCR 用系统自带 `Windows.Media.Ocr`，省掉 Tesseract/PaddleOCR 等几十 MB 的模型；
- PNG 编码用 WIC，省掉 `image` / `png` crate；
- 图标在 `build.rs` 里用纯 std 手写光栅化器渲染，连图标文件都不进仓库。

这条路线带来真实的收益（10.29 MB 稳态内存、空闲 CPU 0.000%），代价是**所有 UI 都得用 GDI 手绘**——这决定了后续所有可维护性问题都源于此，属于自觉的技术负债，不是失误。

### 2.3 与同类工具的差距（功能面）

| 能力 | PixShot | Snipaste | 差距性质 |
|---|---|---|---|
| 框选 / 全屏 / 选窗口 | ✅ | ✅ | 对齐 |
| 贴图置顶 / 拖动 / 缩放 | ✅ | ✅ | 对齐（缺边缘拖拽调整，有设计理由） |
| OCR | ✅（系统引擎） | ❌（付费版/插件） | **局部领先** |
| 热键自定义 | ❌ 硬编码 F1 | ✅ | 定位内缺失 |
| 保存格式 | ❌ 仅 PNG | ✅ PNG/JPG/WebP | 定位内缺失 |
| 标注（箭头/文字/马赛克） | ❌ | ✅ | 定位外 |
| 拖拽到其他应用 | ❌ | ✅ | 定位内缺失 |
| 开机自启 / 设置界面 | ❌（无任何配置项） | ✅ | 定位内缺失 |
| 多语言界面 | ❌ 仅中文 | ✅ | 定位外 |

**判断**：核心交互已对齐，缺的是"日常使用的毛边"——配置能力、格式选择、拖拽输出。这三项才是从"能用的 Demo"变成"天天用的工具"的关键。

---

## 三、核心功能与实现位置

| 功能 | 实现要点 | 代码位置 |
|---|---|---|
| 托盘常驻 + 单实例 | 隐藏窗口承载托盘/热键；`CreateMutexW` 命名互斥体；`TaskbarCreated` 自恢复；5 次重试 + V4 回调协议 | `main.rs:228-534` |
| F1 全局热键 | 主路径 `RegisterHotKey`，失败自动降级 `WH_KEYBOARD_LL` 低级钩子；`PIXSHOT_FORCE_HOOK` 可强制 | `main.rs:258-324` |
| 框选遮罩 | 预截整屏 → 分层窗口 160 alpha 暗化 → 选区 BitBlt 还原内容；单击选窗口（先隐藏自身再 `WindowFromPoint`）；局部失效 + 双缓冲防闪 | `overlay.rs:68-391` |
| 截图 | GDI `BitBlt` + `GetDIBits`，BGRA 自上而下，物理像素，支持负坐标副屏 | `capture.rs:67-122` |
| 操作工具栏 | 无边框自绘标题栏 + 固定深色皮肤 + owner-draw 圆角按钮 + 悬停 0.15s 插值；`WDA_EXCLUDEFROMCAPTURE` 防自捕获 | `main.rs:602-935` |
| 贴图窗口 | 置顶无边框；拖动走 `WM_NCLBUTTONDOWN`+`HTCAPTION`；滚轮以光标为锚点缩放 0.05×–8×；外框矩形缓存 | `pin.rs:61-393` |
| OCR | WinRT 引擎 4 级语言回退；超 `MaxImageDimension` 等比降采样；后台线程 + `WM_APP` 回填 | `ocr.rs`、`results.rs:64-113` |
| OCR 结果窗口 | 自绘标题栏 + Mica + 8px 边缘缩放命中 + 只读 EDIT + 一键复制 | `results.rs:249-487` |
| 剪贴板 | `CF_DIB`（手工行翻转 BGRA）与 `CF_UNICODETEXT`，打开重试 5 次 | `clipboard.rs` |
| PNG 保存 | WIC 编码 + `GetSaveFileNameW` 系统对话框 + 默认文件名 `截图_YYYYMMDD_HHMMSS.png` | `imageio.rs` |
| Fluent 视觉 | 深浅色跟随注册表 + `WM_SETTINGCHANGE`；圆角/Mica/DWM 探测式降级；CJK 优先字体族探测 | `theme.rs` |
| 托盘菜单 | **自绘** Fluent 弹窗 + `SetCapture`（因 `TrackPopupMenuEx` 对隐藏宿主窗口失效） | `traymenu.rs` |
| 诊断 | exe 同目录日志（不可写回退 TEMP）+ 进程完整性级别自检 | `diag.rs` |
| ACL 修复 | `icacls` 去除 Low 完整性标签（UIPI 导致托盘失败的真实根因） | `scripts/fix_integrity.ps1` |

---

## 四、技术架构

### 4.1 进程与线程模型

```
主线程（唯一 GUI 线程，Per-Monitor-V2 DPI）
├── 主消息循环 ── 隐藏窗口 PixShotMain（托盘 + 热键宿主）
│     ├── WM_HOTKEY       → screenshot_flow（APP_BUSY 防重入）
│     │     └── 嵌套模态循环 #1：PixShotOverlay（框选）
│     │     └── 嵌套模态循环 #2：PixShotToolbar（操作栏）
│     ├── WM_APP_TRAY     → 托盘回调分发（左键截图 / 右键菜单）
│     ├── WM_CONTEXTMENU  → 兜底右键
│     └── TaskbarCreated  → 重建托盘图标
├── 贴图窗口 PixShotPin × N（WM_DESTROY 释放 PinData + 图像堆块）
├── OCR 窗口 PixShotOcrResult
└── 后台线程：std::thread::spawn → ocr::recognize（COM MTA）→ PostMessage(WM_OCR_DONE)

辅助：WH_KEYBOARD_LL 键盘钩子（仅热键失败时安装，回调内只 PostMessage）
```

**关键设计**：所有 UI 状态都由"嵌套模态消息循环"驱动，`PostQuitMessage` 在内层循环被消费即返回上一层。这是经典 Win32 模式，实现干净，但代价是**退出信号必须靠 `APP_QUIT` 原子标志跨层传播**（`main.rs:580`）——属于该模式的固有复杂度。

### 4.2 模块依赖

```
                    main.rs（编排层：托盘/热键/流程/工具栏）
                   ╱    │      │      │       │        ╲
        overlay ──╱  capture  pin  results  traymenu   diag
            │         │       │      │        │        │
            └──── theme ──────┴──────┴────────┘        │
                        │                              │
                     util（Win32 辅助/字体/按钮/动画）───┘
                        │
                     error（统一错误类型）
        clipboard / imageio / ocr  ← 末梢能力，单向依赖 error
```

依赖方向清晰：`main` 是唯一编排者，能力模块之间只有 `pin → results/clipboard/imageio` 三处横向调用，无环。

### 4.3 关键工程决策（值得保留的部分）

| 决策 | 理由 | 评价 |
|---|---|---|
| Win32 直写、不引 GUI 框架 | 体积/内存/启动全赢 | 正确，但需接受 GDI 手绘的长期成本 |
| 物理像素坐标系 | 125% DPI 下框选与贴图精确 | 正确且必要 |
| 诊断日志先行 | `Shell_NotifyIcon` 只有 BOOL，失败必然静默 | **本项目最有价值的工程习惯** |
| 进程完整性自检 | UIPI 拒绝低完整性进程访问托盘，与代码无关 | 定位了真实根因，避免误判 |
| 自绘托盘菜单 | `TrackPopupMenuEx` 对隐藏宿主立即返回 0 | 弃用系统 API 的正确判断 |
| 截图工具栏固定深色 | 浅色 Mica 采壁纸色，白壁纸下工具栏隐形 | 有依据的偏离"跟随系统" |
| 贴图禁用 `WS_THICKFRAME` | 实测 DWM 会绘 ~8px 框架带内缩内容 | 像素完整性优先 |
| 双版本 E2E（消息驱动 / 物理注入） | 受限环境无法注入输入 | 设计正确 |

这些决策在 README 中都留下了**实测证据**（含反例与失败现象），是项目最有复用价值的知识资产。

### 4.4 质量基建现状

| 项 | 状态 | 说明 |
|---|---|---|
| 编译 | ✅ 通过，零警告 | `cargo check` 实测 49.5s（本机 GNU 工具链） |
| clippy | ❌ 未安装 | `cargo-clippy` 缺失，从未执行过 lint |
| 单元测试 | ❌ 0 个 | `grep #[test]` 无结果 |
| GUI E2E | ✅ 2 套 / 16 项断言 | 消息驱动版实测 16/16 PASS（存档见 `e2e_artifacts/`） |
| 性能预算 | ✅ 有硬指标 | EXE ≤+20%、冷启动 ≤300ms、内存 ≤50MB、CPU≈0 |
| 视觉验证 | ✅ 像素级脚本 | `verify_fluent.ps1` |
| 真实点击回归 | ⚠️ 曾失败 | `tray_realrt2.txt` 记录右键菜单未出现 |
| 版本控制 | ❌ 无 git | `git status` 报 not a repository |
| CI | ❌ 无 | — |

### 4.5 度量数据

| 指标 | 实测值 | 来源 |
|---|---|---|
| 源码规模 | 4327 行（src 4073 + build.rs 254） | `wc -l` |
| 最大文件 | `main.rs` 987 行（占 23%） | 同上 |
| `unsafe` 密度 | 95 行含 unsafe（约每 45 行 1 处） | `grep -c` |
| 测试脚本 | 1652 行 PowerShell / 9 个 | `wc -l scripts/*.ps1` |
| EXE | 2.06 MB | `perf_baseline.txt` |
| 冷启动 | 78–141 ms | 同上 |
| 稳态内存（托盘） | 10.29 MB | 同上 |
| 稳态内存（全屏贴图） | 27.98 MB | 同上 |
| 空闲 CPU | 0.000%（3×10s） | 同上 |

---

## 五、当前存在的问题

> 分级：**P0** = 会导致功能异常或影响其他程序；**P1** = 架构/性能瓶颈，随功能增长放大；**P2** = 工程与文档债务。

### P0 · 正确性与可靠性（7 项）

#### P0-1 剪贴板写入失败时不会关闭剪贴板，且泄漏全局内存块

`clipboard.rs:83` 与 `clipboard.rs:114`：

```rust
SetClipboardData(CF_DIB.0 as u32, HANDLE(hmem.0))?;   // ← 失败即提前 return
let _ = CloseClipboard();                             // ← 被跳过
```

`SetClipboardData` 失败时 `?` 直接返回，两件事都没做：

1. **`CloseClipboard` 未调用** → 剪贴板被本进程独占，直到进程退出前，其他所有程序都无法读写剪贴板（用户感受是"复制粘贴全失灵"）；
2. **`hmem` 未释放** → 泄漏一整块等于图像大小的全局内存（1080p ≈ 8 MB）；且 `CloseClipboard` 在持有未移交内存块时也不会自动释放。

`copy_text` 同样问题（虽泄漏量小，但剪贴板未关闭的后果一致）。

**修复**：把 `SetClipboardData` 的结果先存入变量，无论成败都走统一的 `CloseClipboard` + 失败时 `GlobalFree`，用 RAII 守卫（`struct ClipboardGuard`）保证任何路径都释放。

#### P0-2 托盘旧协议（V0/V3）兼容分支是死代码，且 V0 下右键菜单坐标错误

`main.rs:486-513` 的判定逻辑：

```rust
let is_left  = matches!(lo_lp, WM_LBUTTONUP | WM_LBUTTONDBLCLK) || matches!(lo_lp, NIN_SELECT | NIN_KEYSELECT);
let is_right = matches!(lo_lp, WM_RBUTTONUP | WM_CONTEXTMENU);
if hi_lp != 0 || is_left || is_right {
    // V4 布局：坐标取 wParam
} else {
    // 旧协议：坐标取 GetCursorPos()
}
```

V0 协议下 `lParam` 低字就是鼠标消息，因此 `is_left`/`is_right` 必然为真 → **永远进入 V4 分支**。此时 `wParam` 是图标 ID（=1），被解析成坐标 `(1, 0)`：

- 左键：无害（截图无需坐标）；
- 右键：菜单会弹到屏幕左上角。虽然后续 `pt.x==0 && pt.y==0` 的兜底只在**双零**时生效，`(1,0)` 不满足，所以兜底也救不回来。

同时，`else` 分支（`main.rs:502-513`）实际不可达——注释宣称的"同时兼容 V0"并未成立。

**修复**：改为用 `NIM_SETVERSION` 的返回值判定协议版本并记入 `NODE_STATE`，或直接放弃 V0（Win10 1809+ 全支持 V4，与 README 的最低系统要求一致），删掉误导性的兼容分支。

#### P0-3 OCR 结果窗口先关闭时，`WM_OCR_DONE` 无处投递 → 内存泄漏

`results.rs:97-110`：后台线程把 `Box<String>` `into_raw` 成 `usize`，通过 `WPARAM` 跨线程传给窗口；`results.rs:422-437` 在 `WM_OCR_DONE` 里 `Box::from_raw` 收回。

若用户在识别完成前关闭窗口（`DestroyWindow`），`PostMessageW` 会投递到已销毁的 HWND（失败），`Box<String>` 永久泄漏；若恰好 HWND 被系统复用，还会**写进别的窗口的消息队列**（指针被当成 payload）。

**修复**：改用 `Arc<Mutex<Option<String>>>` + 窗口存活标志，或把结果投递到主窗口（生命周期确定）再由主窗口转发；至少要在 `WM_DESTROY` 里设置 `cancelled` 标志，同时后台线程检查 `IsWindow(hwnd)` 后再投递并在失败时自行释放。

#### P0-4 截图流程无 panic 防护：panic 即静默死亡，且重入锁永久置位

- `main.rs:6` 声明 `#![windows_subsystem = "windows"]`，无控制台 → panic 信息无处输出，用户看到的是"程序突然消失"；
- `main.rs:571-575` 的 `APP_BUSY.swap(true)` 在 `flow_impl()` **panic 时不会复位**，若进程未退出（如 panic 被后续 catch），从此 F1 全部无效。

**修复**：`main()` 开头装 `std::panic::set_hook`，把 `panic!` 信息 + 位置写入 `diag`；`APP_BUSY` 用 RAII 守卫（`struct BusyGuard`）管理，drop 时复位。

#### P0-5 截屏未校验位图分配失败，错误信息误导

`capture.rs:84-92`：

```rust
let bmp: HBITMAP = CreateCompatibleBitmap(screen_dc, w, h);
let old = SelectObject(mem_dc, bmp);          // ← bmp 可能无效，未检查
let blt_ok = BitBlt(...).is_ok();
```

`CreateCompatibleBitmap` 失败（超大区域/内存不足）时返回 NULL，后续 `SelectObject`/`BitBlt` 失败，最终报的是"**BitBlt 失败**"——排查方向完全错误。同类问题：`GetDC` 之后 `CreateCompatibleDC` 的处理是对的（有检查），此处是遗漏。

**修复**：`bmp.is_invalid()` 时立即清理并返回 `"CreateCompatibleBitmap 失败（区域 WxH 过大或内存不足）"`。

#### P0-6 用固定 150 ms 延时等遮罩消失

`main.rs:590-591`：

```rust
std::thread::sleep(std::time::Duration::from_millis(150));
let image = Arc::new(capture::capture_region(...)?);
```

在 GUI 主线程上硬等，且**依赖时序假设**：低配机/远控/高负载下遮罩销毁与 DWM 合成可能超过 150 ms，导致截到残留遮罩（用户会看到截图里有半透明黑色矩形）。同时这 150 ms 内主消息循环完全阻塞。

值得注意的是：`overlay` 与 `toolbar` 都已设 `WDA_EXCLUDEFROMCAPTURE`，按该属性语义（Win10 2004+）它本就不该出现在 BitBlt 结果里——这个 sleep 的**必要性未被验证**，属于"防御性但无依据"的代码。

**修复**：删除 sleep，改为先 `DwmFlush()` 等待合成完成（或直接依赖 `WDA_EXCLUDEFROMCAPTURE`），并在 E2E 中加一条"截图不含遮罩残留"的像素断言来验证；同时把截图动作移出主线程（见 P1-1）。

#### P0-7 E2E 存档存在自相矛盾的 PASS 记录

`e2e_artifacts/e2e_msg_transcript.txt:42`：

```
[PASS] 识别结果可复制到剪贴板（内容一致） -- 剪贴板 11 字符 / 窗口 69 字符
```

断言是 `$clip.Trim() -eq $text.Trim()`（`scripts/gui_e2e_msg_test.ps1:369`）。用相同代码路径复现（11 字符 vs 69 字符）**实测判定为 FAIL**，与存档的 PASS 矛盾。

这意味着：**存档里的"16 项通过、0 项失败"不能作为当前代码的有效证据**，至少这一条不可复现。可能原因是脚本在存档后被修改（脚本 mtime 早于存档，需进一步核对历史版本）或运行时 `$text` 被后续赋值污染。

**修复**：为 `Check` 增加"参数一致性自检"（`-eq` 成立时必须 `$clip.Length -eq $text.Length`，否则直接判 FAIL），并重新跑一遍覆盖存档；**所有既有 PASS 结论都应在 CI 环境下重跑一次才算数**。

---

### P1 · 架构与性能瓶颈（8 项）

#### P1-1 一次截图，两次全屏捕获、三份全屏位图

当前链路：

1. `overlay::run` 预截**整个虚拟屏幕** → `CapturedImage`（一份全屏 `Vec<u8>`，1080p ≈ 8 MB）；
2. `capture::image_to_memory_bitmap` 再拷成 DIB section（**第二份**）；
3. `CreateCompatibleBitmap(vw, vh)` 双缓冲（**第三份**）；
4. 用户确认选区后，`flow_impl` 又调 `capture_region` **重新截一次**选区。

于是：用户看到的"选区还原画面"来自快照 #1，最终交付的图像来自快照 #2——**两者可能不一致**（期间动画/视频/时钟在变）；同时多付一次全屏 BitBlt + 一次内存分配。

在双 4K（虚拟屏 7680×2160）下，三份位图约 **190 MB** 峰值，远超 README 声明的 50 MB 预算（预算只测了托盘与贴图稳态，未覆盖框选峰值）。

**改进**：`overlay::select_region` 直接返回"预截位图 + 选区"，`flow_impl` 从该位图裁剪出 `CapturedImage`。收益：内存峰值降 1/3、消除画面不一致、省一次全屏捕获。这是**当前性价比最高的一项改造**。

#### P1-2 布局使用固定像素常量，DPI 125% 以上必然溢出

`TB_W=480 / TB_H=174 / TB_HEADER_H=44`（`main.rs:605-608`）、`RES_W=560 / RES_H=420`（`results.rs:41-42`）、按钮坐标全部硬编码（`main.rs:793-798`）。

字体走 `GetDpiForWindow` 正确缩放，但**容器尺寸与控件位置不缩放**。150% 下按钮 76×38 px 内要塞 9pt×1.5 的字 → 文字溢出/截断；200% 下工具栏布局直接崩。README 只声明了"125% 缩放下精确"，实质是把 DPI 适配做了一半。

**改进**：引入统一的 `scale(hwnd) -> f32` 并对所有尺寸/坐标过一遍（或改用 `AdjustWindowRectExForDpi` + 逻辑单位在布局时换算）；至少在 `WM_DPICHANGED` 时重排控件并校验 `TB_W` 是否够放 5 个按钮。

#### P1-3 贴图绘制无离屏缓冲，大图缩放闪烁/卡顿

`pin.rs:255-294`：每次 `WM_PAINT` 直接 `StretchDIBits` 到窗口 DC。GDI 的 `StretchDIBits` 是软件缩放：

- 缩放拖动时逐帧重采样（4K 图 → 几十毫秒/帧），表现为**拖影与卡顿**；
- 无双缓冲，滚动缩放时可见闪烁。

对比 `overlay` 已经做了双缓冲（`overlay.rs:339-387`），这里是不一致。

**改进**：① 加一层离屏 DC 双缓冲；② 缩放时用 `SetStretchBltMode(HALFTONE)` + `StretchBlt` 或按整数倍做快速路径；③ 在 100% 缩放下直接 `BitBlt`（走最快路径）。

#### P1-4 悬停动画引擎每次鼠标移动都重置定时器

`util.rs:274-290`：

```rust
pub fn on_mouse_move(&mut self, parent: HWND) {
    ...
    let _ = SetTimer(parent, TIMER_HOVER_TRACK, 16, None);  // ← 每条 WM_MOUSEMOVE 都调
    self.track_tick(parent);
}
```

`SetTimer` 对同一 ID 会**重置**计时，高频鼠标移动（120 Hz 鼠标 → 每 8 ms 一条消息）会导致 16 ms 定时器被不断重置而**几乎永不触发**，形成"鼠标快速移动时悬停高亮反而迟钝"的反直觉现象。同时每条 mousemove 都产生一次内核调用。

**改进**：用 `tracking` 标志判定定时器是否已存在（与 `TrackMouseEvent` 同一条件），只在首次启动；并把每帧 N 次 `GetWindowRect` 换成子控件矩形缓存（`WM_WINDOWPOSCHANGED` 时刷新，`pin.rs` 已有此模式）。

#### P1-5 图像数据存在 3 处全量拷贝

| 位置 | 拷贝 | 典型代价（1080p） |
|---|---|---|
| `imageio.rs:28` | `save_png` 先 `to_vec()` 再逐像素置 alpha=255 | 8 MB + 200 万次循环 |
| `pin.rs:368` | 贴图发起 OCR 时 `img.to_vec()` | 8 MB |
| `main.rs:924` | 工具栏 OCR `d.image.data.clone()` | 8 MB |

**改进**：`CapturedImage` 改持 `Arc<[u8]>`（`results::start_ocr` 接受 `Arc`）；alpha 置位在 **capture 阶段一次性完成**（`BitBlt` 得到的数据 alpha 本无意义，统一置 255 后，PNG/DIB/OCR 三处都不必再处理）——顺带修掉一个潜在兼容问题：`CF_DIB` 中 alpha=0 时，部分应用（如部分版本的 Word/微信）会把贴图当全透明处理。

#### P1-6 `static mut` 承载关键状态，阻碍演进

`overlay.rs:61 STATE`、`traymenu.rs:44 DATA`、`theme.rs:91 CACHE/READY/FAMILY`、`util.rs:127 FONT_CACHE`、`main.rs:225/274/276/328` 互斥体与钩子句柄——全部 `static mut`，且文件级 `#![allow(static_mut_refs)]` 压制了新版编译器警告。

当前单 GUI 线程下**不会出错**（注释也说明了这一前提），但：

- Rust 2024 edition 已将 `static_mut_refs` 升级为**硬错误**，未来无法平滑升级；
- 一旦要支持"多窗口并行操作"或引入任何后台线程访问状态，立刻踩数据竞争；
- `FONT_CACHE` 永不清理，长期运行下 HFONT 只增不减（数量有限，但无上限）。

**改进**：把每窗口状态改为 `Box<State>` + `GWLP_USERDATA`（`pin.rs`/`results.rs` 已是此模式，可对齐）；全局单例改为 `OnceLock<Mutex<...>>` 或 `thread_local!`；字体缓存随窗口销毁释放。

#### P1-7 低级键盘钩子是系统级输入链上的一环

`main.rs:281-301` 的 `WH_KEYBOARD_LL` 是全局钩子：每次按键都会回调本进程。虽然实现正确（只做 `PostMessageW`、立即 `CallNextHookEx`），但**本进程主线程一旦阻塞（例如 P0-6 的 150 ms sleep、或 OCR 弹框、或较重的 `StretchDIBits`），全系统键盘输入都会被拖慢**。

这条路径只在 `RegisterHotKey` 失败时启用，属"逃生通道"，但风险没有被文档标注。

**改进**：① 在 README"已知限制"里显式声明该风险；② 钩子回调内只做最小工作（已满足）；③ 考虑给钩子宿主单独线程跑消息循环，与 GUI 线程解耦。

#### P1-8 `build.rs` 强依赖 `windres`，MSVC 工具链下静默丢失图标

`build.rs:232-241` 只找 `windres.exe`（PATH 或 `C:\msys64\mingw64\bin`），找不到时**仅打 `cargo:warning` 并继续**，产物无图标。

而 README 明确写"`x86_64-pc-windows-msvc` 或 `x86_64-pc-windows-gnu` 工具链均可"——MSVC 用户会得到一个**没有图标、托盘回退系统默认图标**的 exe，且容易忽略那条 warning。

**改进**：① 用纯 Rust 手写 COFF 目标文件（.res → COFF 并不复杂，项目已有"纯 Rust 渲染 ICO"的先例，风格一致）；② 或引入 `embed-resource`（1 个 build 依赖，需权衡零依赖原则）；③ 最低限度：MSVC 下改为 `cargo:warning` 升级为**构建失败**，避免静默降级。

---

### P2 · 工程与文档债务（9 项）

| 编号 | 问题 | 证据 | 影响 |
|---|---|---|---|
| P2-1 | **零单元测试** | `grep #[test]` 无结果 | `norm_rect` / `clamp_region` / `lerp` / `luminance` / `in_rounded` / ICO 编码等**纯函数完全可测**却未测；这些正是最容易回归的地方 |
| P2-2 | **不是 git 仓库** | `git status` → not a repository | 无历史、无回滚、无 diff 审查；之前"改坏了不知道改了什么"的风险全靠人记 |
| P2-3 | **clippy 未安装、从未 lint** | `cargo-clippy.exe is not installed` | 95 处 `unsafe`、大量裸指针运算从未被静态检查；`as` 截断转换、`static mut` 使用等应被 lint 提示 |
| P2-4 | **无 CI** | 无 `.github/` 等 | 构建与 E2E 全靠手工触发，无法保证"提交即可构建" |
| P2-5 | **README 与实现漂移** | 见下 | 误导新读者，尤其"可复现的实测结论"部分一旦失真会连累全部可信度 |
| P2-6 | **根目录混杂临时产物** | `debug.log`（搜狗输入法日志！）、`pixshot_smoke.png`、`tray_realrt2.txt`、`fix5_verify.txt`、`fluent_verify.txt`、`perf_baseline.txt`、`final_accept.txt` 平铺根目录 | `.gitignore` 只忽略 `/target` 与 `pixshot_smoke.png`；这些应归入 `artifacts/` 或忽略 |
| P2-7 | **系统 `MessageBoxW` 承担全部提示** | `util.rs:224`、`results.rs:478` | "已复制到剪贴板"这种轻提示用**模态阻塞框**，打断操作流；视觉也与 Fluent 自绘割裂；E2E 甚至要专门 `PostMessage(WM_CLOSE)` 关闭它 |
| P2-8 | **贴图右键是系统菜单，托盘右键是自绘菜单** | `pin.rs:344` vs `traymenu.rs` | 同一程序两种右键视觉，明确的不一致 |
| P2-9 | **零配置能力** | 无配置文件/无设置界面 | F1 硬编码、保存格式只有 PNG、无开机自启、无快捷键自定义、无截图历史 |

**P2-5 README 漂移的具体条目**（逐条核对结果）：

| README 声明 | 实际情况 |
|---|---|
| 项目结构列 `verify_fix_single_hook.ps1` | **该文件不存在**（实际是 `verify_fix5b.ps1` 等 9 个脚本） |
| 项目结构只列 4 个脚本 | 实际 **9 个**，漏 `final_accept` / `fix_integrity` / `measure_baseline` / `verify_fluent` / `verify_tray_realclick` / `verify_fix5b` |
| `src/` 模块表 | **缺 `traymenu.rs`**（自绘托盘菜单，且是 README 大篇幅讲的核心难点） |
| "两个脚本验证的检查项（16 项）" | 下方只列 **10 条**编号（且第 8 项重复两次） |
| 测试结果示例"15 项通过" | 最新存档为 **16 项通过**（`e2e_msg_transcript.txt:45`） |
| EXE 体积 **2.11 MB** | `perf_baseline.txt` 实测 **2.06 MB** |
| 两种工具链均可 | MSVC 下图标缺失（见 P1-8） |
| `verify_tray_realclick.ps1` 一键真实点击回归 | `tray_realrt2.txt` 存档显示该路径**曾失败**（菜单未出现），修复后未留新的成功存档 |

---

## 六、有待改进的方面与改进方向

把上面 24 条问题收敛成 6 条主线，每条给出**目标 → 做法 → 验收标准**。

### 主线 1：可靠性收口（对应 P0-1 ~ P0-6）

**目标**：主链路在异常路径上不污染系统、不静默失败、不留泄漏。

| 动作 | 具体做法 |
|---|---|
| 剪贴板 RAII | 抽 `ClipboardGuard`，`Drop` 里无条件 `CloseClipboard`；`SetClipboardData` 失败时 `GlobalFree` 并返回可读错误 |
| panic 可见 | `set_hook` 写 `diag`（含 `file:line` + backtrace 摘要）；`APP_BUSY` 改守卫 |
| 状态守卫化 | `APP_BUSY` / `TRAY_MENU_BUSY` 统一用 `struct BusyGuard(AtomicBool)` |
| 错误定位准确 | 所有 GDI 资源创建点补 `is_invalid()` 检查并给出**指向真实原因**的消息 |
| 去掉时序假设 | 删 150 ms sleep，改 `DwmFlush`；E2E 加"截图无遮罩残留"像素断言 |

**验收**：`SetClipboardData` 注入失败用例下，剪贴板仍可被其他程序正常读写；人为 panic 后诊断日志有记录且 F1 仍可用。

### 主线 2：截屏管线重构（对应 P1-1、P1-5、P0-6）

**目标**：一次快照贯穿全流程，内存峰值下降 ≥50%。

```
改造后：
overlay::select_region() -> Option<(Arc<CapturedImage>, SelectedRegion)>
                                   │
                                   └── flow_impl 直接裁剪，不再重新截屏

CapturedImage { data: Arc<[u8]>, ... }   // 全链路共享，零拷贝
alpha 在 capture 阶段统一置 255          // PNG / CF_DIB / OCR 三处都不再处理
```

**验收**：双 4K 虚拟屏下框选峰值内存 < 90 MB（现约 190 MB）；`pin` 发起 OCR 与工具栏 OCR 都无 8 MB 级拷贝；截图内容与用户所见一致（可通过"框选期间播放动画"人工验证）。

### 主线 3：DPI 与视觉一致性（对应 P1-2、P1-3、P2-7、P2-8）

**目标**：150%/200% DPI 下布局不溢出；同类交互同一套视觉。

| 动作 | 做法 |
|---|---|
| 尺寸参数化 | 引入 `struct Metrics { dpi: f32, ... }`，所有常量改为逻辑单位 × `dpi/96`；监听 `WM_DPICHANGED` 重排 |
| 控件布局弹性 | 工具栏按钮改按 `TextWidth + padding` 计算宽度，避免写死 76px |
| 贴图双缓冲 | 与 `overlay` 对齐，离屏 DC + `SetStretchBltMode(HALFTONE)`；100% 缩放走 `BitBlt` 快速路径 |
| 轻提示替代 MessageBox | 自绘 Toast（复用 `traymenu` 的窗口骨架，2s 自动消失，`WS_EX_NOACTIVATE`） |
| 贴图右键统一 | 把 `pin.rs:296-393` 的 6 个菜单项迁到 `traymenu` 的通用弹窗（支持多项 + 分隔线 + 禁用态） |

**验收**：100/125/150/200% 四档截图（存 `e2e_artifacts/dpi/`）无文字截断；贴图 4K 图缩放无闪烁；全程序无 `MessageBoxW`（错误框除外，且应为自绘）。

### 主线 4：可维护性基建（对应 P1-6、P2-1、P2-2、P2-3、P2-4）

**目标**：从"手工验证"转为"提交即验证"。

1. **`git init` + 首次提交**（把根目录临时文件按 P2-6 归档/忽略）——**这件事应最先做**，成本最低、收益最大；
2. **补单元测试**（目标 30% 行覆盖，优先纯函数）：
   - `overlay::norm_rect`（反向拖动、零面积）、`clamp_region`（越界钳制）
   - `theme::lerp` / `luminance` / `with_user_accent`（强调色守卫边界）
   - `build.rs` 的 `in_rounded` / `on_glyph` / ICO 头部字节
   - `capture::top_down_header`（负高度、stride 计算）
3. **装 clippy + 固化等级**：`cargo clippy -- -D warnings`，逐条整改或就地 `#[allow]` 并写理由；
4. **拆 `main.rs`**：987 行里托盘（~330 行）、热键（~70 行）、工具栏（~340 行）三块边界清晰，可拆为 `tray.rs` / `hotkey.rs` / `toolbar.rs`，`main.rs` 只留编排；
5. **CI（GitHub Actions 或本地 `scripts/ci.ps1`）**：`check → clippy → test → build --release → 体积断言 → 消息驱动 E2E`。

**验收**：`scripts/ci.ps1` 一条命令跑完全部检查并给出退出码；`main.rs` < 400 行。

### 主线 5：文档与证据链校准（对应 P0-7、P2-5、P2-6）

**目标**：README 每一句可复现的结论都有对应的、当前有效的证据文件。

| 动作 | 做法 |
|---|---|
| 文档与实现对齐 | 补 `traymenu.rs`；脚本清单按实际 9 个重写；E2E 项数统一为 16 并逐条列出；体积数字取自 `perf_baseline.txt` 最新值 |
| 证据目录规范化 | `artifacts/{perf,e2e,visual,diag}/*.txt`，每份文件头写"采集时间 + 版本 + 命令"；根目录只留源码与 MD |
| 断言自检 | `Check` 增加参数一致性校验（P0-7）；每次发版前**重跑全部 E2E 并覆盖存档** |
| 补失败存档 | `tray_realrt2.txt` 的失败记录要么附上修复后的成功存档，要么在 README 标注"该路径在受限环境不可用" |
| 风险声明 | "已知限制"补：全局键盘钩子的系统级影响、MSVC 下图标缺失、未签名分发 |

**验收**：README 中"实测/验证"类表述 100% 可在 `artifacts/` 找到当次记录；文档中的脚本路径 `Test-Path` 全为 True。

### 主线 6：产品能力补齐（对应 P2-9）

按"投入产出比"排序，建议只做前 3 项：

| 优先级 | 能力 | 理由 | 实现要点 |
|---|---|---|---|
| ★★★ | **配置文件 + 热键自定义** | 单人用户最常抱怨的第一件事；F1 在多数 IDE/浏览器里都被占用 | `%APPDATA%\PixShot\config.toml`（手写极简解析，或 `serde`+`toml` 2 个 crate，需权衡零依赖）；热键变更需 `UnregisterHotKey` → 重注册 |
| ★★★ | **保存格式选择（PNG/JPG/WebP）+ 记住上次目录** | WIC 已支持全部格式，只是没接 | `IWICImagingFactory::CreateEncoder` 换 GUID 即可，`SetQuality` 设 JPEG 质量；`OPENFILENAMEW` 加滤波器组 |
| ★★☆ | **拖拽输出到其他应用** | 截图工具最高频的"下一步动作" | `IDropSource` + `IDataObject`（`CF_DIB`/`CF_HDROP`），约 300 行 COM，可参考 `pin.rs` 现有结构 |
| ★☆☆ | 开机自启 | `HKCU\...\Run` 一行注册表 | 注意本机 `reg.exe` 被拦，需走 `RegSetValueEx` API |
| ★☆☆ | 截图历史 | 需要落盘管理与清理策略 | 后期再做 |

**明确不建议**：标注编辑（箭头/文字/马赛克）。它会引入"编辑器状态机 + 撤销栈 + 图层"，与"零依赖手绘 GDI"的架构正面冲突，投入产出比极低——真要做得换渲染栈（Direct2D）。

---

## 七、建议路线图

| 阶段 | 周期 | 内容 | 完成标志 |
|---|---|---|---|
| **第一阶段：止血** | 1–2 天 | `git init`；修 P0-1（剪贴板）、P0-2（V0 分支）、P0-3（OCR 回填）、P0-4（panic hook）；重跑 E2E 覆盖存档 | 全 P0 关闭；`git log` 有首个提交；E2E 存档可复现 |
| **第二阶段：瘦身与扩面** | 3–5 天 | 截屏管线重构（P1-1/1-5）；DPI 参数化（P1-2）；贴图双缓冲（P1-3）；单测 + clippy + `ci.ps1` | 框选峰值内存 < 90 MB；4 档 DPI 通过；CI 一条命令跑通 |
| **第三阶段：产品化** | 1 周+ | 配置文件 + 热键自定义 + 格式选择；Toast 替代 MessageBox；README 全面校准；代码签名（按 `SIGNING.md`） | 可发给非技术同事直接使用，无需口头指导 |

---

## 八、总体评价

**优点（真实且稀缺）**

1. **诊断意识**：多数个人 Win32 项目死在"托盘没图标，不知道为什么"上就放弃了。这个项目定位到了 **UIPI + Low Integrity Label** 这一层，并写了自动修复脚本——这是工程能力的分水岭。
2. **实测精神**：托盘 V4 回调布局、`TrackPopupMenuEx` 对隐藏窗口失效、DWM 框架带——这些结论都标了"实测"并附样本，可验证、可传承。
3. **指标约束**：把"体积/启动/内存/CPU"写成硬预算并实测对照，很少见。
4. **零依赖的克制**：连图标都自己算，换来 2.06 MB 单文件与 10 MB 稳态内存。

**短板（可修，但需要纪律）**

1. 异常路径的收尾普遍靠"顺序执行恰好正确"，缺少 RAII 约束（P0-1 是典型）；
2. 布局与状态管理仍是"单场景最优"，没有为 DPI 变化与多窗口并行预留结构；
3. 质量验证依赖手工脚本 + txt 存档，缺少自动化防线，导致出现 P0-7 这种"存档与断言互相矛盾"的可信度问题；
4. 文档写得比代码演进快，出现多处漂移。

**一句话判断**：**代码的技术判断力明显高于其工程纪律**。先把 git 与 CI 立起来、把 P0 收掉，这个项目就能从"能跑的实验品"跨到"可交付的工具"；剩下的功能扩展反而简单。
