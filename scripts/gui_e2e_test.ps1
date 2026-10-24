<#
PixShot GUI 端到端自动化测试
============================
在真实桌面上自动化跑通完整流程：
  框选截图 → 工具条（悬停提示 + 快捷键 Ctrl+C / Ctrl+T）→ 贴图置顶
  （位置/置顶/拖动/滚轮缩放验证）→ 右键菜单 OCR（文字进剪贴板，与工具条一致）。

前置条件：
  - 已执行 cargo build（默认使用 target\debug\pixshot.exe）
  - 系统已安装 OCR 语言包（Windows.Media.Ocr，见 README）

注意：脚本运行期间（约 20 秒）会接管鼠标和键盘，请勿操作桌面。
      脚本自身以 Per-Monitor-V2 DPI 感知运行，125% 缩放下坐标仍为物理像素。

用法：
  pwsh -File scripts\gui_e2e_test.ps1 [-Exe <pixshot.exe>] [-SelX 300 -SelY 300 -SelW 600 -SelH 400]

输出：
  - PASS/FAIL 检查清单；退出码 0 = 全部通过
  - e2e_artifacts\pin_visible.png    贴图置顶显示存档
  - e2e_artifacts\final_state.png    全流程结束状态存档
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\debug\pixshot.exe'),
    [int]$SelX = 300,
    [int]$SelY = 300,
    [int]$SelW = 600,
    [int]$SelH = 400
)

