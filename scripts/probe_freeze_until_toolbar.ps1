# 验证「冻结保持到点工具条按钮」：
#   - 松手确认选区、工具条出现后，屏幕**仍应是冻结画面**（旧行为会在松手瞬间解冻）；
#   - 点工具条上的按钮后（这里点"关闭"按钮，id=5），屏幕才恢复实时状态。
#   - 顺带确认工具条在 z 序上位于冻结层**之上**（工具条自身带 WDA_EXCLUDEFROMCAPTURE，
#     屏幕截图拍不到它，所以只能用 z 序判定，不能用像素）。
#
# 判据：整屏粗采样均值 —— 冻结画面≈RED、解冻后≈GREEN（背后铺了一块"失焦即变绿"的整屏窗口）。
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\pixshot.exe'),
    [int]$BtnCloseId = 5
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'
$env:PIXSHOT_OVERLAY_CAPTURABLE = '1'   # 仅测试：让冻结层可被屏幕截图拍到

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindClass(string c, IntPtr t);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr GetTopWindow(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, System.Text.StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[void][W]::SetProcessDpiAwarenessContext([IntPtr](-4))
$sw = [W]::GetSystemMetrics(0); $sh = [W]::GetSystemMetrics(1)

# 整屏粗采样均值（避开单个窗口的局部影响）
function ScreenAvg {
    $b = New-Object System.Drawing.Bitmap($sw, $sh); $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen(0, 0, 0, 0, (New-Object System.Drawing.Size($sw, $sh))); $g.Dispose()
    $r = 0.0; $gg = 0.0; $bl = 0.0; $n = 0
    for ($y = 0; $y -lt $sh; $y += 24) { for ($x = 0; $x -lt $sw; $x += 24) {
        $p = $b.GetPixel($x, $y); $r += $p.R; $gg += $p.G; $bl += $p.B; $n++ } }
    $b.Dispose(); return @([int]($r / $n), [int]($gg / $n), [int]($bl / $n))
}
function Pump([int]$ms) { $d = [Environment]::TickCount + $ms; while ([Environment]::TickCount -lt $d) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 15 } }
function WaitWin([string]$Class, [int]$TimeoutMs) {
    $d = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $d) { $h = [W]::FindClass($Class, [IntPtr]::Zero); if ($h -ne [IntPtr]::Zero) { return $h }; Pump 60 }
    return [IntPtr]::Zero
}
function ChildById([IntPtr]$Parent, [int]$id) {
    $c = [W]::GetWindow($Parent, 5)   # GW_CHILD
    while ($c -ne [IntPtr]::Zero) { if ([W]::GetDlgCtrlID($c) -eq $id) { return $c }; $c = [W]::GetWindow($c, 2) }
    [IntPtr]::Zero
}
# 自上而下第 idx 个顶层窗口里，类名以 prefix 开头的窗口序号；找不到返回 9999
function TopIndex([string]$prefix) {
    $h = [W]::GetTopWindow([IntPtr]::Zero); $i = 0
    while ($h -ne [IntPtr]::Zero -and $i -lt 200) {
        $sb = New-Object System.Text.StringBuilder 256
        [void][W]::GetClassNameW($h, $sb, 256)
        if ($sb.ToString().StartsWith($prefix)) { return $i }
        $h = [W]::GetWindow($h, 2); $i++
    }
    return 9999
}

Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1200

$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'; $form.StartPosition = 'Manual'
$form.Bounds = New-Object System.Drawing.Rectangle(0, 0, $sw, $sh); $form.TopMost = $true
$form.BackColor = [System.Drawing.Color]::FromArgb(220, 10, 10)                                   # RED
$form.Add_Deactivate({ $form.BackColor = [System.Drawing.Color]::FromArgb(10, 200, 10) })          # -> GREEN
$form.Show(); [void][W]::SetForegroundWindow($form.Handle)
Pump 500
$cBefore = ScreenAvg
Write-Host ("框选前            整屏均值 RGB=({0},{1},{2})  期望≈RED" -f $cBefore[0], $cBefore[1], $cBefore[2])

# F1 -> 框选 -> 松手确认（工具条出现）
[W]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero); Pump 60
[W]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
if ((WaitWin 'PixShotOverlay' 8000) -eq [IntPtr]::Zero) { Write-Host 'INCONCLUSIVE: 遮罩未出现'; Stop-Process -Id $proc.Id -Force -EA SilentlyContinue; exit 2 }
Pump 400
[void][W]::SetCursorPos(120, 120); Start-Sleep -Milliseconds 120
[W]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
for ($i = 1; $i -le 6; $i++) { [void][W]::SetCursorPos(120 + $i * 130, 120 + $i * 90); Start-Sleep -Milliseconds 30 }
[W]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)     # 松手 = 确认
$tb = WaitWin 'PixShotToolbar' 6000
if ($tb -eq [IntPtr]::Zero) { Write-Host 'INCONCLUSIVE: 工具条未出现'; Stop-Process -Id $proc.Id -Force -EA SilentlyContinue; exit 3 }
Pump 900

$cToolbar = ScreenAvg
$iTb = TopIndex 'PixShotToolbar'; $iOv = TopIndex 'PixShotOverlay'
Write-Host ("松手后(工具条在)  整屏均值 RGB=({0},{1},{2})  期望仍≈RED（保持冻结）" -f $cToolbar[0], $cToolbar[1], $cToolbar[2])
Write-Host ("z 序：工具条第 {0} 个 / 冻结层第 {1} 个（越小越靠上，期望 工具条 < 冻结层）" -f $iTb, $iOv)

# 点工具条上的按钮（关闭，id=5）——会话结束，屏幕应恢复
$btn = ChildById $tb $BtnCloseId
if ($btn -ne [IntPtr]::Zero) { [void][W]::SendMessageW($btn, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero) } else { [W]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero) }
Pump 1000
$gone = ([W]::FindClass('PixShotOverlay', [IntPtr]::Zero) -eq [IntPtr]::Zero)
$cAfter = ScreenAvg
Stop-Process -Id $proc.Id -Force -EA SilentlyContinue
Write-Host ("点按钮后          整屏均值 RGB=({0},{1},{2})  期望≈GREEN；冻结层已销毁={3}" -f $cAfter[0], $cAfter[1], $cAfter[2], $gone)

$frozenDuring = ($cToolbar[0] -gt 150 -and $cToolbar[1] -lt 100)
$toolbarAbove = ($iTb -lt $iOv)
$reverted = ($gone -and $cAfter[0] -lt 100 -and $cAfter[1] -gt 150)
if ($frozenDuring -and $toolbarAbove -and $reverted) {
    Write-Host 'PASS: 松手后仍冻结；工具条在冻结层之上；点按钮后屏幕才恢复'
} else {
    Write-Host ("FAIL: frozenDuring={0} toolbarAbove={1} reverted={2}" -f $frozenDuring, $toolbarAbove, $reverted)
}
