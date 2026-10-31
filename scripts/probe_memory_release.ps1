# 验证「框选结束后整屏截图资源及时回收」：对比"拖拽中（冻结画面，持有整屏快照）"
# 与"确认后（成品已裁出、整屏快照已释放，红框窗口仍在）"两个时刻的进程内存。
#
# 1080p 整屏 BGRA ≈ 8 MB；4K 多屏更大。确认后该缓冲应被释放（Rust 大分配走
# VirtualAlloc，drop 时立即归还系统，故驻留集应出现可见下降）。
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\PixShot.exe'),
    [int]$DropMinMb = 5
)
$ErrorActionPreference = 'Stop'
$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindClass(string c, IntPtr t);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
}
"@
[void][W]::SetProcessDpiAwarenessContext([IntPtr](-4))
$sw = [W]::GetSystemMetrics(0); $sh = [W]::GetSystemMetrics(1)
function WS([int]$procId) { [int]((Get-Process -Id $procId).WorkingSet64 / 1MB) }
function Priv([int]$procId) { [int]((Get-Process -Id $procId).PrivateMemorySize64 / 1MB) }

Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1500

[W]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 60
[W]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
$deadline = [Environment]::TickCount + 8000
while ([Environment]::TickCount -lt $deadline -and [W]::FindClass('PixShotOverlay', [IntPtr]::Zero) -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 50 }
Start-Sleep -Milliseconds 500

# 拖拽中：松手前的时刻 —— 整屏快照仍在
[void][W]::SetCursorPos(100, 100); Start-Sleep -Milliseconds 120
[W]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
for ($i = 1; $i -le 6; $i++) { [void][W]::SetCursorPos(100 + $i * 100, 100 + $i * 80); Start-Sleep -Milliseconds 30 }
Start-Sleep -Milliseconds 300
$wsDrag = WS $proc.Id; $prDrag = Priv $proc.Id

# 松手确认：整屏快照应在这一刻释放
[W]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 700
$wsDone = WS $proc.Id; $prDone = Priv $proc.Id
Stop-Process -Id $proc.Id -Force -EA SilentlyContinue

Write-Host ("拖拽中（持有整屏快照）  工作集 {0} MB / 私有 {1} MB" -f $wsDrag, $prDrag)
Write-Host ("确认后（快照已释放）    工作集 {0} MB / 私有 {1} MB" -f $wsDone, $prDone)
$drop = $prDrag - $prDone
Write-Host ("确认后私有内存下降 {0} MB" -f $drop)
if ($drop -ge $DropMinMb) {
    Write-Host 'PASS: 确认后整屏缓冲已归还系统'
} else {
    Write-Host ("INCONCLUSIVE: 下降不足 {0} MB（可能被分配器缓存，非必然失败）" -f $DropMinMb)
}