$env:PIXSHOT_NO_UI_ALERTS = '1'   # 沙箱下托盘注册必失败，屏蔽启动提示框以保证测试确定性
# 本脚本用 keybd_event 注入 F1 来触发全局热键，注入按键带 LLKHF_INJECTED 标志。
# 程序默认忽略注入按键（防止键盘宏误触发）；且当环境里 F1 已被别的程序占用时
# （RegisterHotKey 返回 1409）只能走低级钩子兜底路径，该路径必须放行注入才能被测试。
$env:PIXSHOT_ALLOW_INJECTED = '1'
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "找不到 $Exe，请先执行 cargo build" }
$art = Join-Path $PSScriptRoot '..\e2e_artifacts'
New-Item -ItemType Directory -Force -Path $art | Out-Null
try { Start-Transcript -Path (Join-Path $art 'e2e_transcript.txt') -Force | Out-Null } catch { }
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class Win32 {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    // 注意：绝对不要把 $null 直接传给「标题」参数！
    // PowerShell 会把 $null 自动转成空字符串 ""，而 FindWindowW 对 "" 的语义是
    // 「标题必须为空字符串」，不是「任意标题」—— 实测结果是按类名查找永远返回 0。
    // 需要「只按类名 / 不限定标题」时，用下面 FindWindowClass / FindChildClass
    // 这两个 IntPtr 重载，显式传 [IntPtr]::Zero。
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindWindowClass(string c, IntPtr t);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr a, string c, string t);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowExW")] public static extern IntPtr FindChildClass(IntPtr p, IntPtr a, string c, IntPtr t);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    // 跨进程读取控件文本要用 SendMessage(WM_GETTEXT)，GetWindowTextW 只能读窗口标题缓存。
    // 注意：这个声明此前一直缺失（脚本从未执行到 Get-ControlText 这一步），首次跑到才暴露。
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetWindowLongW(IntPtr h, int i);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr wnd, IntPtr after, int x, int y, int w, int h, uint flags);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
    // 剪贴板（直接 Win32 读取，避免 PowerShell Get-Clipboard 在 MTA/占用时的不稳定）
    [DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool CloseClipboard();
    [DllImport("user32.dll")] public static extern bool IsClipboardFormatAvailable(uint fmt);
    [DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint fmt);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr hmem);
    [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr hmem);
}
"@
# Win32 方式读取剪贴板文本（CF_UNICODETEXT=13）
function Get-ClipText {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32]::OpenClipboard([IntPtr]::Zero)) {
            $h = [IntPtr]::Zero          # 预置，供 finally 安全引用
            try {
                if (-not [Win32]::IsClipboardFormatAvailable(13)) { return '' }
                $h = [Win32]::GetClipboardData(13)
                if ($h -eq [IntPtr]::Zero) { return '' }
                $p = [Win32]::GlobalLock($h)
                if ($p -eq [IntPtr]::Zero) { return '' }
                $len = 0
                while ($len -lt 200000 -and [Runtime.InteropServices.Marshal]::ReadInt16($p, $len * 2) -ne 0) { $len++ }
                # 必须 return：否则本行只作为管道输出，for 会继续跑满 10 轮，
                # 函数最终返回「10 份文本 + 末尾空串」的数组（表现为 Length=11 的假值）。
                return [Runtime.InteropServices.Marshal]::PtrToStringUni($p, $len)
            } finally {
                [void][Win32]::GlobalUnlock($h)
                [void][Win32]::CloseClipboard()
            }
        }
        Start-Sleep -Milliseconds 40
    }
    return ''
}
# Win32 方式检查剪贴板是否含位图（CF_DIB=8）
function Test-ClipDib {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32]::OpenClipboard([IntPtr]::Zero)) {
            try { return [Win32]::IsClipboardFormatAvailable(8) }
            finally { [void][Win32]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
}

# 物理像素坐标（DPI 感知；125% 缩放下也准确）
[void][Win32]::SetProcessDpiAwarenessContext([IntPtr](-4))
$dpi = [Win32]::GetDpiForSystem()
$sw = [Win32]::GetSystemMetrics(0)
$sh = [Win32]::GetSystemMetrics(1)
Write-Host ("环境：屏幕 {0}x{1} 物理像素，系统 DPI {2}（{3}%）" -f $sw, $sh, $dpi, [int]($dpi / 96 * 100))
if (($SelX + $SelW) -ge $sw -or ($SelY + $SelH) -ge $sh) { throw "选区超出屏幕范围" }

# ─── 辅助函数 ─────────────────────────────────────────────
function Wait-Window([string]$Class, [int]$TimeoutMs = 5000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        # 只按类名匹配：标题参数显式传 0（不能传 $null，见 P/Invoke 声明处注释）
        $h = [Win32]::FindWindowClass($Class, [IntPtr]::Zero)
        if ($h -ne [IntPtr]::Zero) { return $h }
        if ([Environment]::TickCount -ge $deadline) { return [IntPtr]::Zero }
        Start-Sleep -Milliseconds 80
    }
}
function Get-Rect([IntPtr]$H) {
    $r = New-Object Win32+RECT
    [void][Win32]::GetWindowRect($H, [ref]$r); $r
}
function Get-Center([IntPtr]$H) {
    $r = Get-Rect $H; @([int](($r.L + $r.R) / 2), [int](($r.T + $r.B) / 2))
}
function Get-Text([IntPtr]$H) {
    $sb = New-Object System.Text.StringBuilder 8192
    [void][Win32]::GetWindowTextW($H, $sb, 8192); $sb.ToString()
}
# 跨进程读取控件真实文本（EDIT 须用 WM_GETTEXT；GetWindowTextW 只能读标题缓存）
function Get-ControlText([IntPtr]$H) {
    $len = [Win32]::SendMessageW($H, 0x000E, [IntPtr]::Zero, [IntPtr]::Zero).ToInt32()   # WM_GETTEXTLENGTH
    if ($len -lt 0) { $len = 0 }
    $buf = [Runtime.InteropServices.Marshal]::AllocHGlobal((($len + 1) * 2))
    try {
        [void][Win32]::SendMessageW($H, 0x000D, [IntPtr]($len + 1), $buf)                 # WM_GETTEXT
        [Runtime.InteropServices.Marshal]::PtrToStringUni($buf, $len)
    } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($buf) }
}
function Mouse([uint32]$F) { [Win32]::mouse_event($F, 0, 0, 0, [UIntPtr]::Zero) }
function Click-At([int]$X, [int]$Y) {
    [void][Win32]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 90
    Mouse 0x02; Start-Sleep -Milliseconds 50; Mouse 0x04; Start-Sleep -Milliseconds 110
}
function RightClick-At([int]$X, [int]$Y) {
    [void][Win32]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 90
    Mouse 0x08; Start-Sleep -Milliseconds 50; Mouse 0x10; Start-Sleep -Milliseconds 200
}
function Drag-Mouse([int]$X1, [int]$Y1, [int]$X2, [int]$Y2) {
    [void][Win32]::SetCursorPos($X1, $Y1); Start-Sleep -Milliseconds 160
    Mouse 0x02
    $n = 14
    for ($i = 1; $i -le $n; $i++) {
        [void][Win32]::SetCursorPos(($X1 + [int](($X2 - $X1) * $i / $n)), ($Y1 + [int](($Y2 - $Y1) * $i / $n)))
        Start-Sleep -Milliseconds 30
    }
    Start-Sleep -Milliseconds 90
    Mouse 0x04; Start-Sleep -Milliseconds 130
}
function Wheel([int]$Delta) { [Win32]::mouse_event(0x0800, 0, 0, [uint32]$Delta, [UIntPtr]::Zero); Start-Sleep -Milliseconds 220 }
function Press-Key([byte]$Vk) {
    [Win32]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 90
}
# Ctrl+<键>（工具条快捷键必须用**真实** Ctrl 状态，程序侧读 GetKeyState）
function Press-Ctrl([byte]$Vk) {
    [Win32]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40   # Ctrl down
    [Win32]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 50
    [Win32]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 150  # Ctrl up
}
function Wait-WindowGone([string]$Class, [int]$TimeoutMs = 4000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $deadline) {
        if ([Win32]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero) { return $true }
        Start-Sleep -Milliseconds 80
    }
    ([Win32]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero)
}
# 触发一次全新截图：F1 → 框选 (SelX,SelY,SelW,SelH) → 返回工具条句柄
function Invoke-Capture {
    Press-Key 0x70
    Start-Sleep -Milliseconds 600
    $o = Wait-Window 'PixShotOverlay' 12000
    if ($o -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    Start-Sleep -Milliseconds 500
    Drag-Mouse $SelX $SelY ($SelX + $SelW) ($SelY + $SelH)
    return (Wait-Window 'PixShotToolbar' 6000)
}
function Find-ChildById([IntPtr]$Parent, [int]$Id) {
    $c = [Win32]::GetWindow($Parent, 5)          # GW_CHILD
    while ($c -ne [IntPtr]::Zero) {
        if ([Win32]::GetDlgCtrlID($c) -eq $Id) { return $c }
        $c = [Win32]::GetWindow($c, 2)           # GW_HWNDNEXT
    }
    [IntPtr]::Zero
}
# ─── 自绘菜单（PixShotMenu）的项定位 ───────────────────────────
# 菜单是纯自绘窗口（无子控件、无菜单文本可读），只能按几何位置点击。
# 以下常量必须与 src/menu.rs 的 measure()/hit_test() 保持一致：
#   pad = scaled(6)、item_h = scaled(34)、sep_h = scaled(9)（均为逻辑像素 × dpi/96）
# 贴图菜单项顺序（src/pin.rs）：
#   0 复制图像 / 1 保存为 <格式>… / 2 识别文字 (OCR) / 3[分隔线] 重置缩放 / 4[分隔线] 关闭贴图
function Get-MenuItemCenter([IntPtr]$Menu, [int]$Index) {
    $d = [math]::Max([int][Win32]::GetDpiForWindow($Menu), [int]$dpi)
    $s = $d / 96.0
    $pad = 6; $itemH = 34; $sep = 9
    $sepBefore = @(3, 4)             # 0 基索引：第 4、5 项前有分隔线
    $top = $pad
    $centerY = $top
    for ($i = 0; $i -le $Index; $i++) {
        if ($i -gt 0 -and ($sepBefore -contains $i)) { $top += $sep }
        if ($i -eq $Index) { $centerY = $top + [int]($itemH / 2); break }
        $top += $itemH
    }
    $r = Get-Rect $Menu
    @(
        [int](($r.L + $r.R) / 2),
        [int]($r.T + [math]::Round($centerY * $s))
    )
}
# 点开贴图右键菜单并点击指定项；返回是否成功点中
function Select-PinMenuItem([IntPtr]$Pin, [int]$Index) {
    $c = Get-Center $Pin
    $alive = [Win32]::IsWindow($Pin)
    Write-Host ("  [调试] 右键贴图 center=({0},{1}) pin alive={2}" -f $c[0], $c[1], $alive)
    if (-not $alive) { return $false }
    RightClick-At $c[0] $c[1]
    $menu = Wait-Window 'PixShotMenu' 2500
    Write-Host ("  [调试] PixShotMenu 句柄=0x{0:X}" -f [int64]$menu)
    if ($menu -eq [IntPtr]::Zero) { return $false }
    Start-Sleep -Milliseconds 180
    $pt = Get-MenuItemCenter $menu $Index
    Write-Host ("  [调试] 菜单项 #{0} 点击=({1},{2})" -f $Index, $pt[0], $pt[1])
    Click-At $pt[0] $pt[1]
    Start-Sleep -Milliseconds 250
    return $true
}

function Shot([int]$X, [int]$Y, [int]$W, [int]$H, [string]$File) {
    $bmp = New-Object System.Drawing.Bitmap($W, $H)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size($W, $H)))
    $g.Dispose(); $bmp.Save($File, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    Write-Host "  [存档] $File"
}

$script:Pass = 0; $script:Fail = 0
function Check([string]$Name, $Ok, [string]$Detail = '') {
    # 严格布尔判定。原因（实测踩过）：PowerShell 的 -eq 作用于「集合」时返回的是
    # 筛选结果（数组）而不是布尔值，而非空数组在 if 中恒为真 —— 于是出现过
    # 「剪贴板 11 字符 / 窗口 212 字符」却判 PASS 的自相矛盾记录。
    # 这里只接受真正的 [bool]，其他类型一律记失败并告警，绝不静默放行。
    $isOk = $false
    if ($Ok -is [bool]) {
        $isOk = $Ok
    } else {
        $t = if ($null -eq $Ok) { 'null' } else { $Ok.GetType().Name }
        Write-Host ("  [断言告警]「{0}」的判定值不是布尔（实际 {1}），已按失败处理" -f $Name, $t) -ForegroundColor Yellow
    }
    if ($isOk) { $script:Pass++ } else { $script:Fail++ }
    Write-Host ("  [{0}] {1}{2}" -f $(if ($isOk) { 'PASS' } else { 'FAIL' }), $Name, $(if ($Detail) { " -- $Detail" } else { '' }))
}

Write-Host ''
Write-Host '==== PixShot GUI 端到端测试：框选截图 -> 贴图置顶(拖动/缩放) -> OCR ===='
Write-Host '  （脚本将接管鼠标键盘约 20 秒，请勿操作桌面）'
Write-Host ''

