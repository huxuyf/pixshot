# 真实菜单验证：截屏成品里到底有没有「前台弹出的菜单」。
#
# 流程：显示桌面 -> 右键弹出桌面菜单 -> 记录菜单区域(R_before)
#       -> 触发截图(F1) -> 框选整屏 -> Ctrl+C -> 剪贴板取成品 C
#       -> 再记录同一区域(R_after，此时菜单已关闭)
# 判据：C 与 R_after 在菜单区域的差异。差异大 => 成品里保留了菜单；接近 0 => 菜单丢失。
#
# 两种热键链路上都跑：
#   - FORCE_HOOK=1：热键按键会投递给前台窗口（用户机器上 F1 被占用时的兜底路径）
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\PixShot.exe'),
    [int]$Mx = 520, [int]$My = 260,
    [int]$Rw = 320, [int]$Rh = 470
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class N {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindClass(string c, IntPtr t);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[void][N]::SetProcessDpiAwarenessContext([IntPtr](-4))
$sw = [N]::GetSystemMetrics(0); $sh = [N]::GetSystemMetrics(1)
$Rw = [Math]::Min($Rw, $sw - $Mx - 2); $Rh = [Math]::Min($Rh, $sh - $My - 2)

function Shot([int]$x, [int]$y, [int]$w, [int]$h) {
    $b = New-Object System.Drawing.Bitmap($w, $h)
    $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen($x, $y, 0, 0, (New-Object System.Drawing.Size($w, $h)))
    $g.Dispose(); return $b
}
function MeanDiff($a, $b, [int]$step = 4) {
    $sum = 0.0; $n = 0
    for ($y = 0; $y -lt $a.Height; $y += $step) {
        for ($x = 0; $x -lt $a.Width; $x += $step) {
            $pa = $a.GetPixel($x, $y); $pb = $b.GetPixel($x, $y)
            $sum += ([Math]::Abs($pa.R - $pb.R) + [Math]::Abs($pa.G - $pb.G) + [Math]::Abs($pa.B - $pb.B)) / 3.0
            $n++
        }
    }
    if ($n -eq 0) { return 0 }; return $sum / $n
}
function Pump([int]$ms) { $d = [Environment]::TickCount + $ms; while ([Environment]::TickCount -lt $d) { [System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 15 } }
function WaitWin([string]$Class, [int]$TimeoutMs) {
    $d = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $d) { $h = [N]::FindClass($Class, [IntPtr]::Zero); if ($h -ne [IntPtr]::Zero) { return $h }; Pump 60 }
    return [IntPtr]::Zero
}
function Drag([int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    [void][N]::SetCursorPos($x0, $y0); Start-Sleep -Milliseconds 120
    [N]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
    for ($i = 1; $i -le 8; $i++) { [void][N]::SetCursorPos([int]($x0 + ($x1 - $x0) * $i / 8), [int]($y0 + ($y1 - $y0) * $i / 8)); Start-Sleep -Milliseconds 25 }
    [N]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 150
}

Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1200

# 显示桌面：Win+D（否则右键可能落在某个窗口上，菜单形态不稳定）
[N]::keybd_event(0x5B, 0, 0, [UIntPtr]::Zero)
[N]::keybd_event(0x44, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
[N]::keybd_event(0x44, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
[N]::keybd_event(0x5B, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 900

# 右键弹出桌面菜单
[void][N]::SetCursorPos($Mx, $My); Start-Sleep -Milliseconds 200
[N]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)
[N]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 700
$before = Shot $Mx $My $Rw $Rh

# 触发截图 + 框选整屏 + Ctrl+C
[N]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60
[N]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
$ov = WaitWin 'PixShotOverlay' 8000
if ($ov -eq [IntPtr]::Zero) { Write-Host 'INCONCLUSIVE: 遮罩未出现'; Stop-Process -Id $proc.Id -Force -EA SilentlyContinue; exit 2 }
Pump 400
Drag 20 20 ($sw - 20) ($sh - 20)
if ((WaitWin 'PixShotToolbar' 5000) -eq [IntPtr]::Zero) { Write-Host 'INCONCLUSIVE: 工具条未出现'; Stop-Process -Id $proc.Id -Force -EA SilentlyContinue; exit 3 }
Pump 300
[N]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); Pump 50
[N]::keybd_event(0x43, 0, 0, [UIntPtr]::Zero); Pump 60
[N]::keybd_event(0x43, 0, 2, [UIntPtr]::Zero); Pump 50
[N]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero); Pump 500

# 菜单此刻应已关闭：记录同一区域，并从成品里取同一区域
$after = Shot $Mx $My $Rw $Rh
$img = [System.Windows.Forms.Clipboard]::GetImage()
Stop-Process -Id $proc.Id -Force -EA SilentlyContinue
if ($null -eq $img) { Write-Host 'INCONCLUSIVE: 剪贴板无图像'; exit 4 }

# 成品为整屏裁剪，原点在 (20,20)；裁出与 R_* 相同的区域
$cut = New-Object System.Drawing.Bitmap($Rw, $Rh)
$g = [System.Drawing.Graphics]::FromImage($cut)
$g.DrawImage($img, (New-Object System.Drawing.Rectangle(0, 0, $Rw, $Rh)), (New-Object System.Drawing.Rectangle(($Mx - 20), ($My - 20), $Rw, $Rh)), [System.Drawing.GraphicsUnit]::Pixel)
$g.Dispose()

$dBefore = MeanDiff $cut $before
$dAfter = MeanDiff $cut $after
$dBefAft = MeanDiff $before $after
$art = Join-Path $PSScriptRoot '..\e2e_artifacts'
New-Item -ItemType Directory -Force -Path $art | Out-Null
$tag = if ($env:PIXSHOT_FORCE_HOOK) { 'hook' } else { 'hotkey' }
$before.Save((Join-Path $art "menu_$tag`_before.png"), [System.Drawing.Imaging.ImageFormat]::Png)
$after.Save((Join-Path $art "menu_$tag`_after.png"), [System.Drawing.Imaging.ImageFormat]::Png)
$cut.Save((Join-Path $art "menu_$tag`_captured.png"), [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host ("菜单区域 {0}x{1} @ {2},{3}  链路={4}" -f $Rw, $Rh, $Mx, $My, $tag)
Write-Host ("成品 vs R_before = {0:N2} ；成品 vs R_after = {1:N2} ；R_before vs R_after = {2:N2}" -f $dBefore, $dAfter, $dBefAft)
if ($dBefAft -le 6) {
    Write-Host 'RESULT: 实时画面两帧一致（菜单未在这段时间关闭）-> 本样本无法区分，忽略'
} elseif ($dBefore -le $dAfter) {
    Write-Host 'RESULT: 成品保留了菜单（更接近"菜单打开"的画面）'
} else {
    Write-Host 'RESULT: 成品丢失了菜单（更接近"菜单已关闭"的画面）'
}
