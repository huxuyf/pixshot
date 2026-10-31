# 验证「框选期间整屏冻结（无暗化）」的观感。
#
# 手法：铺一个铺满屏幕、**一失去前台就变绿**的窗口（正常 RED）。
#   - 正常情况屏幕应是 RED；
#   - 遮罩抢前台后该窗口会变绿 —— 但遮罩此刻显示的是"按下热键那一刻"的冻结画面，
#     所以框选期间屏幕看上去仍应是 RED（绿变只发生在遮罩背后）。
# 为能看到遮罩本身，探针会给程序设 PIXSHOT_OVERLAY_CAPTURABLE=1（跳过 WDA 排除），
# 这样普通屏幕截图能拍到遮罩，从而直接比对它显示的内容。
#
# 判据：框选期间屏幕 ≈ RED 且与"遮罩已消失后"的 GREEN 明显不同。
param([string]$Exe = (Join-Path $PSScriptRoot '..\target\release\PixShot.exe'))
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'
$env:PIXSHOT_OVERLAY_CAPTURABLE = '1'   # 仅测试：让遮罩可被屏幕截图拍到

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
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[void][W]::SetProcessDpiAwarenessContext([IntPtr](-4))
$sw = [W]::GetSystemMetrics(0); $sh = [W]::GetSystemMetrics(1)

function Avg([int]$x, [int]$y, [int]$w, [int]$h) {
    $b = New-Object System.Drawing.Bitmap($w, $h); $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h))); $g.Dispose()
    $r = 0; $gg = 0; $bl = 0; $n = 0
    for ($yy = 0; $yy -lt $h; $yy += 8) { for ($xx = 0; $xx -lt $w; $xx += 8) {
        $p = $b.GetPixel($xx, $yy); $r += $p.R; $gg += $p.G; $bl += $p.B; $n++ } }
    $b.Dispose(); return @([int]($r / $n), [int]($gg / $n), [int]($bl / $n))
}
function Pump([int]$ms) { $d = [Environment]::TickCount + $ms; while ([Environment]::TickCount -lt $d) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 15 } }
function WaitWin([string]$Class, [int]$TimeoutMs) {
    $d = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $d) { $h = [W]::FindClass($Class, [IntPtr]::Zero); if ($h -ne [IntPtr]::Zero) { return $h }; Pump 60 }
    return [IntPtr]::Zero
}

Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1200

# 铺满屏幕的"失焦即变绿"窗口
$form = New-Object System.Windows.Forms.Form
$form.FormBorderStyle = 'None'; $form.StartPosition = 'Manual'
$form.Bounds = New-Object System.Drawing.Rectangle(0, 0, $sw, $sh)
$form.TopMost = $true
$form.BackColor = [System.Drawing.Color]::FromArgb(220, 10, 10)   # RED
$form.Add_Deactivate({ $form.BackColor = [System.Drawing.Color]::FromArgb(10, 200, 10) })  # -> GREEN
$form.Show(); [void][W]::SetForegroundWindow($form.Handle)
Pump 500

$cx = [int]($sw / 2); $cy = [int]($sh / 2)
$cBefore = Avg ($cx - 150) ($cy - 100) 300 200
Write-Host ("框选前  屏幕中心均值 RGB=({0},{1},{2})  期望≈RED" -f $cBefore)

# 触发截图：遮罩出现并抢走前台（背后的窗口随即变绿，但遮罩显示的是冻结画面）
[W]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero); Pump 60
[W]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
$ov = WaitWin 'PixShotOverlay' 8000
if ($ov -eq [IntPtr]::Zero) { Write-Host 'INCONCLUSIVE: 遮罩未出现'; Stop-Process -Id $proc.Id -Force -EA SilentlyContinue; exit 2 }
Pump 800   # 留足时间让"背后的窗口"完成变绿重绘
$cDuring = Avg ($cx - 150) ($cy - 100) 300 200
Write-Host ("框选期间屏幕中心均值 RGB=({0},{1},{2})  期望仍≈RED（冻结）" -f $cDuring)

# Esc 取消，遮罩消失后再看一次（此时真实前台应已是 GREEN）
[W]::keybd_event(0x1B, 0, 0, [UIntPtr]::Zero); Pump 60
[W]::keybd_event(0x1B, 0, 2, [UIntPtr]::Zero); Pump 800
$cAfter = Avg ($cx - 150) ($cy - 100) 300 200
Stop-Process -Id $proc.Id -Force -EA SilentlyContinue
Write-Host ("遮罩消失后屏幕中心均值 RGB=({0},{1},{2})  期望≈GREEN（真实前台已变）" -f $cAfter)

$frozen = ($cDuring[0] -gt 150 -and $cDuring[1] -lt 100)
$liveChanged = ($cAfter[0] -lt 100 -and $cAfter[1] -gt 150)
if ($frozen -and $liveChanged) {
    Write-Host 'PASS: 框选期间屏幕是冻结画面（RED），取消后才露出真实的前台变化（GREEN）'
} elseif (-not $liveChanged) {
    Write-Host 'INCONCLUSIVE: 背后的窗口没有按预期变化，样本无效'
} else {
    Write-Host 'FAIL: 框选期间没有冻结（看到的是实时画面）'
}
