# PixShot 托盘/热键根因诊断脚本
# 启动 exe → 等待 → 收集进程状态与程序自报日志 → 交叉验证 F1 占用 → 结束
$ErrorActionPreference = 'Continue'
$exe = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot.exe"
$log = "C:\Users\user\Desktop\pixpin\pixshot\target\release\pixshot_diag.log"
$out = "C:\Users\user\Desktop\pixpin\pixshot\diag_probe.txt"
function W($s) { Add-Content -Path $out -Value $s -Encoding UTF8 }
Set-Content -Path $out -Value "=== 托盘/热键诊断 $(Get-Date -Format 'HH:mm:ss') ===" -Encoding UTF8

Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 600
Remove-Item $log -ErrorAction SilentlyContinue

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class P {
    [DllImport("user32.dll", SetLastError = true)] public static extern bool RegisterHotKey(IntPtr h, int id, uint m, uint vk);
    [DllImport("user32.dll", SetLastError = true)] public static extern bool UnregisterHotKey(IntPtr h, int id);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
    [StructLayout(LayoutKind.Sequential)] public struct R { public int L, T, Rt, B; }
}
"@

$p = Start-Process -FilePath $exe -PassThru
W "已启动 PID=$($p.Id)"
Start-Sleep -Seconds 3

$alive = -not $p.HasExited
W "3 秒后进程存活: $alive"
if (-not $alive) {
    W "!!! 进程已退出，ExitCode=$($p.ExitCode) (0x$('{0:X8}' -f $p.ExitCode))"
} else {
    $g = Get-Process -Id $p.Id
    W ("  内存 WS={0:N2}MB  线程={1}" -f ($g.WorkingSet64/1MB), $g.Threads.Count)
    # 枚举该进程的窗口（确认宿主窗口存在）
    $script:tp = $p.Id
    $script:hits = New-Object System.Collections.Generic.List[string]
    $cb = [P+EnumProc]{ param($h, $l)
        $wp = 0; [void][P]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $script:tp) {
            $r = New-Object P+R
            [void][P]::GetWindowRect($h, [ref]$r)
            $cls = [P]::FindWindowW('PixShotMain', $null)
            $script:hits.Add(("hwnd=0x{0:X} rect=({1},{2})-({3},{4})" -f $h.ToString('X'), $r.L, $r.T, $r.Rt, $r.B))
        }
        return $true }
    [void][P]::EnumWindows($cb, [IntPtr]::Zero)
    W "  该进程窗口数: $($script:hits.Count)"
    foreach ($s in $script:hits) { W "    $s" }
}

W "--- 程序自报诊断日志 ---"
if (Test-Path $log) {
    Get-Content $log | ForEach-Object { W "  $_" }
} else {
    W "  !! 日志文件不存在：目标 $log"
}

W "--- 交叉验证 F1 当前是否被占用 ---"
$r = [P]::RegisterHotKey([IntPtr]::Zero, 98, 0x4000, 0x70)
$err = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
if ($r) {
    W "  探测注册成功 -> F1 空闲，pixshot 未持有 F1"
    [void][P]::UnregisterHotKey([IntPtr]::Zero, 98)
} else {
    W "  探测注册失败 error=$err -> $(if ($err -eq 1409) { 'F1 已被 pixshot 持有（注册成功）' } else { '其他原因' })"
}

Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
W "=== 诊断结束（已结束测试进程）==="