$proc = $null
try {
    # ── 1. 启动应用：常驻托盘（无遮罩弹出）；按 F1 全局热键进入截图流程 ──
    $proc = Start-Process -FilePath $Exe -PassThru
    $mainH = Wait-Window 'PixShotMain' 10000
    Start-Sleep -Milliseconds 800
    $noOv = ([Win32]::FindWindowClass('PixShotOverlay', [IntPtr]::Zero)) -eq [IntPtr]::Zero
    Check '启动后常驻托盘（无遮罩弹出）' $noOv
    if ($mainH -eq [IntPtr]::Zero) { throw '主窗口（托盘宿主）未创建' }
    Press-Key 0x70    # F1 全局热键（物理按键注入）
    Start-Sleep -Milliseconds 600
    $ov = Wait-Window 'PixShotOverlay' 12000
    Check 'F1 触发截图流程（遮罩出现）' ($ov -ne [IntPtr]::Zero) "PID=$($proc.Id)"
    if ($ov -eq [IntPtr]::Zero) { throw 'F1 未触发截图流程（热键被占用？）' }
    Start-Sleep -Milliseconds 600    # 等待遮罩完成显示并获得输入焦点
    $ovr = Get-Rect $ov
    Write-Host ("  [遮罩] rect=({0},{1})-({2},{3})" -f $ovr.L, $ovr.T, $ovr.R, $ovr.B)

    # ── 2. 拖拽框选 (SelX,SelY) → (+W,+H) ──
    Drag-Mouse $SelX $SelY ($SelX + $SelW) ($SelY + $SelH)
    Write-Host ("  [操作] 框选区域 ({0},{1}) {2}x{3}" -f $SelX, $SelY, $SelW, $SelH)

    # ── 3. 操作工具栏弹出，点击「贴图」按钮（控件 ID=1）──
    $tb = Wait-Window 'PixShotToolbar' 6000
    Check '框选完成 -> 操作工具栏弹出' ($tb -ne [IntPtr]::Zero)
    if ($tb -eq [IntPtr]::Zero) { throw '工具栏未出现（框选可能失败）' }
    $btnPin = Find-ChildById $tb 1
    if ($btnPin -eq [IntPtr]::Zero) { throw '工具栏上未找到「贴图」按钮' }

    # ── 3.1 悬停提示（真实光标）：移到「保存」图标上，延时后应弹出 PixShotTip 气泡 ──
    # 气泡是自绘窗口（非 comctl32），文案同步为窗口标题；它随工具条一同创建（初始隐藏），
    # 故这里必须校验 IsWindowVisible，不能只按类名查找。
    $btnSaveT = Find-ChildById $tb 3
    if ($btnSaveT -ne [IntPtr]::Zero) {
        $sr = Get-Rect $btnSaveT
        $sx = [int](($sr.L + $sr.R) / 2); $sy = [int](($sr.T + $sr.B) / 2)
        [void][Win32]::SetCursorPos($sx, $sy); Start-Sleep -Milliseconds 250
        [void][Win32]::SetCursorPos($sx + 1, $sy)   # 再动一下，确保按钮收到 WM_MOUSEMOVE
        $tipH = [IntPtr]::Zero
        $tipDeadline = [Environment]::TickCount + 2500
        while ([Environment]::TickCount -lt $tipDeadline) {
            $h = [Win32]::FindWindowClass('PixShotTip', [IntPtr]::Zero)
            if ($h -ne [IntPtr]::Zero -and [Win32]::IsWindowVisible($h)) { $tipH = $h; break }
            Start-Sleep -Milliseconds 50
        }
        $tipText = if ($tipH -ne [IntPtr]::Zero) { Get-Text $tipH } else { '' }
        # 气泡文案 = 说明文字 + 快捷键（Ctrl+S 见 src/toolbar.rs 的 SHORTCUTS）
        Check '悬停图标弹出提示气泡（真实光标 + 文案含快捷键）' `
            ($tipH -ne [IntPtr]::Zero -and $tipText -eq '保存图像  Ctrl+S') ("PixShotTip 文本='{0}'" -f $tipText)
    }

    # ── 3.2 快捷键 Ctrl+C = 复制并关闭：写剪贴板 CF_DIB 并收起工具条 ──
    Write-Host '  [操作] 按 Ctrl+C（工具条快捷键：复制并关闭）'
    Press-Ctrl 0x43
    Start-Sleep -Milliseconds 700
    Check '快捷键 Ctrl+C 复制图像到剪贴板（CF_DIB）' (Test-ClipDib)
    Check '快捷键 Ctrl+C 后工具条自动关闭' (Wait-WindowGone 'PixShotToolbar' 4000)

    # ── 3.3 快捷键 Ctrl+T = 贴图：创建贴图窗口并收起工具条 ──
    $tb2 = Invoke-Capture
    Check '再次框选 -> 工具条弹出' ($tb2 -ne [IntPtr]::Zero)
    if ($tb2 -eq [IntPtr]::Zero) { throw '第二次框选未出现工具条' }
    Write-Host '  [操作] 按 Ctrl+T（工具条快捷键：贴图）'
    Press-Ctrl 0x54
    Start-Sleep -Milliseconds 700
    Check '快捷键 Ctrl+T 后工具条自动关闭' (Wait-WindowGone 'PixShotToolbar' 4000)

    # ── 4. 贴图窗口：位置 / 尺寸 / 置顶 ──
    $pin = Wait-Window 'PixShotPin' 6000
    Check '贴图窗口已创建' ($pin -ne [IntPtr]::Zero)
    if ($pin -eq [IntPtr]::Zero) { throw '贴图窗口未出现' }
    $r0 = Get-Rect $pin
    # 贴图窗口 = 图像 + 四周投影留白（16 逻辑像素 × 显示器 DPI）：
    # 图像内容仍严格落在原截图位置，窗口整体外扩一圈用于绘制柔和投影。
    $pinDpi = [Win32]::GetDpiForWindow($pin)
    if ($pinDpi -lt 96) { $pinDpi = 96 }
    $pad = [int][Math]::Round(16 * $pinDpi / 96.0)
    Check '贴图显示于原截图位置（图像内容偏移一圈留白，初始 100% 缩放）' `
        (($r0.L -eq ($SelX - $pad)) -and ($r0.T -eq ($SelY - $pad)) -and (($r0.R - $r0.L) -eq ($SelW + 2 * $pad)) -and (($r0.B - $r0.T) -eq ($SelH + 2 * $pad))) `
        ("rect=({0},{1})-({2},{3}) 图像应位于 ({4},{5}) {6}x{7}，投影留白 {8}px" -f $r0.L, $r0.T, $r0.R, $r0.B, $SelX, $SelY, $SelW, $SelH, $pad)
    $ex = [Win32]::GetWindowLongW($pin, -20)     # GWL_EXSTYLE
    Check '贴图窗口置顶（WS_EX_TOPMOST）' (($ex -band 8) -ne 0) ("exstyle=0x{0:X8}" -f $ex)
    Shot ($SelX - 30) ($SelY - 30) ($SelW + 60) ($SelH + 60) (Join-Path $art 'pin_visible.png')

    # ── 5. 按住左键拖动贴图 (+140,+60) ──
    $c = Get-Center $pin
    Drag-Mouse $c[0] $c[1] ($c[0] + 140) ($c[1] + 60)
    $r1 = Get-Rect $pin
    Check '按住左键拖动贴图窗口' `
        (([Math]::Abs($r1.L - ($r0.L + 140)) -le 3) -and ([Math]::Abs($r1.T - ($r0.T + 60)) -le 3)) `
        ("移动后 rect=({0},{1})" -f $r1.L, $r1.T)

    # ── 6. 滚轮缩放（两次 +120 → 约 1.21 倍）──
    $c = Get-Center $pin
    [void][Win32]::SetCursorPos($c[0], $c[1]); Start-Sleep -Milliseconds 160
    Wheel 120; Wheel 120
    $r2 = Get-Rect $pin
    $gw = ($r2.R - $r2.L) - ($r1.R - $r1.L)
    $gh = ($r2.B - $r2.T) - ($r1.B - $r1.T)
    Check '滚轮缩放贴图（两次放大）' (($gw -gt [int]($SelW * 0.15)) -and ($gh -gt [int]($SelH * 0.15))) ("尺寸 +${gw}x+${gh}px")

    # ── 7. 右键菜单 → 「识别文字 (OCR)」：与工具条 OCR **行为完全一致** ──
    # 即：后台识别 → 文字写入剪贴板 → 主窗口 Toast；**不再弹出 OCR 结果窗口**。
    # 菜单是自绘窗口 PixShotMenu，只处理 Esc，必须用鼠标点击菜单项。
    # 贴图菜单第 3 项（0 基索引 2）即「识别文字 (OCR)」。
    $clipBefore = Get-ClipText
    $menuOk = $false
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        $menuOk = Select-PinMenuItem $pin 2
        if ($menuOk) { break }
        Write-Host ("  [重试] 第 {0} 次未等到贴图右键菜单弹出" -f $attempt)
        Start-Sleep -Milliseconds 400
    }
    Check '右键菜单选择「识别文字 (OCR)」' $menuOk '自绘菜单已弹出，已点击第 3 项'

    # ── 8. 等待识别完成：文字进入剪贴板（CF_UNICODETEXT）──
    $ocrText = ''
    $deadline = [Environment]::TickCount + 25000
    while ([Environment]::TickCount -lt $deadline) {
        $t = Get-ClipText
        if ($t.Trim().Length -gt 0 -and $t -ne $clipBefore) { $ocrText = $t; break }
        Start-Sleep -Milliseconds 250
    }
    Check '右键菜单 OCR 把文字写入剪贴板（与工具条一致）' ($ocrText.Trim().Length -gt 0) ("共 $($ocrText.Length) 字符")
    Check '右键菜单 OCR 不再弹出结果窗口' ((Wait-Window 'PixShotOcrResult' 800) -eq [IntPtr]::Zero)
    if ($ocrText) {
        $prev = if ($ocrText.Length -gt 200) { $ocrText.Substring(0, 200) + '...' } else { $ocrText }
        $oneline = ($prev -replace "[\r\n]+", " | ")
        Write-Host ("  [OCR 文本预览] {0}" -f $oneline)
    }

    # ── 9. 贴图菜单「复制图像」→ 验证剪贴板位图（截图复制功能）──
    # 第 1 项（0 基索引 0）即「复制图像」；同样走自绘菜单 + 鼠标点击。
    # 先把贴图重新提到 TOPMOST 层顶部：环境里可能有其他置顶浮窗（如取色器）
    # 恰好压在贴图中心，右键会被它吃掉，导致菜单弹不出来。
    $SWP_TOP = 0x0001 -bor 0x0010               # NOSIZE | NOACTIVATE
    $HWND_TOPMOST = [IntPtr](-1)
    [void][Win32]::SetWindowPos($pin, $HWND_TOPMOST, 1150, 100, 0, 0, $SWP_TOP)
    Start-Sleep -Milliseconds 250
    $clicked = Select-PinMenuItem $pin 0
    if ($clicked) {
        Start-Sleep -Milliseconds 700
        $imgOk = Test-ClipDib
        Check '截图可复制到剪贴板（CF_DIB 位图）' $imgOk '自绘菜单已弹出，已点击第 1 项「复制图像」'
    } else {
        Check '截图可复制到剪贴板（CF_DIB 位图）' $false '右键菜单未打开'
    }

    # ── 10. 最终存档：全屏（贴图 + 剪贴板 OCR 结果）──
    Shot 0 0 $sw $sh (Join-Path $art 'final_state.png')
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })

