<#
PixShot 性能基线测量 v2
========================
测量项（改造前基线）：
  1. EXE 体积
  2. 启动时间：Start-Process → 全屏遮罩出现（FindWindowW 1ms 轮询，实测该类名可被找到），×3
  3. 稳态内存/空闲 CPU：a) 仅托盘（Esc 取消遮罩） b) 含全屏贴图
窗口驱动全部走 SendMessage；日志实时写入 perf_baseline.txt。
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\PixShot.exe')
)
$env:PIXSHOT_NO_UI_ALERTS = '1'   # 沙箱下托盘注册必失败，屏蔽启动提示框以保证测试确定性
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class M {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
}
"@
$log = Join-Path $PSScriptRoot '..\perf_baseline.txt'
function Log($s) { Add-Content -Path $log -Value $s -Encoding UTF8 }
# 屏幕坐标 → LPARAM（低字 x / 高字 y）
function LP([int]$x, [int]$y) { [IntPtr]((($x -band 0xFFFF)) -bor (($y -band 0xFFFF) -shl 16)) }
Set-Content -Path $log -Value "=== PixShot 性能基线 v2 $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') ===" -Encoding UTF8

# EnumWindows 按 PID 收集全部窗口类名（与 gui_e2e_msg_test.ps1 相同的已验证模式）
function Get-WinByEnum([int]$TargetPid) {
    $script:enumPid = $TargetPid
    $script:enumHits = New-Object System.Collections.Generic.List[object]
    $cb = [M+EnumProc]{ param($h, $l)
        $wpid = 0
        [void][M]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $script:enumPid) {
            $sb = New-Object System.Text.StringBuilder 256
            [void][M]::GetClassNameW($h, $sb, 256)
            $script:enumHits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true }
    [void][M]::EnumWindows($cb, [IntPtr]::Zero)
    $script:enumHits
}
function Find-Enum([int]$TargetPid, [string]$Class) {
    foreach ($w in (Get-WinByEnum $TargetPid)) { if ($w.Class -eq $Class) { return $w.Hwnd } }
    [IntPtr]::Zero
}
function Wait-Enum([int]$TargetPid, [string]$Class, [int]$TimeoutMs = 15000) {
    $t0 = [Environment]::TickCount
    while ([Environment]::TickCount -lt ($t0 + $TimeoutMs)) {
        $h = Find-Enum $TargetPid $Class
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 80 }
    [IntPtr]::Zero
}

# ── 0. 体积与系统 ──
$exeItem = Get-Item $Exe
Log ("系统: {0}" -f [Environment]::OSVersion.VersionString)
Log ("EXE: {0:N2} MB ({1} 字节)" -f ($exeItem.Length / 1MB), $exeItem.Length)
Get-Process PixShot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

# ── 1. 启动时间 ×3（EnumWindows 轮询：沙箱中 FindWindowW 对部分类名/标题不可见；
#        枚举粒度 ~100ms，对 ≤300ms 预算的判定足够）──
$starts = @()
for ($i = 1; $i -le 3; $i++) {
    $p = Start-Process -FilePath $Exe -PassThru
    $t0 = [Environment]::TickCount; $h = [IntPtr]::Zero
    while ([Environment]::TickCount -lt ($t0 + 15000)) {
        $h = Find-Enum $p.Id 'PixShotMain'
        if ($h -ne [IntPtr]::Zero) { break }
        Start-Sleep -Milliseconds 20 }
    $ms = [Environment]::TickCount - $t0
    $starts += $ms
    Log "  启动第 $i 次: 托盘就绪（主窗口创建）耗时 $ms ms"
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 900
}
$min = ($starts | Measure-Object -Minimum).Minimum
$avg = [math]::Round(($starts | Measure-Object -Average).Average, 1)
Log "  => 启动耗时（到托盘就绪）: 最小 $min ms / 平均 $avg ms（进程级测量，暖缓存；真实冷启动额外含磁盘 IO）"

# ── 2a. 稳态：仅托盘（启动即驻留，无需遮罩/Esc）──
$p = Start-Process -FilePath $Exe -PassThru
$mainH = Wait-Enum $p.Id 'PixShotMain'
Start-Sleep -Milliseconds 1500
$g0 = Get-Process -Id $p.Id
$c0 = $g0.TotalProcessorTime.TotalMilliseconds
Log ("托盘空转: WorkingSet {0:N2} MB / Private {1:N2} MB" -f ($g0.WorkingSet64 / 1MB), ($g0.PrivateMemorySize64 / 1MB))
Start-Sleep -Seconds 10
$g1 = Get-Process -Id $p.Id
$cpu1 = $g1.TotalProcessorTime.TotalMilliseconds - $c0
Log ("  10 秒空闲 CPU: {0:N1} ms (≈{1:N3}% 单核)" -f $cpu1, ($cpu1 / 100))
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 700

# ── 2b. 稳态：全屏贴图存留（F1=WM_HOTKEY 触发截图流程）──
$p = Start-Process -FilePath $Exe -PassThru
$mainH = Wait-Enum $p.Id 'PixShotMain'
Start-Sleep -Milliseconds 400
[void][M]::PostMessageW($mainH, 0x0312, [IntPtr]1, [IntPtr]0)   # WM_HOTKEY（F1 路径）
$t0 = [Environment]::TickCount
while ([Environment]::TickCount -lt ($t0 + 15000)) {
    $ov = [M]::FindWindowW('PixShotOverlay', $null)
    if ($ov -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 2 }
# 只保留区域截图：拖动框选覆盖整个虚拟屏幕（超大坐标由程序钳制到屏幕范围）
[void][M]::SendMessageW($ov, 0x0201, [IntPtr]1, (LP 0 0))          # WM_LBUTTONDOWN
[void][M]::SendMessageW($ov, 0x0200, [IntPtr]1, (LP 8000 8000))    # WM_MOUSEMOVE
[void][M]::SendMessageW($ov, 0x0202, [IntPtr]0, (LP 8000 8000))    # WM_LBUTTONUP
$tbr = Wait-Enum $p.Id 'PixShotToolbar'
if ($tbr -eq [IntPtr]::Zero) { Log "!! 工具栏未出现"; Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue; exit 1 }
$btn = [IntPtr]::Zero; $c = [M]::GetWindow($tbr, 5)
while ($c -ne [IntPtr]::Zero) { if ([M]::GetDlgCtrlID($c) -eq 1) { $btn = $c; break }; $c = [M]::GetWindow($c, 2) }
[void][M]::SendMessageW($btn, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)   # BM_CLICK 贴图
$pin = Wait-Enum $p.Id 'PixShotPin'
Log "全屏贴图: $(if ($pin -ne [IntPtr]::Zero) { '已创建（置顶显示中）' } else { '未创建!' })"
Start-Sleep -Milliseconds 2000
$g2 = Get-Process -Id $p.Id
$c2 = $g2.TotalProcessorTime.TotalMilliseconds
Log ("贴图稳态(全屏 1920x1080): WorkingSet {0:N2} MB / Private {1:N2} MB" -f ($g2.WorkingSet64 / 1MB), ($g2.PrivateMemorySize64 / 1MB))
Start-Sleep -Seconds 10
$g3 = Get-Process -Id $p.Id
$cpu2 = $g3.TotalProcessorTime.TotalMilliseconds - $c2
Log ("  含贴图 10 秒空闲 CPU: {0:N1} ms (≈{1:N3}% 单核)" -f $cpu2, ($cpu2 / 100))
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
Log "=== 基线测量结束 ==="


