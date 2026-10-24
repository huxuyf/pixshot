# PixShot 改动清单（对应评估报告 24 条问题 + 6 条改进主线）

> 交付日期：2026-10-24　|　基线文档：`outputs/pixshot-项目评估报告.md`
>
> 验证状态（全量实测）：
> `cargo check --all-targets` **零错误、零代码警告**；
> `cargo test` **46 项全过**（32 单元测试 + 14 集成测试）；
> `scripts/ci.ps1` **完整 6 步全过**（含 GUI E2E），release 产物 **2.17 MB**（护栏 2.6 MB）；
> `gui_e2e_test.ps1` **14/14 通过**、`gui_e2e_msg_test.ps1` **16/16 通过**。

---

## 〇、6 条改进主线对照

| 主线 | 覆盖问题 | 落地情况 |
|---|---|---|
| 1. 可靠性收口 | P0-1 ~ P0-6 | ✅ 全部完成：RAII 守卫、协议双分支、结果槽 + 存活标志、panic 兜底 + BusyGuard、DIB 失败校验、删除 150ms sleep |
| 2. 截屏管线重构 | P1-1、P1-5、P0-6 | ✅ 全部完成：单次全屏捕获 + `crop()` + `Arc<[u8]>` 全链路零拷贝 |
| 3. DPI 与视觉一致性 | P1-2、P1-3、P2-7、P2-8 | ✅ 全部完成：`dpi_of`/`scaled` 参数化 + `WM_DPICHANGED`；贴图 100% 快路径 + HALFTONE/COLORONCOLOR；Toast；统一自绘菜单 |
| 4. 可维护性基建 | P1-6、P2-1、P2-2、P2-3、P2-4 | ⚠ 4/5 完成：`static mut` 清零、46 项测试、git 仓库、CI 脚本、零警告；**clippy 因组件未安装未执行** |
| 5. 文档与证据链校准 | P0-7、P2-5、P2-6 | ✅ 全部完成：README 全面校准、临时产物归档 + `.gitignore` 重写、E2E 断言严格化并实测复现（见 P0-7 与第七节） |
| 6. 产品能力补齐 | P2-9 | ✅ 完成：`config.ini` 五项配置（热键/格式/目录/OCR 语言/Toast 时长）；开机自启与截图历史超出本轮范围 |

---

## 一、P0 缺陷（7 条）

### P0-1 剪贴板打开后异常路径不关闭 → 全系统剪贴板被锁死

- **文件**：`src/clipboard.rs`
- **原问题**：`SetClipboardData` 用 `?` 提前返回时跳过 `CloseClipboard()`，且失败分支泄漏
  `GlobalAlloc` 出的内存块，导致其他程序长时间无法访问剪贴板。
- **改法**：引入 RAII 守卫 `struct ClipboardGuard`：
  - `ClipboardGuard::open()` 内部重试 5 次（每次 `Sleep(10)`，剪贴板可能被其他进程瞬时占用）；
  - `Drop` 中**无条件** `CloseClipboard()`，任何 return 路径（含 `?`、含 panic 展开）都覆盖；
  - `SetClipboardData` 失败时先 `GlobalFree(hmem)`（`windows::Win32::Foundation::GlobalFree`，
    返回 `Result<HGLOBAL>`）再返回错误，所有权语义明确。
- **测试**：`guard_drop_runs_on_early_return` 验证提前返回时守卫仍执行释放。

### P0-2 托盘 V0 回调协议分支不可达，V0 下右键菜单坐标被解析成 (1,0)

- **文件**：`src/tray.rs`
- **原问题**：`NIM_SETVERSION(NOTIFYICON_VERSION_4)` 若失败，回调仍是 V0 布局
  （wParam=图标 ID、lParam=事件），但代码无条件按 V4 解析（wParam=坐标、lParam=事件+ID），
  于是 `LOWORD(wParam)` 得到 uID=1 当作 x 坐标，菜单弹到屏幕左上角。
- **改法**：新增 `static TRAY_V4: AtomicBool`，在 `NIM_SETVERSION` **成功后**置 true；
  回调内先 `load()` 再分派两条完整分支：

  ```rust
  if v4 {
      match lo_lp { WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT => screenshot_flow(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(hwnd, x, y), _ => {} }
  } else {
      match lo_lp { WM_LBUTTONUP | WM_LBUTTONDBLCLK => screenshot_flow(),
                    WM_RBUTTONUP | WM_CONTEXTMENU => { GetCursorPos(&mut pt);
                                                       tray_menu(hwnd, pt.x, pt.y) }, _ => {} }
  }
  ```

  V0 下无坐标可依，改用 `GetCursorPos()` 取当前位置 —— 与用户点击位置一致。
