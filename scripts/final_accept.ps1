# 最终验收：① 托盘真实注册进外壳（含图标位置反查）② 真实 F1 按键触发截图
$out = "C:\Users\user\Desktop\pixpin\pixshot\final_accept.txt"
function W($s) { Add-Content -Path $out -Value $s -Encoding UTF8 }
Set-Content -Path $out -Value "=== 最终验收 $(Get-Date -Format 'HH:mm:ss') ===" -Encoding UTF8

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;

[StructLayout(LayoutKind.Sequential)]
public struct NII { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }

public class A {
    [DllImport("shell32.dll", SetLastError = true)]
    public static extern int Shell_NotifyIconGetRect(ref NII id, out RECT r);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@

function Find-W([int]$TargetPid, [string]$Class) {
    $script:tp = $TargetPid; $script:hits = New-Object System.Collections.Generic.List[object]
    $cb = [A+EnumProc]{ param($h, $l)
        $wp = 0; [void][A]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $script:tp) {
            $sb = New-Object System.Text.StringBuilder 64
            [void][A]::GetClassNameW($h, $sb, 64)
            $script:hits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true }
    [void][A]::EnumWindows($cb, [IntPtr]::Zero)
    foreach ($w in $script:hits) { if ($w.Class -eq $Class) { return $w.Hwnd } }
    [IntPtr]::Zero }

$exe = "C:\Users\user\Desktop\pixpin\pixshot\target\release\PixShot.exe"
$logPath = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot_diag.log"

Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700
Remove-Item $logPath -ErrorAction SilentlyContinue
$env:PIXSHOT_NO_UI_ALERTS = '1'

W "── ① 启动（工作区内，标签已修复）──"
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 3
W "  进程存活: $(-not $p.HasExited)  PID=$($p.Id)"
$main = Find-W $p.Id 'PixShotMain'
W "  主窗口: 0x$($main.ToString('X'))"
if (Test-Path $logPath) {
    foreach ($l in (Get-Content $logPath)) { if ($l -match 'NIM_ADD 第 1|NIM_SETVERSION|集成状态|RegisterHotKey') { W "  $l" } }
}

W "── ② 反查外壳是否真正持有该托盘图标（Shell_NotifyIconGetRect）──"
$id = New-Object NII
$id.cbSize = [Runtime.InteropServices.Marshal]::SizeOf([type][NII])
$id.hWnd = $main
$id.uID = 1
$r = New-Object RECT
$hr = [A]::Shell_NotifyIconGetRect([ref]$id, [ref]$r)
$ge = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
if ($hr -eq 0) {
    W "  ✓ 外壳已持有该图标：屏幕位置 ($($r.L),$($r.T))-($($r.R),$($r.B))  ← 图标确实在任务栏托盘区"
} else {
    W "  ✗ 外壳未返回图标位置：hr=0x$('{0:X8}' -f $hr) err=$ge"
}

W "── ③ 真实 F1 按键注入（验证全局热键）──"
$pt = New-Object A+POINT
[void][A]::GetCursorPos([ref]$pt)
$cursorOk = [A]::SetCursorPos(500, 500)
Start-Sleep -Milliseconds 200
$p2 = New-Object A+POINT
[void][A]::GetCursorPos([ref]$p2)
W "  输入注入可用性: SetCursorPos=$cursorOk (光标 $($pt.X),$($pt.Y) -> $($p2.X),$($p2.Y))"
# 注入 F1 按下/抬起
[A]::keybd_event(0x70, 0, 0, [UIntPtr]::Zero)
Start-Sleep -Milliseconds 60
[A]::keybd_event(0x70, 0, 2, [UIntPtr]::Zero)
# 用 EnumWindows 轮询遮罩（本环境 FindWindowW 对部分类名不可靠）
$ov = [IntPtr]::Zero
$t0 = [Environment]::TickCount
while ([Environment]::TickCount -lt ($t0 + 6000)) {
    $ov = Find-W $p.Id 'PixShotOverlay'
    if ($ov -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 150
}
$f1ms = [Environment]::TickCount - $t0
W "  注入 F1 后遮罩出现: $($ov -ne [IntPtr]::Zero) （耗时 $f1ms ms）  ← 期望 True"
if (Test-Path $logPath) {
    foreach ($l in (Get-Content $logPath)) { if ($l -match 'WM_HOTKEY') { W "  日志: $l" } }
}
if ([A]::IsWindow($ov)) {
    # 取消遮罩（WM_KEYDOWN Esc 消息）
    Add-Type -TypeDefinition "using System;using System.Runtime.InteropServices;public class S{[DllImport(`"user32.dll`")]public static extern IntPtr SendMessageW(IntPtr h,uint m,IntPtr w,IntPtr l);}"
    [void][S]::SendMessageW($ov, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 400
}
Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
W "=== 验收结束（测试进程已结束）==="
