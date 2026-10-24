# DPI 感知的真实右键：先展开 ^ 溢出面板，再右键图标，观察外壳回调与自绘菜单
$out = "C:\Users\user\Desktop\pixpin\pixshot\tray_realrt2.txt"
function W($s) { Add-Content -Path $out -Value $s -Encoding UTF8 }
Set-Content -Path $out -Value "=== DPI 感知真实右键 $(Get-Date -Format 'HH:mm:ss') ===" -Encoding UTF8

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class R2 {
    [StructLayout(LayoutKind.Sequential)] public struct NII { public uint cbSize; public IntPtr hWnd; public uint uID; public Guid guidItem; }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, Rt, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("shell32.dll", SetLastError = true)] public static extern int Shell_NotifyIconGetRect(ref NII id, out RECT r);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
}
"@
# 关键：Per-Monitor-V2 DPI 感知，坐标才是物理像素
[void][R2]::SetProcessDpiAwarenessContext([IntPtr](-4))

function Find-W([int]$TargetPid, [string]$Class) {
    $script:tp = $TargetPid; $script:hits = New-Object System.Collections.Generic.List[object]
    $cb = [R2+EnumProc]{ param($h, $l)
        $wp = 0; [void][R2]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $script:tp) {
            $sb = New-Object System.Text.StringBuilder 64
            [void][R2]::GetClassNameW($h, $sb, 64)
            $script:hits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true }
    [void][R2]::EnumWindows($cb, [IntPtr]::Zero)
    foreach ($w in $script:hits) { if ($w.Class -eq $Class) { return $w.Hwnd } }
    [IntPtr]::Zero }
function Click([int]$x, [int]$y, [bool]$right) {
    [void][R2]::SetCursorPos($x, $y); Start-Sleep -Milliseconds 250
    $p = New-Object R2+POINT; [void][R2]::GetCursorPos([ref]$p)
    if ($right) { [R2]::mouse_event(0x0008,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 80; [R2]::mouse_event(0x0010,0,0,0,[UIntPtr]::Zero) }
    else        { [R2]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 80; [R2]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero) }
    return "$($p.X),$($p.Y)" }

$exe = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot.exe"
$logPath = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot_diag.log"
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700
Remove-Item $logPath -ErrorAction SilentlyContinue
$env:PIXSHOT_NO_UI_ALERTS='1'
$p = Start-Process -FilePath $exe -PassThru
Start-Sleep -Seconds 2
$main = Find-W $p.Id 'PixShotMain'
W "pixshot PID=$($p.Id)"

function IconRect($main) {
    $id = New-Object R2+NII
    $id.cbSize = [Runtime.InteropServices.Marshal]::SizeOf([type][R2+NII])
    $id.hWnd = $main; $id.uID = 1
    $r = New-Object R2+RECT
    $hr = [R2]::Shell_NotifyIconGetRect([ref]$id, [ref]$r)
    return @($hr, $r)
}

# ① 当前（溢出面板未展开）图标矩形
$res = IconRect $main
W "① 溢出未展开时：hr=0x$('{0:X8}' -f $res[0])  rect=($($res[1].L),$($res[1].T))-($($res[1].Rt),$($res[1].B))"
$trayR = New-Object R2+RECT; [void][R2]::GetWindowRect([R2]::FindWindowW('Shell_TrayWnd',$null), [ref]$trayR)
W "   任务栏 rect=($($trayR.L),$($trayR.T))-($($trayR.Rt),$($trayR.B))  (已 DPI 感知，物理像素)"

# ② 尝试用 UIAutomation 找到并点击 ^ 溢出按钮
W ""
W "② 尝试展开 ^ 溢出面板（UIAutomation 查找溢出按钮）"
$opened = $false
try {
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes
    $root = [System.Windows.Automation.AutomationElement]::RootElement
    $cond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ClassNameProperty, "Shell_TrayWnd")
    $taskbar = $root.FindFirst([System.Windows.Automation.TreeScope]::Children, $cond)
    if ($taskbar -ne $null) {
        $desc = $taskbar.FindAll([System.Windows.Automation.TreeScope]::Descendants,
            [System.Windows.Automation.Condition]::TrueCondition)
        W "   任务栏 UIA 后代元素数: $($desc.Count)"
        foreach ($el in $desc) {
            $n = $el.Current.Name
            $t = $el.Current.ControlType.ProgrammaticName
            if ($n -match '隐藏的图标|hidden icons|溢出|Overflow') {
                $b = $el.Current.BoundingRectangle
                W "   找到溢出按钮: 名称='$n' 类型=$t 位置=($([int]$b.X),$([int]$b.Y)) 尺寸=$([int]$b.Width)x$([int]$b.Height)"
                if ($b.Width -gt 0) {
                    $cx = [int]($b.X + $b.Width/2); $cy = [int]($b.Y + $b.Height/2)
                    $script:overflowXY = @($cx, $cy)
                    $actual = Click $cx $cy $false
                    W "   已左键点击溢出按钮 (目标 $cx,$cy 实际 $actual)"
                    $opened = $true
                }
                break
            }
        }
        if (-not $opened) { W "   未在任务栏 UIA 后代中找到名称含「隐藏的图标」的元素" }
    } else { W "   未找到 Shell_TrayWnd 的 UIA 元素" }
} catch { W "   UIAutomation 失败: $($_.Exception.Message)" }
Start-Sleep -Milliseconds 800

# ③ 右键图标：对 ^ 面板的未知状态健壮 —— 先直点，失败再切换 ^ 重试
W ""
W "③ 右键图标"
$script:pid0 = $p.Id
$menu = [IntPtr]::Zero

function Try-Menu($main, $tag) {
    $res = IconRect $main
    if ($res[0] -ne 0) { W "   [$tag] 取图标矩形失败 hr=0x$('{0:X8}' -f $res[0])"; return [IntPtr]::Zero }
    $cx = [int](($res[1].L + $res[1].Rt)/2); $cy = [int](($res[1].T + $res[1].B)/2)
    $act = Click $cx $cy $true
    W "   [$tag] 图标 rect=($($res[1].L),$($res[1].T))-($($res[1].Rt),$($res[1].B)) 右键(目标 $cx,$cy 实际 $act)"
    $t0 = [Environment]::TickCount
    while ([Environment]::TickCount -lt ($t0 + 2500)) {
        $m = Find-W $script:pid0 'PixShotTrayMenu'
        if ($m -ne [IntPtr]::Zero) { return $m }
        Start-Sleep -Milliseconds 100
    }
    return [IntPtr]::Zero
}

$menu = Try-Menu $main '第1次'
if ($menu -eq [IntPtr]::Zero) {
    # 图标在 ^ 溢出区且面板未展开时，GetRect 会返回 ^ 按钮矩形 —— 需要先把面板展开。
    # 判据：展开后图标矩形会移到任务栏上方（y < 任务栏顶边）。
    W "   第1次未出现菜单 -> 确保 ^ 面板展开后重试"
    $trayTop = $trayR.T
    $ok = $false
    for ($i = 1; $i -le 3; $i++) {
        $r0 = IconRect $main
        if ($r0[0] -eq 0 -and $r0[1].T -lt $trayTop) { $ok = $true; W "   [$i] 面板已展开，图标 rect=($($r0[1].L),$($r0[1].T))-($($r0[1].Rt),$($r0[1].B))"; break }
        W "   [$i] 面板未展开（图标矩形 y=$($r0[1].T) 未高于任务栏顶 $trayTop）-> 点击 ^ 切换"
        if ($script:overflowXY) { [void](Click $script:overflowXY[0] $script:overflowXY[1] $false) }
        Start-Sleep -Milliseconds 900
    }
    if ($ok) { $menu = Try-Menu $main '第2次' }
}
W "   右键后自绘菜单出现: $($menu -ne [IntPtr]::Zero)"
if ($menu -ne [IntPtr]::Zero) {
    $mr = New-Object R2+RECT; [void][R2]::GetWindowRect($menu, [ref]$mr)
    W "   菜单 rect=($($mr.L),$($mr.T))-($($mr.Rt),$($mr.B))"
    Start-Sleep -Milliseconds 700
    W "   0.7s 后菜单仍存在: $([R2]::IsWindow($menu))"
    # 点击「退出」项
    if ([R2]::IsWindow($menu)) {
        [void][R2]::SetCursorPos($mr.L + 60, $mr.T + 20)
        Start-Sleep -Milliseconds 200
        [R2]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 60; [R2]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero)
        Start-Sleep -Milliseconds 900
        W "   点击项后进程存活: $(-not $p.HasExited)   <- False 表示「退出」生效"
    }
}

W ""
W "④ 程序日志（托盘）"
if (Test-Path $logPath) { foreach ($l in (Get-Content $logPath)) { if ($l -match '\[tray\]') { W "  $l" } } }
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
W "=== 结束 ==="