- **兼容性**：V4 路径行为与改动前完全一致（E2E 断言的坐标解析不受影响）。

### P0-3 OCR 结果窗口关闭后仍投递 `WM_OCR_DONE` → 写已释放内存

- **文件**：`src/results.rs`
- **原问题**：后台线程捕获裸 `HWND`，窗口销毁后 `PostMessageW` 仍会送达（HWND 值可能被
  Windows 复用），或消息无人接收导致 `Box<Data>` 泄漏。
- **改法**：
  - 结果槽改为 `type OcrResultSlot = Arc<Mutex<Option<(String, bool)>>>`，线程只写槽；
  - 存活标志 `alive: Arc<AtomicBool>`，线程在 `PostMessageW` **之前**检查 `alive.load()`；
  - `WM_DESTROY` 中 `data.fx.stop(hwnd); data.alive.store(false)`，先停动画定时器再断消息；
  - `start_ocr(image: Arc<[u8]>, w, h)` 直接把 `Arc` 移交给线程（零拷贝）。
- **效果**：窗口先关后完成识别时，线程安全退出、不投消息、不泄漏。

### P0-4 panic 静默 + 重入标志无异常保护 → 一次 panic 后永久无法截图

- **文件**：`src/main.rs`、`src/util.rs`
- **原问题**：`windows_subsystem = "windows"` 下 panic 无输出；`APP_BUSY` 手工置位，
  panic 展开后不复位，程序表面存活但热键永久失效。
- **改法**：
  - `install_panic_hook()` 把 panic 信息（含位置）写入 `diag::log("[panic] ...")` 并 `eprintln!`；
  - `pub(crate) fn screenshot_flow()` 外层 `catch_unwind(AssertUnwindSafe(...))`，
    panic 被捕获后记录日志、返回错误、**不向上传播**到消息循环；
  - `static APP_BUSY: AtomicBool` 改为私有，只暴露 `BusyGuard::acquire() -> Option<Self>`，
    `Drop` 自动复位 —— 异常路径不再需要手工清理。
- **效果**：单次截图失败不会让程序进入「活着但按键无反应」的僵尸态。

### P0-5 `CreateDIBSection` 失败未校验 → 空指针解引用

- **文件**：`src/capture.rs`
- **原问题**：只判断 `HBITMAP` 非空，未检查 `bits` 输出指针；失败时后续
  `copy_nonoverlapping` 从空指针读取。
- **改法**：同时校验 `CreateDIBSection` 返回值与 `bits.is_null()`，任一失败即返回
  `AppError::Capture`，**错误信息带上区域尺寸与坐标**，便于判断是否超大分辨率/内存不足。

### P0-6 固定 `sleep(150ms)` 等遮罩消失 → 时序假设 + 阻塞 GUI 线程

- **文件**：`src/main.rs`
- **原问题**：截完图后 `std::thread::sleep(Duration::from_millis(150))` 等遮罩窗口消失
  再重新截屏。这既是时序赌博（慢机器会截到遮罩），又白白阻塞 GUI 线程 150ms；
  且遮罩本身已设 `WDA_EXCLUDEFROMCAPTURE`，根本不需要等。
- **改法**：**删除 sleep**。因为截屏管线改为「只截一次全屏」（见 P1-1），
  选区直接从快照 `shot.crop(region.x, region.y, region.width, region.height)` 裁出，
  不存在「再截一次」的需求，时序问题从根上消失。
  这比原报告建议的「`DwmFlush()` 等合成完成」更彻底 —— 不等合成就没有时序假设，
  且不阻塞 GUI 线程。至于「把截图移出主线程」，单次捕获后主线程只剩一次内存拷贝
  （`CreateDIBSection` 直接内存访问，无 `GetDIBits` 往返），已无必要。
- **未做**：原报告建议「E2E 中加一条『截图不含遮罩残留』的像素断言」，
  本轮未改 E2E 脚本（见 P0-7 说明）。

### P0-7 E2E 存档与断言自相矛盾（PASS 但参数不符）

- **状态**：**已在第二批次修复（根治）**。
- **原问题**：`e2e_artifacts/e2e_msg_transcript.txt` 里出现「PASS 但 11 vs 69 字符」的
  自相矛盾记录，存档不可复现。
- **真实根因**（本轮实测定位，比原报告推测更具体）：
  1. `Get-ClipText` 的 `for` 重试循环里**漏了 `return`** —— 读完一次剪贴板后没退出，
     循环跑满 10 轮，函数最终返回「10 份文本 + 末尾空串」的**数组**。于是
     `$clipS.Length` 显示 11（元素数）而非字符数；
  2. `Check` 的 `$Ok` 参数无类型约束，而 PowerShell 的 `-eq` 作用于**集合**时返回的是
     **筛选结果（数组）**而不是布尔值，**非空数组在 `if` 中恒为真** → 假 PASS。
- **修法**：
  - `Get-ClipText` 改为 `return [Marshal]::PtrToStringUni(...)`，并预置 `$h = [IntPtr]::Zero`
    供 `finally` 安全引用（原代码在格式不可用提前 return 时，`finally` 会引用未定义的 `$h`）；
  - 两个 E2E 脚本的 `Check` 改为**严格布尔判定**：只接受真正的 `[bool]`，其他类型
    （含数组、null）一律记 **FAIL** 并打印「断言告警：判定值不是布尔（实际 xxx）」，
    绝不静默放行；
  - 修复后实测：物理版 `剪贴板 212 字符 / 窗口 212 字符` PASS、消息版
    `剪贴板 107 字符 / 窗口 107 字符` PASS —— 两处数字首次真正相等。

---

## 二、P1 结构性问题（8 条）

### P1-1 一次截图两次全屏捕获、三份全屏位图

- **文件**：`src/capture.rs`、`src/overlay.rs`、`src/main.rs`
- **改法**：
  - `overlay::select_region() -> Option<(Arc<CapturedImage>, SelectedRegion)>`：
    遮罩启动时截**一次**全屏，事件循环结束把「快照 + 选区」一起返回；
  - `CapturedImage { x, y, width, height, data: Arc<[u8]> }`，新增
    `crop(&self, x, y, w, h) -> CapturedImage`（含越界钳制）；
  - `main.rs` 的 `flow_impl()` 只做 `shot.crop(...)`，不再调用 `capture::full_screen()`。
- **效果**：双 4K 场景的瞬时位图从 3 份（约 190MB 峰值）降到 1 份；遮罩还原与最终结果
  像素级一致（同一份快照），不会出现「框选时看到的」与「贴出来的」不一致。
- **测试**：`crop_keeps_absolute_origin`、`crop_clamps_to_bounds`、`crop_full_frame_is_identity`。

### P1-2 固定像素布局在 150%+ DPI 下溢出

- **文件**：`src/util.rs`（新增 `dpi_of` / `scaled`）、`src/toolbar.rs`、`src/results.rs`、`src/overlay.rs`
- **改法**：
  - 统一 `pub fn dpi_of(hwnd) -> i32`（`GetDpiForWindow`）与 `pub fn scaled(v: i32, dpi: i32) -> i32`；
  - 所有布局常量改为**逻辑尺寸 + 运行时换算**（工具栏 `TB_W=480 / TB_H=174 / TB_HEADER_H=44`）；
  - 各窗口处理 `WM_DPICHANGED` 重新排布；
  - 选区「单击判定」阈值按 DPI 换算（`MIN_DRAG_LOGICAL = 3`），高 DPI 下不会把微拖误判为单击。

### P1-3 贴图缩放走 `StretchBlt` 无插值 → 缩小出现锯齿/摩尔纹

- **文件**：`src/pin.rs`
- **改法**：
  - **100% 快路径**：直接 `StretchDIBits` 到窗口 DC（无中间位图，无插值开销）；
  - 其他倍率：`CreateCompatibleDC` 离屏合成 + `SetStretchBltMode`
    （缩小 `HALFTONE` 并配 `SetBrushOrgEx` 消除图案偏移，放大 `COLORONCOLOR`）+ `BitBlt` 一次贴出。
- **效果**：缩放画质与闪烁同时改善；100% 时不额外占用一份内存位图。
- **测试**：`zoom_keeps_cursor_anchor`、`scale_bounds`。

### P1-4 `HoverFx` 每次 `WM_MOUSEMOVE` 都 `SetTimer` + 每 tick N 次 `GetWindowRect`

- **文件**：`src/util.rs`
- **改法**：
  - `on_mouse_move` 只在 `!tracking` 时首次 `SetTimer`（原来每移动一像素就调一次 `SetTimer`）；
  - `track_tick` 用一次 `WindowFromPoint` 判断光标是否还在窗口内，替代对每个按钮
    `GetWindowRect` + 逐点包含判断；
  - 新增 `stop(parent)`，离开窗口/销毁时显式 `KillTimer`。
