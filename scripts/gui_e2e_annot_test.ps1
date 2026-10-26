<#
PixShot 标注功能 GUI 端到端测试
================================
在真实桌面上跑通「框选 → 标注（矩形/步骤序号）→ 撤销 → 合成复制」：

  1. 框选截图，弹出工具条；
  2. 工具条新增的 3 个标注按钮（ID 6=矩形 / 7=箭头 / 8=步骤序号）存在；
  3. 点击「矩形」按钮后工具条不关闭（工具按钮不触发导出动作）；
  4. 在冻结画面上拖出矩形 → 屏幕出现红色描边（RGB≈E53935）；
  5. Ctrl+Z 撤销 → 红色描边消失（像素回到画矩形之前的值）；
  6. 重画矩形 → Ctrl+C 复制 → 剪贴板含 CF_DIB 且其中含红色像素
     （**合成图回归**：导出图与屏幕所见一致）；
  7. 再次截图，点击「步骤序号」按钮并在画面上单击 → 出现蓝色徽章（RGB≈1E6FFF）。

前置条件：已执行 cargo build（默认使用 target\debug\pixshot.exe）。

说明：本脚本设置 PIXSHOT_OVERLAY_CAPTURABLE=1，让冻结层可被普通屏幕截图捕获
（正常使用下冻结层是 WDA_EXCLUDEFROMCAPTURE 的），从而能用像素采样回归标注观感。

注意：脚本运行期间（约 25 秒）会接管鼠标和键盘，请勿操作桌面。

用法：
  pwsh -File scripts\gui_e2e_annot_test.ps1 [-Exe <pixshot.exe>] [-SelX 300 -SelY 300 -SelW 600 -SelH 400]

输出：
  - PASS/FAIL 检查清单；退出码 0 = 全部通过
  - e2e_artifacts\annot_after_rect.png   画矩形后的选区存档
  - e2e_artifacts\annot_final.png        全流程结束状态存档
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\debug\pixshot.exe'),
    [int]$SelX = 300,
    [int]$SelY = 300,
    [int]$SelW = 600,
    [int]$SelH = 400
)

$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'
# 冻结层可被屏幕截图捕获（正常使用下它是排除出捕获的），用于像素级回归
$env:PIXSHOT_OVERLAY_CAPTURABLE = '1'
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "找不到 $Exe，请先执行 cargo build" }
$art = Join-Path $PSScriptRoot '..\e2e_artifacts'
New-Item -ItemType Directory -Force -Path $art | Out-Null
try { Start-Transcript -Path (Join-Path $art 'e2e_annot_transcript.txt') -Force | Out-Null } catch { }
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Win32a {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindWindowClass(string c, IntPtr t);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
    [DllImport("user32.dll")] public static extern IntPtr PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool CloseClipboard();
    [DllImport("user32.dll")] public static extern bool EmptyClipboard();
    [DllImport("user32.dll")] public static extern bool IsClipboardFormatAvailable(uint fmt);
    [DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint fmt);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr hmem);
    [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr hmem);
}
"@

[void][Win32a]::SetProcessDpiAwarenessContext([IntPtr](-4))
$dpi = [Win32a]::GetDpiForSystem()
$sw = [Win32a]::GetSystemMetrics(0)
$sh = [Win32a]::GetSystemMetrics(1)
Write-Host ("环境：屏幕 {0}x{1} 物理像素，系统 DPI {2}（{3}%）" -f $sw, $sh, $dpi, [int]($dpi / 96 * 100))
if (($SelX + $SelW) -ge $sw -or ($SelY + $SelH) -ge $sh) { throw "选区超出屏幕范围" }

# ─── 辅助函数 ─────────────────────────────────────────────
function Wait-Window([string]$Class, [int]$TimeoutMs = 5000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        $h = [Win32a]::FindWindowClass($Class, [IntPtr]::Zero)
        if ($h -ne [IntPtr]::Zero) { return $h }
        if ([Environment]::TickCount -ge $deadline) { return [IntPtr]::Zero }
        Start-Sleep -Milliseconds 80
    }
}
function Get-Rect2([IntPtr]$H) { $r = New-Object Win32a+RECT; [void][Win32a]::GetWindowRect($H, [ref]$r); $r }
function Get-Center2([IntPtr]$H) { $r = Get-Rect2 $H; @([int](($r.L + $r.R) / 2), [int](($r.T + $r.B) / 2)) }
function Mouse([uint32]$F) { [Win32a]::mouse_event($F, 0, 0, 0, [UIntPtr]::Zero) }
function Click-At([int]$X, [int]$Y) {
    [void][Win32a]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 90
    Mouse 0x02; Start-Sleep -Milliseconds 50; Mouse 0x04; Start-Sleep -Milliseconds 140
}
function Drag-Mouse([int]$X1, [int]$Y1, [int]$X2, [int]$Y2) {
    [void][Win32a]::SetCursorPos($X1, $Y1); Start-Sleep -Milliseconds 160
    Mouse 0x02
    $n = 14
    for ($i = 1; $i -le $n; $i++) {
        [void][Win32a]::SetCursorPos(($X1 + [int](($X2 - $X1) * $i / $n)), ($Y1 + [int](($Y2 - $Y1) * $i / $n)))
        Start-Sleep -Milliseconds 30
    }
    Start-Sleep -Milliseconds 90
    Mouse 0x04; Start-Sleep -Milliseconds 140
}
function Press-Key([byte]$Vk) {
    [Win32a]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 110
}
function Press-Ctrl([byte]$Vk) {
    [Win32a]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 50
    [Win32a]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 180
}
function Wait-WindowGone([string]$Class, [int]$TimeoutMs = 4000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $deadline) {
        if ([Win32a]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero) { return $true }
        Start-Sleep -Milliseconds 80
    }
    ([Win32a]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero)
}
function Find-ChildById([IntPtr]$Parent, [int]$Id) {
    $c = [Win32a]::GetWindow($Parent, 5)
    while ($c -ne [IntPtr]::Zero) {
        if ([Win32a]::GetDlgCtrlID($c) -eq $Id) { return $c }
        $c = [Win32a]::GetWindow($c, 2)
    }
    [IntPtr]::Zero
}
function Invoke-Capture {
    Press-Key 0x70
    Start-Sleep -Milliseconds 600
    $o = Wait-Window 'PixShotOverlay' 12000
    if ($o -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    Start-Sleep -Milliseconds 500
    Drag-Mouse $SelX $SelY ($SelX + $SelW) ($SelY + $SelH)
    return (Wait-Window 'PixShotToolbar' 6000)
}
function Get-Pixel([int]$X, [int]$Y) {
    $bmp = New-Object System.Drawing.Bitmap(1, 1)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size(1, 1)))
    $g.Dispose()
    $c = $bmp.GetPixel(0, 0); $bmp.Dispose(); $c
}
function Test-Redish($C) { ($C.R -gt 150) -and ($C.G -lt 120) -and ($C.B -lt 120) }
function Test-Bluish($C) { ($C.B -gt 150) -and ($C.R -lt 120) -and ($C.G -lt 170) }
function Test-ClipDib {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            try { return [Win32a]::IsClipboardFormatAvailable(8) }
            finally { [void][Win32a]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
}
# 剪贴板 CF_DIB 中是否存在"矩形红"像素（R>200,G<90,B<90）—— 验证合成图确实含标注。
function Test-ClipDibHasRed {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            try {
                if (-not [Win32a]::IsClipboardFormatAvailable(8)) { return $false }
                $h = [Win32a]::GetClipboardData(8)
                if ($h -eq [IntPtr]::Zero) { return $false }
                $p = [Win32a]::GlobalLock($h)
                if ($p -eq [IntPtr]::Zero) { return $false }
                try {
                    $biSize = [Runtime.InteropServices.Marshal]::ReadInt32($p, 0)
                    $w = [Runtime.InteropServices.Marshal]::ReadInt32($p, 4)
                    $hh = [Runtime.InteropServices.Marshal]::ReadInt32($p, 8)
                    $bits = [Runtime.InteropServices.Marshal]::ReadInt16($p, 14)
                    if ($bits -ne 32) { Write-Host ("  [调试] CF_DIB 位深={0}（预期 32）" -f $bits); return $false }
                    $off = $biSize; if ($off -lt 40) { $off = 40 }
                    $aw = [Math]::Abs($w); $ah = [Math]::Abs($hh)
                    $len = $aw * $ah * 4
                    # 一次拷出像素块，再在内存数组里扫描（逐字节 Marshal 读取过慢）
                    $buf = New-Object byte[] $len
                    [Runtime.InteropServices.Marshal]::Copy([IntPtr]::Add($p, $off), $buf, 0, $len)
                    for ($k = 0; $k -lt $len; $k += 4) {
                        # BGRA：buf[k]=B, [k+1]=G, [k+2]=R
                        if ($buf[$k + 2] -gt 200 -and $buf[$k + 1] -lt 90 -and $buf[$k] -lt 90) { return $true }
                    }
                    return $false
                } finally { [void][Win32a]::GlobalUnlock($h) }
            } finally { [void][Win32a]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
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
    $isOk = $false
    if ($Ok -is [bool]) { $isOk = $Ok }
    else {
        $t = if ($null -eq $Ok) { 'null' } else { $Ok.GetType().Name }
        Write-Host ("  [断言告警]「{0}」的判定值不是布尔（实际 {1}），已按失败处理" -f $Name, $t) -ForegroundColor Yellow
    }
    if ($isOk) { $script:Pass++ } else { $script:Fail++ }
    Write-Host ("  [{0}] {1}{2}" -f $(if ($isOk) { 'PASS' } else { 'FAIL' }), $Name, $(if ($Detail) { " -- $Detail" } else { '' }))
}

# 矩形四边中点附近（避开选区红框，距离边界 ≥ 40px）
$rectP1 = @(($SelX + 60), ($SelY + 60))
$rectP2 = @(($SelX + 260), ($SelY + 200))
$rectEdgeProbe = @(($SelX + 60), ($SelY + 130))   # 矩形左边中点
# 步骤序号落点与探针（探针偏离圆心，避开圆心处的白色数字）
$stepClick = @(($SelX + 380), ($SelY + 120))
$stepProbe = @(($SelX + 380 - 7), ($SelY + 120))

Write-Host ''
Write-Host '==== PixShot 标注端到端测试：框选 -> 矩形 -> 撤销 -> 步骤序号 -> 合成复制 ===='
Write-Host '  （脚本将接管鼠标键盘约 25 秒，请勿操作桌面）'
Write-Host ''

$proc = $null
try {
    $proc = Start-Process -FilePath $Exe -PassThru
    $mainH = Wait-Window 'PixShotMain' 10000
    Start-Sleep -Milliseconds 800
    if ($mainH -eq [IntPtr]::Zero) { throw '主窗口（托盘宿主）未创建' }

    $tb = Invoke-Capture
    for ($try = 1; $try -le 3 -and $tb -eq [IntPtr]::Zero; $try++) {
        Write-Host ("  [重试] 第 {0} 次框选未产生工具条，重新截图" -f $try)
        $stray = [Win32a]::FindWindowClass('PixShotOverlay', [IntPtr]::Zero)
        if ($stray -ne [IntPtr]::Zero) {
            [void][Win32a]::PostMessageW($stray, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)
            Start-Sleep -Milliseconds 400
        }
        $tb = Invoke-Capture
    }
    Check '框选完成 -> 工具条弹出' ($tb -ne [IntPtr]::Zero)
    if ($tb -eq [IntPtr]::Zero) { throw '工具栏未出现（框选可能失败）' }

    # ── 1. 新增的 3 个标注按钮存在（ID 6/7/8）──
    $btnRect = Find-ChildById $tb 6
    $btnArrow = Find-ChildById $tb 7
    $btnStep = Find-ChildById $tb 8
    Check '工具条含「矩形」按钮（ID 6）' ($btnRect -ne [IntPtr]::Zero)
    Check '工具条含「箭头」按钮（ID 7）' ($btnArrow -ne [IntPtr]::Zero)
    Check '工具条含「步骤序号」按钮（ID 8）' ($btnStep -ne [IntPtr]::Zero)

    # ── 2. 画矩形：点工具按钮 → 在冻结画面拖出矩形 ──
    $probeBefore = Get-Pixel $rectEdgeProbe[0] $rectEdgeProbe[1]
    Write-Host ("  [采样] 画矩形前 探针像素 RGB=({0},{1},{2})" -f $probeBefore.R, $probeBefore.G, $probeBefore.B)

    $rc = Get-Center2 $btnRect
    Click-At $rc[0] $rc[1]
    Start-Sleep -Milliseconds 250
    Check '点击「矩形」后工具条保持打开（工具按钮不触发导出）' ([Win32a]::IsWindowVisible($tb))

    Drag-Mouse $rectP1[0] $rectP1[1] $rectP2[0] $rectP2[1]
    Start-Sleep -Milliseconds 300
    $probeAfter = Get-Pixel $rectEdgeProbe[0] $rectEdgeProbe[1]
    Write-Host ("  [采样] 画矩形后 探针像素 RGB=({0},{1},{2})" -f $probeAfter.R, $probeAfter.G, $probeAfter.B)
    Check '拖拽后画面出现矩形红色描边（RGB≈E53935）' (Test-Redish $probeAfter) ("像素=({0},{1},{2})" -f $probeAfter.R, $probeAfter.G, $probeAfter.B)
    Shot $SelX $SelY $SelW $SelH (Join-Path $art 'annot_after_rect.png')

    # ── 3. Ctrl+Z 撤销 → 红色描边消失（像素回到画之前的值）──
    Press-Ctrl 0x5A
    Start-Sleep -Milliseconds 350
    $probeUndo = Get-Pixel $rectEdgeProbe[0] $rectEdgeProbe[1]
    Write-Host ("  [采样] 撤销后 探针像素 RGB=({0},{1},{2})" -f $probeUndo.R, $probeUndo.G, $probeUndo.B)
    $restored = (-not (Test-Redish $probeUndo)) -and ($probeUndo.R -eq $probeBefore.R) -and ($probeUndo.G -eq $probeBefore.G) -and ($probeUndo.B -eq $probeBefore.B)
    Check 'Ctrl+Z 撤销矩形后画面还原' $restored ("撤销后=({0},{1},{2})，原始=({3},{4},{5})" -f $probeUndo.R, $probeUndo.G, $probeUndo.B, $probeBefore.R, $probeBefore.G, $probeBefore.B)

    # ── 4. 合成回归：重画矩形 → Ctrl+C → 剪贴板合成图必须含红色标注 ──
    # 先清空剪贴板，避免"通过"来自上一步/其它程序的残留位图。
    [void][Win32a]::OpenClipboard([IntPtr]::Zero)
    [void][Win32a]::EmptyClipboard()
    [void][Win32a]::CloseClipboard()
    # 工具仍为「矩形」，直接再拖一次
    Drag-Mouse $rectP1[0] $rectP1[1] $rectP2[0] $rectP2[1]
    Start-Sleep -Milliseconds 300
    Check '重画矩形（供合成回归）' (Test-Redish (Get-Pixel $rectEdgeProbe[0] $rectEdgeProbe[1]))
    Write-Host '  [操作] 按 Ctrl+C（复制并关闭，导出前合成标注）'
    Press-Ctrl 0x43
    Start-Sleep -Milliseconds 900
    Check 'Ctrl+C 复制到剪贴板（CF_DIB）' (Test-ClipDib)
    Check '合成图含矩形标注（CF_DIB 内含红色像素）' (Test-ClipDibHasRed) '导出图与屏幕所见一致'
    Check 'Ctrl+C 后工具条自动关闭' (Wait-WindowGone 'PixShotToolbar' 4000)

    # ── 5. 步骤序号：重新截图 → 点工具 → 单击放置 → 出现蓝色徽章 ──
    $tb2 = Invoke-Capture
    for ($try = 1; $try -le 3 -and $tb2 -eq [IntPtr]::Zero; $try++) { $tb2 = Invoke-Capture }
    Check '再次框选 -> 工具条弹出' ($tb2 -ne [IntPtr]::Zero)
    if ($tb2 -eq [IntPtr]::Zero) { throw '第二次框选未出现工具条' }
    $btnStep2 = Find-ChildById $tb2 8
    if ($btnStep2 -eq [IntPtr]::Zero) { throw '未找到「步骤序号」按钮' }
    $sc = Get-Center2 $btnStep2
    Click-At $sc[0] $sc[1]
    Start-Sleep -Milliseconds 250
    Click-At $stepClick[0] $stepClick[1]
    Start-Sleep -Milliseconds 350
    $stepAfter = Get-Pixel $stepProbe[0] $stepProbe[1]
    Write-Host ("  [采样] 放置序号后 探针像素 RGB=({0},{1},{2})" -f $stepAfter.R, $stepAfter.G, $stepAfter.B)
    Check '单击放置步骤序号（出现蓝色徽章 RGB≈1E6FFF）' (Test-Bluish $stepAfter) ("像素=({0},{1},{2})" -f $stepAfter.R, $stepAfter.G, $stepAfter.B)

    Shot 0 0 $sw $sh (Join-Path $art 'annot_final.png')
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
