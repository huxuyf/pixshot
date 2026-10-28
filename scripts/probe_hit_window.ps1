# 临时探针：识别 (375,375) 处命中的窗口（诊断 overlay 被遮挡问题）
Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
public static class W3 {
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint f);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern long GetWindowLongW(IntPtr h, int i);
  public struct RECT { public int L, T, R, B; }
}
'@
$h = [IntPtr]$args[0]
$sb = New-Object System.Text.StringBuilder 256
[void][W3]::GetClassNameW($h, $sb, 256)
$sb2 = New-Object System.Text.StringBuilder 256
[void][W3]::GetWindowTextW($h, $sb2, 256)
$procId = 0
[void][W3]::GetWindowThreadProcessId($h, [ref]$procId)
$root = [W3]::GetAncestor($h, 2)
$r = New-Object W3+RECT
[void][W3]::GetWindowRect($h, [ref]$r)
$ex = [W3]::GetWindowLongW($h, -20)
$proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
Write-Host "class=$($sb.ToString()) title=$($sb2.ToString()) pid=$procId ($($proc.ProcessName)) root=$root visible=$([W3]::IsWindowVisible($h)) exstyle=0x$($ex.ToString('X')) rect=($($r.L),$($r.T))-($($r.R),$($r.B))"