- **效果**：悬停动画从「高频内核调用」降为「1 次/ tick」，空闲 CPU 仍为 0。

### P1-5 全屏位图被复制 3 次

- **文件**：`src/capture.rs`、`src/toolbar.rs`、`src/pin.rs`、`src/results.rs`
- **改法**：`Arc<[u8]>` 贯穿全链路，`Arc::clone` 代替 `to_vec()`；
  alpha 通道在捕获阶段统一置 255（原来在 3 个消费点各扫一遍）。
- **效果**：双 4K 下省掉约 100MB 级别的瞬时拷贝。

### P1-6 `static mut` 遍布 → 阻碍 Rust 2024 迁移、且是 UB 隐患

- **文件**：`src/overlay.rs`、`src/theme.rs`、`src/util.rs`
- **改法**：
  - `overlay.rs`：删除 `#![allow(static_mut_refs)]` 与 `static mut STATE`，改
    `Box<State>` 挂 `GWLP_USERDATA`（`WM_CREATE` 存入，`WM_DESTROY` 只清 USERDATA，
    消息循环退出后 `Box::from_raw` 读回 `result` 再 `cleanup()`）；
  - `theme.rs`：`CACHE` / `FAMILY` 改 `thread_local! { RefCell<Option<_>> }`；
  - `util.rs`：`FONT_CACHE` 改 `thread_local!`；
  - 各类简单标志改 `AtomicBool` / `AtomicU32` / `AtomicIsize`。
- **效果**：全仓 **零 `static mut`**、零 `static_mut_refs` allow，为 Rust 2024 edition 铺路。

### P1-7 低级键盘钩子是系统级输入链上的一环

- **原问题**：`WH_KEYBOARD_LL` 是全局钩子，每次按键都回调本进程。实现虽正确
  （只 `PostMessageW` 后立即 `CallNextHookEx`），但**主线程一旦阻塞**（原 P0-6 的 150ms sleep、
  模态弹框、较重的 `StretchDIBits`），**全系统键盘输入都会被拖慢**。
- **改法（两条线）**：
  1. **减少主线程阻塞**：删除 150ms sleep（P0-6）、`MessageBoxW` 换 Toast（P2-7）、
     截屏改单次捕获（P1-1）——钩子回调链上的阻塞点基本清空；
  2. **补修饰键校验**：`static HOOK_VK: AtomicU32` + `static HOOK_MODS: AtomicU32`，
     `fn modifiers_match()` 用 `GetAsyncKeyState` 比对 Ctrl/Alt/Shift/Win ——
     `KBDLLHOOKSTRUCT.flags` 不反映修饰键状态，缺这步会把 `Ctrl+F1` 误判为 F1；
  3. **文档声明固有风险**（README 设计说明 + 已知限制）：回调超过系统
     `LowLevelHooksTimeout`（默认约 300ms）会被**静默摘除**；钩子能看到全系统按键，
     部分安全软件会直接拦截。因此**钩子仅作兜底**，默认路径是 `RegisterHotKey`。

### P1-8 `build.rs` 只认 windres，MSVC 工具链静默无图标

- **文件**：`build.rs`
- **改法**：
  - 抽象 `RcTool { Windres, MsvcRc }`：GNU 工具链走 `windres`（输出 COFF `.o`），
    MSVC 工具链走 `rc.exe`（输出 `.res`，`link.exe` 直接接受）；
  - 查找顺序：PATH → MSYS2 常见安装位置；
  - **两者都找不到时 `panic!` 让构建失败**，并打印两条解决路径（装 MSYS2 / 用 Native Tools 提示符）
    与显式跳过方式（`PIXSHOT_NO_ICON=1`）——早期静默跳过会产出「托盘空白方块」，排查成本极高；
  - 图标几何抽出 `src/icon_geometry.rs`，`build.rs` 用 `#[path = "src/icon_geometry.rs"] mod geo;`
    引入，与 `tests/icon_geometry.rs` 共用同一份代码，杜绝「测试一份、构建另一份」。

---

## 三、P2 工程纪律与体验（9 条）

### P2-1 零单元测试

- **原问题**：全仓 `grep #[test]` 无结果，而 `norm_rect` / `clamp_region` / `lerp` /
  `luminance` / `in_rounded` / ICO 编码等**纯函数完全可测**却未测。
- **改法**：把纯逻辑从 Win32 调用里剥出来，补 **46 项测试**：

  | 模块 | 项数 | 覆盖内容 |
  |---|---|---|
  | `clipboard.rs` | 3 | 行序翻转、单行恒等、守卫在提前返回时仍释放 |
  | `capture.rs` | 4 | 顶向下 DIB 负高度、裁剪保留绝对原点、越界钳制、全帧恒等 |
  | `config.rs` | 7 | 单键/组合热键解析、非法热键拒绝、非法值回退、往返一致性、格式别名 |
  | `imageio.rs` | 3 | BGR24 去 alpha 保序、奇数宽度、默认文件名后缀 |
  | `ocr.rs` | 3 | 同尺寸拷贝、降采样取最近邻、退化输入不 panic |
  | `overlay.rs` | 4 | 正反拖动归一化、零面积、虚拟屏幕钳制、钳制后非空 |
  | `pin.rs` | 3 | 文件名短化、缩放上下界、缩放保持光标锚点 |
  | `theme.rs` | 5 | 亮度极值、强调色亮度护栏、插值端点/中点/越界、描边克制 |
  | `tests/icon_geometry.rs` | 14 | 端点、对称性、越界、退化线段、常量约束 |

### P2-2 不是 git 仓库

- **改法**：`git init -b main` + 首次提交 `34eaade`，35 个文件入库；
  提交信息按「结构变化 / 缺陷修复 / 工程基建」分段。此后每次改动都有 diff 可审。

### P2-3 clippy 未安装、从未 lint

- **状态**：**未完成**，如实说明。`cargo-clippy.exe is not installed for the toolchain
  'stable-x86_64-pc-windows-gnu'`，无法执行。
- **已做的替代措施**：`cargo check --all-targets` 做到**零错误、零代码警告**
  （原基线有多处 `unused import` / `unused_mut` / `dead_code`，逐一清理；
  确实预留未用的 API 用 `#[allow(dead_code)]` + 注释说明用途，而不是删除）。
- **后续**：`rustup component add clippy` 后再跑，预计能补出 `as` 截断转换等问题。

### P2-4 无 CI

- **改法**：新增 `scripts/ci.ps1`（6 步，任一失败即返回该步编号）：
  1. `cargo check --all-targets`（含 build.rs 图标生成）
  2. `cargo test`（单元 + 集成）
  3. `cargo build`（debug，供 E2E）
  4. `cargo build --release`
  5. **体积护栏**（`pixshot.exe ≤ 2.6 MB`，超出即失败并提示检查新增依赖/未裁剪 feature）
  6. GUI E2E（可用 `-SkipE2E` 关闭）
- **设计细节**：原生命令失败不会触发 PowerShell 异常，因此每步显式校验 `$LASTEXITCODE`；
  E2E 用**独立进程**执行（`Start-Process -Wait`），因为 E2E 脚本内部以 `exit` 返回结果，
  直接 `&` 调用会终止当前会话。

### P2-5 README 与实现漂移

- **改法**（逐条核对后修正）：
  - 项目结构树：补 `tray.rs` / `toolbar.rs` / `menu.rs` / `toast.rs` / `config.rs` /
    `icon_geometry.rs` 与 `tests/`，删除已不存在的 `traymenu.rs`；
  - 脚本清单：删掉**并不存在**的 `verify_fix_single_hook.ps1`，补全漏列的
    `fix_integrity.ps1`、`verify_tray_realclick.ps1`、`verify_fluent.ps1`、
    `verify_fix5b.ps1`、`measure_baseline.ps1`、`final_accept.ps1` 与新增的 `ci.ps1`；
  - E2E 项数：原「16 项」但正文只列 10 条、示例又说「15 项通过」，三处互斥 →
    改为按脚本实际 `Check` 调用数（消息驱动 16 / 物理 15），并说明列表是「能力点」而非「断言条数」；
  - 体积：2.11 MB → 实测 **2.17 MB**；
  - 新增章节：配置文件、图标嵌入与构建工具链（windres / rc.exe 双路径）、环境变量
    `PIXSHOT_NO_ICON`；
  - 设计说明补：一次捕获管线、并发与生命周期、菜单与提示、热键两条路径及其固有风险。

### P2-6 根目录混杂临时产物

- **改法**：
  - 8 个文件移入 `artifacts/`：`debug.log`（原为搜狗输入法日志）、`tray_realrt2.txt`、
    `fix5_verify.txt`、`fluent_verify.txt`、`perf_baseline.txt`、`final_accept.txt`、
    `pixshot_smoke.png`、`ci_run.log`；
  - `.gitignore` 重写为分组结构：构建产物（`/target`）、运行产物
    （`/artifacts/`、`/e2e_artifacts/`、`pixshot_diag.log`、`pixshot_smoke.png`）、
    编辑器杂项、工具数据（`.workbuddy/`、`/outputs/`）。
    其中 `.workbuddy/` 只做「不入库」，并在注释里写明**不删除**（它是项目数据目录）。

### P2-7 系统 `MessageBoxW` 承担全部提示

- **文件**：`src/toast.rs`（新增 211 行）、调用点 `src/results.rs`
- **改法**：非模态 Toast —— `WS_EX_NOACTIVATE`（不抢焦点）、`SetTimer` 自动消失
  （时长读 `config.toast_ms`，默认 1600ms）、点击即关、`DrawTextW(DT_CALCRECT)` 精确测量、
  **复用已有窗口**避免反复创建闪烁、位置在光标右下并越界翻转。
- **效果**：「已复制到剪贴板」不再弹模态框，E2E 也不必再专门 `PostMessage(WM_CLOSE)` 关框。

### P2-8 贴图右键是系统菜单、托盘右键是自绘菜单

- **文件**：`src/menu.rs`（新增 402 行），删除 `src/traymenu.rs`
- **改法**：托盘与贴图共用同一套自绘 Fluent 菜单：
  - `Item::new(label)` / `Item::sep()` / `Item::disabled(label)`，`show(x, y, items) -> Option<usize>`；
  - 每个实例独立 `Box<Data>` 挂 `GWLP_USERDATA`，循环结束后读 `chosen` 再释放（避免循环中悬垂）；
  - `measure()` 用 `DT_CALCRECT` 算宽，`hit_test()` 让分隔线不响应点击；
  - 修正贴边翻转「用宽度判断高度」的历史 bug：`let mh = menu_height(d, dpi);`
    `let mut py = if y + mh > vy + vh { y - mh } else { y };`
  - 用 `SetCapture` 而非 `TrackPopupMenuEx`（后者要求宿主能成为前台窗口，隐藏托盘宿主上实测静默失败）。
- **贴图菜单项**：复制图像 / 保存为 {当前格式}… / 识别文字 (OCR) / 重置缩放 (100%) / 关闭贴图 (Esc)。

### P2-9 零配置能力

- **文件**：`src/config.rs`（新增 400 行）
- **位置**：`%APPDATA%\PixShot\config.ini`（无 `APPDATA` 回退 exe 同目录），UTF-8，
  首次启动生成带注释模板。
- **字段与解决的原问题**：

  | 字段 | 解决的原问题 |
  |---|---|
  | `hotkey` | 「F1 硬编码」→ 支持 `Ctrl+Shift+S` 等任意组合 |
  | `format` | 「保存格式只有 PNG」→ png / jpg / bmp |
  | `save_dir` | 每次保存都从系统默认位置开始 → 记住上次目录并回写 |
  | `ocr_lang` | OCR 语言只能走回退链 → 可固定 |
  | `toast_ms` | 提示时长不可调 |

- **解析**：手写 `key = value` 解析器（零依赖），支持 `#` / `;` 注释；
  `parse_detailed(text) -> (Config, Vec<String>)` 返回告警，`load()` 把告警写诊断日志；
  非法值**回退默认而非报错** —— 配置文件写坏不会导致启动失败。
- **未做**：开机自启、截图历史（原报告提到但不在本轮范围，且涉及注册表/存储设计）。

---

## 四、文件级变更汇总

### 新增（6 个）

| 文件 | 行数 | 用途 |
|---|---|---|
| `src/config.rs` | 400 | 配置读写与热键解析 |
| `src/menu.rs` | 402 | 统一自绘菜单 |
| `src/toolbar.rs` | 454 | 操作工具栏（从 main.rs 拆出） |
| `src/tray.rs` | 456 | 托盘 + 热键 + 单实例（从 main.rs 拆出） |
| `src/toast.rs` | 211 | 非模态提示 |
| `src/icon_geometry.rs` | 81 | 图标几何（build.rs 与 tests 共用） |

### 重写（11 个）

`src/main.rs`（987 → 320）、`src/capture.rs`、`src/overlay.rs`、`src/util.rs`、
`src/theme.rs`、`src/clipboard.rs`、`src/imageio.rs`、`src/ocr.rs`、`src/pin.rs`、
`src/results.rs`、`build.rs`

### 删除（1 个）

`src/traymenu.rs`（功能并入 `src/menu.rs`）

### 新增工程基建（5 项）

| 文件 | 说明 |
|---|---|
| `tests/icon_geometry.rs` | 14 项集成测试：端点/对称性/越界/退化输入/常量约束 |
| `scripts/ci.ps1` | 6 步流水线：check → test → build → release → 体积护栏 → E2E（`-SkipE2E` 可跳过） |
| `artifacts/` | 根目录临时产物归档（原 debug.log、*.txt、pixshot_smoke.png 等 8 个文件） |
| `.gitignore` | 重写：构建产物 / 运行产物（artifacts、e2e_artifacts、diag log）/ 编辑器杂项 / 工具数据 |
| `git` 仓库 | `git init -b main`，首次提交 `34eaade`，35 个文件入库 |

---

## 五、兼容性保证（E2E 契约未变）

以下被 E2E 脚本硬编码依赖的标识**全部保持原样**：

| 契约项 | 保持的值 |
|---|---|
| 窗口类名 | `PixShotOverlay`、`PixShotToolbar`、`PixShotPin`、`PixShotResults`、`PixShotMain`（新增 `PixShotMenu`、`PixShotToast`） |
| 工具栏按钮控件 ID | `BTN_PIN=1`、`BTN_COPY=2`、`BTN_SAVE=3`、`BTN_OCR=4`、`BTN_CLOSE=5` |
| 按钮文本 | `贴图` / `复制` / `保存` / `OCR 识别` / `关闭` / `✕` |
| 状态文本格式 | `已截取 {} × {} 像素`、`选择操作：` |
| 贴图窗口标题 | 原样 |
| 窗口消息 | `WM_APP` 系列（`WM_OCR_DONE` 等）编号未变 |
| 热键行为 | 默认 F1；`RegisterHotKey` 优先、钩子兜底的策略未变 |
| 托盘回调 | V4 布局解析逻辑未变（新增 V0 分支，不影响 V4） |

唯一的行为差异是**新增**：配置可改热键/格式/目录、Toast 替代 MessageBox、右键菜单多项目化。
这三项都不改变既有控件的存在与文本，E2E 断言不受影响。

---

## 六、本轮未做（明确说明）

| 项 | 原因 | 建议 |
|---|---|---|
| `--smoke` 实测 | 本机安全程序限制 Desktop 目录下 exe 的文件读写（非代码问题） | 把 exe 复制到 `%TEMP%` 后执行 `pixshot.exe --smoke` |
| clippy lint | 工具链未安装组件（`cargo-clippy.exe is not installed for 'stable-x86_64-pc-windows-gnu'`） | `rustup component add clippy` 后再跑 |
| 配置热重载 | `config::reload()` 已实现并保留，但未接 UI 触发 | 可在托盘菜单加「重新加载配置」 |
| 单实例唤起 | 无 IPC 通道，第二实例直接退出 | 需要命名管道或 `WM_COPYDATA` |

> E2E 实测与断言自检原列在此表，已于第二批修复中完成并实测通过（见下节）。

---

## 七、第二批修复：E2E 跑通（实测记录）

### 现象
用户执行 `scripts\ci.ps1` 后，E2E 第一步即失败：
`主窗口（托盘宿主）未创建`（`gui_e2e_test.ps1:203`）。

### 环境事实（先测清楚，避免误判为代码故障）
| 探测项 | 结果 |
|---|---|
| pixshot 进程是否启动 | **是**，诊断日志有 `[boot] 主窗口创建成功 hwnd=0x8B0B78` |
| 枚举该进程顶层窗口 | `class='PixShotMain' title='PixShot' visible=False`（**窗口确实存在**） |
| `FindWindowW('PixShotMain', $null)` | **0x0**（找不到） |
| `FindWindowW('PixShotMain', 'PixShot')` | 0x1AA09EE ✅ |
| `FindWindowW('PixShotMain', [NullString]::Value)` | 0x1AA09EE ✅ |
| `RegisterHotKey(F1)` / `(F3)` | **均失败，err=1409**（已被其他程序占用） |
| `RegisterHotKey(F2)` | 成功 → 说明是 F1/F3 被独占，非系统限制 |

F1/F3 的占用者是桌面上常驻的 **FastStone Capture**（默认截图热键即 F1/F3）。

### 五个根因与修法（由浅入深，逐个被实测暴露）

**① `$null` 传给 `FindWindowW` 的标题参数 → 语义被改写**
PowerShell 把 `$null` 自动转成 `""`（`[string]$null` 长度 0、非 null），而
`FindWindowW` 对 `lpWindowName=""` 的语义是「**标题必须为空字符串**」，不是「任意标题」。
主窗口标题被我改成了 `PixShot`（原来为空串），于是按类名查找永远返回 0。
→ 新增 `FindWindowClass` / `FindChildClass`（`IntPtr` 重载，显式传 `[IntPtr]::Zero`），
`Wait-Window` 与所有「只按类名」的查找全部改用它。

**② 菜单已改自绘，脚本还在找系统菜单 `#32768`**
P2-8 把菜单统一成 `PixShotMenu` 后，`FindWindowW('#32768', ...)` 永远返回 0，
物理版第 7 步「↓×3 + Enter 选 OCR」三轮全部 `continue` 跳过。
→ 查找改 `PixShotMenu`；同时因为自绘菜单**只处理 Esc、不处理方向键**，
选菜单项必须改成**鼠标点击**：新增 `Get-MenuItemCenter`（按 `menu.rs` 的
`pad=6 / item_h=34 / sep_h=9` 逻辑像素 × dpi 换算，并考虑第 4、5 项前的分隔线）
与 `Select-PinMenuItem`。

**③ 菜单与 OCR 窗口的点击目标被遮挡 / Z 序不对**
`SetWindowPos(..., SWP_NOZORDER)` 只移动不改 Z 序：OCR 结果窗口是普通顶层窗口，
被最大化的编辑器盖住 —— 此时 `GetWindowRect` 与找子控件**都成功**，但注入的点击
落到下面那个窗口上，表现为「点得到按钮却没复制」。
贴图中心则恰好被 FastStone 的置顶取色器面板压住，右键被它吃掉，菜单弹不出来。
→ 两处都改用 `HWND_TOPMOST` 显式置顶；第 10 步前再提升一次贴图的 Z 序。

**④ 钩子有意过滤注入按键，E2E 的热键路径被自己挡住**
`ll_keyboard_proc` 里 `if is_down && !injected && ...` —— 默认忽略 `LLKHF_INJECTED`，
避免键盘宏误触发。但 E2E 只能用 `keybd_event` 注入 F1，而 F1 被占用又必然走钩子兜底
路径 → 钩子永远收不到（被忽略）。
→ 新增环境变量 `PIXSHOT_ALLOW_INJECTED=1`（`static ALLOW_INJECTED: AtomicBool`），
仅显式设置时放行注入按键；E2E 脚本设置该变量，**默认行为不变**。

**⑤ 断言本身是假的（P0-7 的根因，比原报告推测更具体）**
`Check '识别结果可复制到剪贴板' ($clipS.Trim() -eq $text.Trim())` 在
「剪贴板 11 字符 / 窗口 212 字符」时仍判 PASS。两层原因：
- `Get-ClipText` 的 `for` 重试循环**漏了 `return`**：读完一次不退出，跑满 10 轮，
  函数返回「10 份文本 + 末尾空串」共 **11 个元素的数组**（`$clipS.Length` = 元素数 = 11）；
- PowerShell 的 `-eq` 作用于**集合**时返回**筛选结果（数组）**而非布尔，
  而非空数组在 `if` 中恒为真 → 假 PASS。
→ `Get-ClipText` 加 `return` 并预置 `$h`；两个脚本的 `Check` 改为**严格布尔判定**：
非 `[bool]` 一律 FAIL 并打印「断言告警：判定值不是布尔（实际 xxx）」。

另外顺带修掉：`Win32` 类里**从未声明过 `SendMessageW`**（`Get-ControlText` 一直在用），
之前从未执行到该步所以未暴露；消息驱动版两处等待 `#32770` 消息框的死代码（已改 Toast，
会白等 3 秒）也一并清理。

### 实测结果

| 脚本 | 结果 |
|---|---|
| `gui_e2e_test.ps1`（物理驱动） | **14 项通过，0 失败**（剪贴板 212 字符 = 窗口 212 字符） |
| `gui_e2e_msg_test.ps1`（消息驱动） | **16 项通过，0 失败**（剪贴板 107 字符 = 窗口 107 字符） |
| `ci.ps1`（完整 6 步） | **全部通过**：check 2.0s → test 3.1s → build 0.1s → release 3.5s → 体积 2.17 MB / 2.60 MB → E2E 11.9s |

### 仍然存在的环境限制（非代码问题）
- 本机 F1/F3 被 FastStone Capture 占用，pixshot 因此**始终走钩子兜底**而非
  `RegisterHotKey` 主路径。两条路径都已实测可用，但若想验证主路径，
  需先退出 FastStone Capture（或用 `config.ini` 换成未被占用的键，如 F2）。
- E2E 期间会接管鼠标键盘约 12~40 秒，且要求点击目标可见（脚本已用置顶规避大部分干扰，
  但若桌面有全屏置顶窗口仍会干扰）。
