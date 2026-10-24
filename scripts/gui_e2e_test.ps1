<#
PixShot GUI 端到端自动化测试
============================
在真实桌面上自动化跑通完整流程：
  框选截图 → 贴图置顶（位置/置顶/拖动/滚轮缩放验证）→ 右键菜单 OCR → 识别结果查看/复制。

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
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr p, IntPtr a, string c, string t);
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
            try {
                if (-not [Win32]::IsClipboardFormatAvailable(13)) { return '' }
                $h = [Win32]::GetClipboardData(13)
                if ($h -eq [IntPtr]::Zero) { return '' }
                $p = [Win32]::GlobalLock($h)
                if ($p -eq [IntPtr]::Zero) { return '' }
                $len = 0
                while ($len -lt 200000 -and [Runtime.InteropServices.Marshal]::ReadInt16($p, $len * 2) -ne 0) { $len++ }
                [Runtime.InteropServices.Marshal]::PtrToStringUni($p, $len)
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
        $h = [Win32]::FindWindowW($Class, $null)
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
function Find-ChildById([IntPtr]$Parent, [int]$Id) {
    $c = [Win32]::GetWindow($Parent, 5)          # GW_CHILD
    while ($c -ne [IntPtr]::Zero) {
        if ([Win32]::GetDlgCtrlID($c) -eq $Id) { return $c }
        $c = [Win32]::GetWindow($c, 2)           # GW_HWNDNEXT
    }
    [IntPtr]::Zero
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
    if ($Ok) { $script:Pass++ } else { $script:Fail++ }
    Write-Host ("  [{0}] {1}{2}" -f $(if ($Ok) { 'PASS' } else { 'FAIL' }), $Name, $(if ($Detail) { " -- $Detail" } else { '' }))
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
    $noOv = ([Win32]::FindWindowW('PixShotOverlay', $null)) -eq [IntPtr]::Zero
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
    $br = Get-Rect $btnPin
    $bx = [int](($br.L + $br.R) / 2); $by = [int](($br.T + $br.B) / 2)
    Write-Host ("  [操作] 点击「贴图」按钮 @ ({0},{1})" -f $bx, $by)
    Click-At $bx $by

    # ── 4. 贴图窗口：位置 / 尺寸 / 置顶 ──
    $pin = Wait-Window 'PixShotPin' 6000
    Check '贴图窗口已创建' ($pin -ne [IntPtr]::Zero)
    if ($pin -eq [IntPtr]::Zero) { throw '贴图窗口未出现' }
    $r0 = Get-Rect $pin
    Check '贴图显示于原截图位置（初始 100% 缩放）' `
        (($r0.L -eq $SelX) -and ($r0.T -eq $SelY) -and (($r0.R - $r0.L) -eq $SelW) -and (($r0.B - $r0.T) -eq $SelH)) `
        ("rect=({0},{1})-({2},{3})" -f $r0.L, $r0.T, $r0.R, $r0.B)
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

    # ── 7. 右键菜单 → 「识别文字 (OCR)」 ──
    # 键盘导航方案：正常情况（打开时无预选）↓×3 命中第 3 项；
    # 若系统预选了第 1 项则 ↓×3 会落在「重置缩放」（无害），换 ↓×2 再试。
    $ocrw = [IntPtr]::Zero
    $downsUsed = 0
    foreach ($downs in 3, 3, 2) {
        # 若上一轮菜单仍打开，先 Esc 关闭（仅当菜单确认打开时才按，避免误关贴图）
        $menu = [Win32]::FindWindowW('#32768', $null)
        if ($menu -ne [IntPtr]::Zero) { Press-Key 0x1B; Start-Sleep -Milliseconds 300 }
        $c = Get-Center $pin
        RightClick-At $c[0] $c[1]
        Start-Sleep -Milliseconds 450
        $menu = [Win32]::FindWindowW('#32768', $null)
        if ($menu -eq [IntPtr]::Zero) { continue }
        for ($i = 0; $i -lt $downs; $i++) { Press-Key 0x28 }
        Start-Sleep -Milliseconds 150
        Press-Key 0x0D
        $ocrw = Wait-Window 'PixShotOcrResult' 2500
        $downsUsed = $downs
        if ($ocrw -ne [IntPtr]::Zero) { break }
        Write-Host ("  [重试] ↓x$downs 未命中 OCR 项，换下一种键盘导航方案")
    }
    Check '右键菜单选择「识别文字 (OCR)」' ($ocrw -ne [IntPtr]::Zero) "命中方案 ↓x$downsUsed"
    if ($ocrw -eq [IntPtr]::Zero) { throw 'OCR 结果窗口未出现' }

    # ── 8. 等待识别完成，读取结果文本 ──
    $title = ''
    $deadline = [Environment]::TickCount + 25000
    while ([Environment]::TickCount -lt $deadline) {
        $title = Get-Text $ocrw
        if ($title -match '识别结果' -or $title -match '识别失败') { break }
        Start-Sleep -Milliseconds 250
    }
    Check 'OCR 后台识别完成' ($title -match '识别结果') $title
    $edit = [Win32]::FindWindowExW($ocrw, [IntPtr]::Zero, 'EDIT', $null)
    $text = ''
    if ($edit -ne [IntPtr]::Zero) { $text = Get-ControlText $edit }
    Check '识别文本可在结果窗口查看' ($text.Trim().Length -gt 0) "共 $($text.Length) 字符"
    if ($text) {
        $prev = if ($text.Length -gt 200) { $text.Substring(0, 200) + '...' } else { $text }
        $oneline = ($prev -replace "[\r\n]+", " | ")
        Write-Host ("  [OCR 文本预览] {0}" -f $oneline)
    }

    # ── 9. 「复制文本」→ 验证剪贴板 ──
    # 贴图是置顶窗口，先把它移到屏幕右侧、结果窗口移到左侧，避免遮挡按钮
    $SWP = 0x0001 -bor 0x0004 -bor 0x0010        # NOSIZE | NOZORDER | NOACTIVATE
    [void][Win32]::SetWindowPos($pin, [IntPtr]::Zero, 1150, 100, 0, 0, $SWP)
    [void][Win32]::SetWindowPos($ocrw, [IntPtr]::Zero, 60, 80, 0, 0, $SWP)
    Start-Sleep -Milliseconds 350
    $btnCopy = Find-ChildById $ocrw 100          # ID_COPY = 100
    Check '结果窗口含「复制文本」按钮' ($btnCopy -ne [IntPtr]::Zero)
    if ($btnCopy -ne [IntPtr]::Zero) {
        $cr = Get-Rect $btnCopy
        Click-At ([int](($cr.L + $cr.R) / 2)) ([int](($cr.T + $cr.B) / 2))
        # 等待并关闭「已复制到剪贴板」提示框（标题 PixShot OCR）
        $box = [IntPtr]::Zero
        $deadline = [Environment]::TickCount + 4000
        while ([Environment]::TickCount -lt $deadline) {
            $box = [Win32]::FindWindowW('#32770', 'PixShot OCR')
            if ($box -ne [IntPtr]::Zero) { break }
            Start-Sleep -Milliseconds 120
        }
        if ($box -ne [IntPtr]::Zero) { Press-Key 0x0D; Start-Sleep -Milliseconds 350 }
        Start-Sleep -Milliseconds 400
        $clipS = Get-ClipText
        Check '识别结果可复制到剪贴板' ($clipS.Trim() -eq $text.Trim()) `
            ("剪贴板 $($clipS.Length) 字符 / 窗口 $($text.Length) 字符")
    }

    # ── 10. 贴图菜单「复制图像」→ 验证剪贴板位图（截图复制功能）──
    # 按第 7 步命中的导航方案推断：↓x3 命中=无预选（复制图像=↓x1）；↓x2 命中=预选第1项（直接回车）
    $imgDowns = $downsUsed - 2
    $menu = [Win32]::FindWindowW('#32768', $null)
    if ($menu -ne [IntPtr]::Zero) { Press-Key 0x1B; Start-Sleep -Milliseconds 300 }
    $c = Get-Center $pin
    RightClick-At $c[0] $c[1]
    Start-Sleep -Milliseconds 450
    $menu = [Win32]::FindWindowW('#32768', $null)
    if ($menu -ne [IntPtr]::Zero) {
        for ($i = 0; $i -lt $imgDowns; $i++) { Press-Key 0x28 }
        Start-Sleep -Milliseconds 150
        Press-Key 0x0D
        Start-Sleep -Milliseconds 700
        $imgOk = Test-ClipDib
        Check '截图可复制到剪贴板（CF_DIB 位图）' $imgOk '剪贴板已含位图数据'
    } else {
        Check '截图可复制到剪贴板（CF_DIB 位图）' $false '右键菜单未打开'
    }

    # ── 11. 最终存档：全屏（贴图 + OCR 结果同屏）──
    Shot 0 0 $sw $sh (Join-Path $art 'final_state.png')
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })

