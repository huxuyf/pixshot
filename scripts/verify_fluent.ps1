<#
Fluent 视觉自动化验证 v2（Step 5）
====================================
关键修正：本脚本以 Per-Monitor-V2 DPI 感知运行（坐标=物理像素）；
品红靶标仅用于贴图（贴图自带图像，验证 DONOTROUND/描边/保真）；
结果窗口验证用灰色衬底窗（置于其下）产生强对比：
  - 贴图角(2,2) 应为品红(255,0,255) —— DONOTROUND（若被圆角裁掉则露桌面）
  - 贴图边(0,300) 应为品红*80%+黑20% ≈ (204,0,204) —— DWMWA_BORDER_COLOR 描边
  - 结果窗右上角 应为衬底灰(128,128,128) —— DWMWCP_ROUND 圆角（方角则显窗口本色）
  - 结果窗右边 应为描边灰(102,102,102)
  - 卡片边缘 margin 应为卡片色（浅色主题=(255,255,255)）
  - 标题带 不应为纯黑(0,0,0)（纯黑=Mica 显影失败）
#>
$env:PIXSHOT_NO_UI_ALERTS = '1'   # 沙箱下托盘注册必失败，屏蔽启动提示框以保证测试确定性
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class V2 {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out R r);
    [StructLayout(LayoutKind.Sequential)] public struct R { public int L, T, Rt, B; }
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
}
"@
[void][V2]::SetProcessDpiAwarenessContext([IntPtr](-4))   # 物理像素坐标

$log = Join-Path $PSScriptRoot '..\fluent_verify.txt'
function Log($s) { Add-Content -Path $log -Value $s -Encoding UTF8 }
Set-Content -Path $log -Value "=== Fluent 视觉验证 v2 $(Get-Date -Format 'HH:mm:ss') ===" -Encoding UTF8

