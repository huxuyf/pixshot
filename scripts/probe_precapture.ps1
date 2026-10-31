# 探针：验证「按下热键那一刻」的整屏预捕获是否在热键按键进入前台窗口之前完成，
# 并被截图流程消费。仅打印诊断日志中的相关行，不做像素断言。
param([string]$Exe = (Join-Path $PSScriptRoot '..\target\debug\PixShot.exe'))
$ErrorActionPreference = 'Stop'
$env:PIXSHOT_NO_UI_ALERTS = '1'
# 探针用 keybd_event 注入 F1；钩子默认忽略注入按键，这里显式放行以便验证预捕获。
$env:PIXSHOT_ALLOW_INJECTED = '1'

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class W {
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
}
"@

Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400
$log = Join-Path (Split-Path $Exe) 'pixshot_diag.log'
Remove-Item $log -ErrorAction SilentlyContinue

$proc = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Milliseconds 1500

# 打开一个「按键即关闭」的前台菜单（右键桌面/窗口空白处）
[W]::SetCursorPos(600, 400)
[W]::mouse_event(0x0008, 0, 0, 0, [UIntPtr]::Zero)  # RIGHTDOWN
[W]::mouse_event(0x0010, 0, 0, 0, [UIntPtr]::Zero)  # RIGHTUP
Start-Sleep -Milliseconds 500

# 注入 F1：热键按键（会关闭上面那个菜单）+ 触发截图流程
[W]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 60
[W]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 1200

Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 200

Write-Host '──── 诊断日志中的热键/预捕获/overlay 行 ────'
Get-Content $log -ErrorAction SilentlyContinue |
    Where-Object { $_ -match 'hotkey|预捕获|overlay|集成状态|WM_HOTKEY' }
