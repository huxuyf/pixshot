# 复验：#1 自绘托盘菜单（出现 + 点击退出 + Esc 关闭）  #3 拖动框选区域
$out = "C:\Users\user\Desktop\pixpin\pixshot\fix5_verify.txt"
function W($s) { Add-Content -Path $out -Value $s -Encoding UTF8 }
Set-Content -Path $out -Value "=== 复验 $(Get-Date -Format 'HH:mm:ss') ===" -Encoding UTF8

Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class V {
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern int GetClassLongW(IntPtr h, int i);
    public delegate bool EnumChildProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr p, EnumChildProc cb, IntPtr l);
}
"@
function Find-W([int]$TargetPid, [string]$Class) {
    $script:tp = $TargetPid; $script:hits = New-Object System.Collections.Generic.List[object]
    $cb = [V+EnumProc]{ param($h, $l)
        $wp = 0; [void][V]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $script:tp) {
            $sb = New-Object System.Text.StringBuilder 64
            [void][V]::GetClassNameW($h, $sb, 64)
            $script:hits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true }
    [void][V]::EnumWindows($cb, [IntPtr]::Zero)
    foreach ($w in $script:hits) { if ($w.Class -eq $Class) { return $w.Hwnd } }
    [IntPtr]::Zero }
function Wait-W([int]$TargetPid, [string]$Class, [int]$Ms = 8000) {
    $t0 = [Environment]::TickCount
    while ([Environment]::TickCount -lt ($t0 + $Ms)) {
        $h = Find-W $TargetPid $Class
        if ($h -ne [IntPtr]::Zero) { return $h }
        [void][System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 80 }
    [IntPtr]::Zero }
function LP([int]$x, [int]$y) { [IntPtr](($x -band 0xFFFF) -bor (($y -band 0xFFFF) -shl 16)) }
function Children([IntPtr]$h) {
    $script:lbls = New-Object System.Collections.Generic.List[string]
    $cb = [V+EnumChildProc]{ param($c, $l)
        $sb = New-Object System.Text.StringBuilder 128
        [void][V]::GetWindowTextW($c, $sb, 128)
        if ($sb.Length -gt 0) { $script:lbls.Add($sb.ToString()) }
        return $true }
    [void][V]::EnumChildWindows($h, $cb, [IntPtr]::Zero)
    $script:lbls }

$exe = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot.exe"
$logPath = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot_diag.log"
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700
Remove-Item $logPath -ErrorAction SilentlyContinue
$env:PIXSHOT_NO_UI_ALERTS='1'

$form = New-Object System.Windows.Forms.Form
$form.StartPosition='Manual'; $form.Location = New-Object System.Drawing.Point(300, 300)
$form.Size = New-Object System.Drawing.Size(600, 400); $form.Text = "PixShotTarget"
$form.TopMost = $true; $form.Show()
for ($i=0;$i -lt 12;$i++){ [void][System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 50 }
W "靶标窗口: (300,300) 600x400"

$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 2
$main = Find-W $p.Id 'PixShotMain'
W "pixshot PID=$($p.Id) 存活=$(-not $p.HasExited) main=0x$($main.ToString('X'))"

W ""
W "== #1a 托盘右键回调 -> 自绘菜单弹出，点击退出 =="
[void][V]::PostMessageW($main, 0x0401, (LP 900 400), [IntPtr]((1 -shl 16) -bor 0x007B))   # 实测编码: wp=坐标, lp=事件|ID<<16
$menu = Wait-W $p.Id 'PixShotTrayMenu' 4000
W "  菜单窗口出现: $($menu -ne [IntPtr]::Zero)   <- 期望 True"
if ($menu -ne [IntPtr]::Zero) {
    $r = New-Object V+RECT; [void][V]::GetWindowRect($menu, [ref]$r)
    W ("  菜单位置 ({0},{1})-({2},{3})  尺寸 {4}x{5}" -f $r.L,$r.T,$r.R,$r.B,($r.R-$r.L),($r.B-$r.T))
    [void][V]::SendMessageW($menu, 0x0201, [IntPtr]1, (LP 76 25))
    Start-Sleep -Milliseconds 900
    W "  点击[退出]后进程存活: $(-not $p.HasExited)   <- 期望 False"
    W "  剩余 pixshot 进程数: $((Get-Process pixshot -ErrorAction SilentlyContinue | Measure-Object).Count)  <- 期望 0"
}

W ""
W "== #1b 直接 WM_CONTEXTMENU 路径 + Esc 关闭 =="
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600
$p2 = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 2
$main2 = Find-W $p2.Id 'PixShotMain'
[void][V]::PostMessageW($main2, 0x007B, [IntPtr]::Zero, [IntPtr]::Zero)
$menu2 = Wait-W $p2.Id 'PixShotTrayMenu' 4000
W "  菜单窗口出现: $($menu2 -ne [IntPtr]::Zero)   <- 期望 True"
if ($menu2 -ne [IntPtr]::Zero) {
    [void][V]::SendMessageW($menu2, 0x0100, [IntPtr]0x1B, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 500
    W "  Esc 关闭菜单: $(-not [V]::IsWindow($menu2))   <- 期望 True"
}

W ""
W "== #3 拖动框选区域（400,350 → 1000,750，600x400）=="
[void][V]::PostMessageW($main2, 0x0312, [IntPtr]1, [IntPtr]0)
$ov = Wait-W $p2.Id 'PixShotOverlay' 8000
W "  遮罩已打开: $($ov -ne [IntPtr]::Zero)"
if ($ov -ne [IntPtr]::Zero) {
    Start-Sleep -Milliseconds 600
    [void][V]::SendMessageW($ov, 0x0201, [IntPtr]1, (LP 400 350))
    [void][V]::SendMessageW($ov, 0x0200, [IntPtr]1, (LP 1000 750))
    Start-Sleep -Milliseconds 150
    [void][V]::SendMessageW($ov, 0x0202, [IntPtr]0, (LP 1000 750))
    Start-Sleep -Milliseconds 300
    W "  框选后遮罩关闭: $(-not [V]::IsWindow($ov))   <- 期望 True"
    $tb = Wait-W $p2.Id 'PixShotToolbar' 6000
    if ($tb -ne [IntPtr]::Zero) {
        $lbls = Children $tb
        W "  工具栏文本: $($lbls -join ' | ')"
        $l = ($lbls | Where-Object { $_ -match '已截取' } | Select-Object -First 1)
        if ($l -and $l -match '已截取\s*(\d+)\s*[×x]\s*(\d+)') {
            W "  选区尺寸: $($Matches[1]) x $($Matches[2])   <- 拖动 600x400"
        }
        # == #5 双击关闭贴图 ==
        W ""
        W "== #5 贴图双击关闭 =="
        $btnPin = [System.IntPtr]::Zero
        $c = [V]::GetWindow($tb, 5)
        while ($c -ne [System.IntPtr]::Zero) {
            if ([V]::GetDlgCtrlID($c) -eq 1) { $btnPin = $c; break }
            $c = [V]::GetWindow($c, 2)
        }
        [void][V]::SendMessageW($btnPin, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)
        $pin = Wait-W $p2.Id 'PixShotPin' 5000
        W "  贴图窗口已创建: $($pin -ne [IntPtr]::Zero)"
        if ($pin -ne [IntPtr]::Zero) {
            $cls2 = [V]::GetClassLongW($pin, -26)
            W ("  贴图类样式=0x{0:X8} 含 CS_DBLCLKS: {1}" -f $cls2, (($cls2 -band 0x8) -ne 0))
            [void][V]::SendMessageW($pin, 0x0203, [IntPtr]1, (LP 60 60))   # WM_LBUTTONDBLCLK
            Start-Sleep -Milliseconds 400
            W "  双击后贴图仍在: $([V]::IsWindow($pin))   <- 期望 False（已退出贴图）"
        }
    } else { W "  !! 工具栏未出现" }
}

W ""
W "== 程序日志 =="
if (Test-Path $logPath) {
    foreach ($l in (Get-Content $logPath)) {
        if ($l -match 'UI 字体族|自绘菜单|WM_CONTEXTMENU|NIM_ADD 第 1|双击') { W "  $l" }
    }
}
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
try { $form.Close() } catch {}
W "=== 复验结束 ==="