function Find-Enum([int]$TargetPid, [string]$Class) {
    $script:tp = $TargetPid; $script:hits = New-Object System.Collections.Generic.List[object]
    $cb = [V2+EnumProc]{ param($h, $l)
        $wp = 0; [void][V2]::GetWindowThreadProcessId($h, [ref]$wp)
        if ($wp -eq $script:tp) {
            $sb = New-Object System.Text.StringBuilder 64
            [void][V2]::GetClassNameW($h, $sb, 64)
            $script:hits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true }
    [void][V2]::EnumWindows($cb, [IntPtr]::Zero)
    foreach ($w in $script:hits) { if ($w.Class -eq $Class) { return $w.Hwnd } }
    [IntPtr]::Zero }
function Wait-Enum([int]$TargetPid, [string]$Class, [int]$TimeoutMs = 12000) {
    $t0 = [Environment]::TickCount
    while ([Environment]::TickCount -lt ($t0 + $TimeoutMs)) {
        $h = Find-Enum $TargetPid $Class
        if ($h -ne [IntPtr]::Zero) { return $h }
        [void][System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 80 }
    [IntPtr]::Zero }
function LP([int]$x, [int]$y) { [IntPtr](($x -band 0xFFFF) -bor (($y -band 0xFFFF) -shl 16)) }
function Get-Rect([IntPtr]$H) {
    $r = New-Object V2+R
    [void][V2]::GetWindowRect($H, [ref]$r); $r }
function Get-Client([IntPtr]$H) {
    $r = New-Object V2+R
    [void][V2]::GetClientRect($H, [ref]$r); $r }
function Log-Rects([string]$Tag, [IntPtr]$H) {
    $w = Get-Rect $H; $c = Get-Client $H
    $ww = $w.Rt - $w.L; $wh = $w.B - $w.T
    Log ("  [{0}] 窗口 {1}x{2} @({3},{4})，客户区 {5}x{6}" -f $Tag, $ww, $wh, $w.L, $w.T, $c.Rt, $c.B) }
function Px([int]$x, [int]$y) {
    $b = New-Object System.Drawing.Bitmap(1, 1)
    $g = [System.Drawing.Graphics]::FromImage($b)
    $g.CopyFromScreen($x, $y, 0, 0, $b.Size); $g.Dispose()
    $c = $b.GetPixel(0, 0); $b.Dispose()
    "($($c.R),$($c.G),$($c.B))" }
function Pump([int]$ms = 120) {
    $end = [Environment]::TickCount + $ms
    while ([Environment]::TickCount -lt $end) {
        [void][System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 15 } }

$light = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize' -Name AppsUseLightTheme -ErrorAction SilentlyContinue).AppsUseLightTheme -ne 0
Log "系统主题: $(if ($light) { '浅色' } else { '深色' })"

$proc = $null
$form = $null
try {
    # ── A. 品红靶标（TopMost，覆盖选区 300,300-900,700）──
    $form = New-Object System.Windows.Forms.Form
    $form.StartPosition = 'Manual'
    $form.Location = New-Object System.Drawing.Point(250, 250)
    $form.Size = New-Object System.Drawing.Size(700, 500)
    $form.FormBorderStyle = 'None'
    $form.BackColor = [System.Drawing.Color]::FromArgb(255, 0, 255)
    $form.TopMost = $true
    $form.Show(); Pump 600
    Log "品红靶标已上屏 (250,250)-(950,750)"

    # ── B. 启动（托盘驻留）→ F1 触发 → 拖拽 → 工具栏 → OCR 窗口 → 贴图 ──
    $proc = Start-Process -FilePath (Join-Path $PSScriptRoot '..\target\release\pixshot.exe') -PassThru
    $pid0 = $proc.Id
    $mainH = Wait-Enum $pid0 'PixShotMain'
    Start-Sleep -Milliseconds 400
    $noOv = [V2]::FindWindowW('PixShotOverlay', $null) -eq [IntPtr]::Zero
    Log "启动托盘驻留（无遮罩）：$noOv"
    # F1 触发（WM_HOTKEY 路径）
    [void][V2]::PostMessageW($mainH, 0x0312, [IntPtr]1, [IntPtr]0)
    $t0 = [Environment]::TickCount
    while ([Environment]::TickCount -lt ($t0 + 15000)) {
        $ov = [V2]::FindWindowW('PixShotOverlay', $null)
        if ($ov -ne [IntPtr]::Zero) { break }
        Pump 4 }
    [void][V2]::SendMessageW($ov, 0x0201, [IntPtr]1, (LP 300 300))
    foreach ($p in @(@(560, 480), @(900, 700))) {
        [void][V2]::SendMessageW($ov, 0x0200, [IntPtr]1, (LP $p[0] $p[1]))
        Start-Sleep -Milliseconds 20 }
    [void][V2]::SendMessageW($ov, 0x0202, [IntPtr]0, (LP 900 700))
    $tb = Wait-Enum $pid0 'PixShotToolbar'
    if ($tb -eq [IntPtr]::Zero) { throw '工具栏未出现' }
    Log "工具栏已出现"

    $btn = [IntPtr]::Zero; $c = [V2]::GetWindow($tb, 5)
    while ($c -ne [IntPtr]::Zero) { if ([V2]::GetDlgCtrlID($c) -eq 4) { $btn = $c; break }; $c = [V2]::GetWindow($c, 2) }
    [void][V2]::SendMessageW($btn, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)   # OCR
    $ocrw = Wait-Enum $pid0 'PixShotOcrResult'
    if ($ocrw -eq [IntPtr]::Zero) { throw '结果窗口未出现' }
    Pump 300

    $btnPin = [IntPtr]::Zero; $c = [V2]::GetWindow($tb, 5)
    while ($c -ne [IntPtr]::Zero) { if ([V2]::GetDlgCtrlID($c) -eq 1) { $btnPin = $c; break }; $c = [V2]::GetWindow($c, 2) }
    [void][V2]::SendMessageW($btnPin, 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero)   # 贴图
    $pin = Wait-Enum $pid0 'PixShotPin'
    if ($pin -eq [IntPtr]::Zero) { throw '贴图未出现' }
    Pump 200
    Log-Rects 't1 贴图创建后' $pin
    Pump 500
    Log-Rects 't2 +0.7s' $pin
    $pr = Get-Rect $pin
    Log ("贴图 rect=({0},{1})-({2},{3})" -f $pr.L, $pr.T, $pr.Rt, $pr.B)

    # ── C. 贴图验证（品红证据链；贴图内部=捕获时靶标影像，与靶标当前位置无关）──
    $pinCorner = Px ($pr.L + 2) ($pr.T + 2)
    $pinBorder = Px $pr.L ($pr.T + 300)
    $pinInner = Px ($pr.L + 60) ($pr.T + 60)
    Log "  贴图角(2,2) = $pinCorner  [品红(255,0,255)=DONOTROUND ✓ / 露底=被裁角 ✗]"
    Log "  贴图左边(0,300) = $pinBorder  [≈(204,0,204)=描边 ✓]"
    Log "  贴图内部(60,60) = $pinInner  [品红=图像保真 ✓]"

    # ── D. 结果窗口验证：靶标转为灰色衬底，非置顶，挪到结果窗口正后方 ──
    $rr = Get-Rect $ocrw
    $cw = $rr.Rt - $rr.L; $ch = $rr.B - $rr.T
    Log ("结果窗口 rect=({0},{1})-({2},{3}) {4}x{5}" -f $rr.L, $rr.T, $rr.Rt, $rr.B, $cw, $ch)
    $form.TopMost = $false
    $form.BackColor = [System.Drawing.Color]::FromArgb(128, 128, 128)
    $form.Location = New-Object System.Drawing.Point(($rr.L - 20), ($rr.T - 20))
    $form.Size = New-Object System.Drawing.Size(($cw + 40), ($ch + 40))
    [void][V2]::SetForegroundWindow($ocrw)   # 确保结果窗口在衬底之上
    Pump 500
    Log-Rects 't3 表单移动+前台切换后' $pin
    Log "  [t3 实时] 贴图角(2,2)=$(Px ($pr.L + 2) ($pr.T + 2))，左边(0,300)=$(Px $pr.L ($pr.T + 300))"
    Pump 1000
    Log "  [t4 +1s 实时] 贴图角(2,2)=$(Px ($pr.L + 2) ($pr.T + 2))，左边(0,300)=$(Px $pr.L ($pr.T + 300))"
    Log-Rects 't4 矩形' $pin

    # 采右侧（避开置顶贴图遮挡区 x<900）
    $cCorner = Px ($rr.Rt - 2) ($rr.T + 1)          # 右上角
    $cBorder = Px ($rr.Rt - 1) ($rr.T + 200)        # 右边线
    $cCard = Px ($rr.Rt - 20) ($rr.T + 50)         # 卡片右上 margin（避开 EDIT：EDIT 右缘=客户区-24）
    $cHeader = Px ($rr.Rt - 60) ($rr.T + 8)        # 标题带（Mica 或回退色）
    Log "  结果窗右上角(Rt-2,T+1) = $cCorner  [灰(128,128,128)=圆角✓ / 卡片色=方角✗]"
    Log "  结果窗右边(Rt-1,T+200) = $cBorder  [描边：灰底*80% ≈(102,102,102) ✓]"
    Log "  结果窗卡片margin(Rt-20,T+50) = $cCard  [浅色=(255,255,255)/深色=(43,43,43)]"
    Log "  结果窗标题带(Rt-60,T+8) = $cHeader  [非纯黑(0,0,0)即可；≈基色/材质色]"

    # ── E. 全屏存档 ──
    $sw = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width
    $sh = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Height
    $bmp = New-Object System.Drawing.Bitmap($sw, $sh)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen(0, 0, 0, 0, $bmp.Size); $g.Dispose()
    $bmp.Save((Join-Path $PSScriptRoot '..\e2e_artifacts\fluent_final.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Log "存档: e2e_artifacts\fluent_final.png"
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
    if ($form) { try { $form.Close(); [void][System.Windows.Forms.Application]::DoEvents() } catch { } }
}
Log "=== 验证结束 ==="


