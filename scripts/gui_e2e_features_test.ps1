<#
PixShot 新功能 GUI 端到端测试
==============================
在真实桌面上跑通「智能选区单击扩充 → 截图辅助取色 → 颜色面板 → 橡皮/水印按钮 → 贴图锁定/透明度」：

  1. 默认 pick_level=region：不拖动、单击即自动扩充为光标下窗口并进入标注；
  2. 选区阶段按 C 把光标像素颜色复制到剪贴板（Hex RGB）；
  3. 工具条新增按钮 ID 18=「更多」/ 17=颜色面板；悬停「更多」弹出子工具条
     （含橡皮 ID 15 / 水印 ID 16 等 8 个标注按钮）；
  4. 功能组（贴图/保存/复制/OCR）与标注组之间有分组竖杠间距；
  5. 点击 17 弹出颜色面板（位于工具条下方并右对齐），仅含 8 个颜色块（101–108，
     顺序为红/绿/蓝/青/品红/黄/黑/白），选色后自动收起，再次点 17 可重开/关闭；
  6. 悬停「颜色面板」按钮（空快捷键 tip）不崩溃且文案正确；
  7. 选中标注工具后，光标在画布上 Ctrl+滚轮即可调整「线宽 / 字号」，Toast 实时刷新当前值；
  8. Ctrl+T 创建贴图；右键菜单「锁定位置」后拖动无效，「解锁位置」后可拖动；
  9. Ctrl+滚轮调整透明度（窗口尺寸不变）；「关闭贴图」后窗口销毁；双击图像区关闭。

前置条件：已执行 cargo build（默认使用 target\debug\pixshot.exe）。

说明：本脚本设置 PIXSHOT_OVERLAY_CAPTURABLE=1，让冻结层可被普通屏幕截图捕获
（正常使用下冻结层是 WDA_EXCLUDEFROMCAPTURE 的）。

注意：脚本运行期间（约 25 秒）会接管鼠标和键盘，请勿操作桌面。

用法：
  pwsh -File scripts\gui_e2e_features_test.ps1 [-Exe <pixshot.exe>] [-SelX 300 -SelY 300 -SelW 600 -SelH 400]

输出：
  - PASS/FAIL 检查清单；退出码 0 = 全部通过
  - e2e_artifacts\features_final.png   全流程结束状态存档
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\debug\pixshot.exe'),
    [int]$SelX = 300,
    [int]$SelY = 300,
    [int]$SelW = 600,
    [int]$SelH = 400
)

$env:PIXSHOT_NO_UI_ALERTS = '1'
$env:PIXSHOT_ALLOW_INJECTED = '1'
# 冻结层可被屏幕截图捕获（正常使用下它是排除出捕获的），用于像素级回归
$env:PIXSHOT_OVERLAY_CAPTURABLE = '1'
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "找不到 $Exe，请先执行 cargo build" }
$art = Join-Path $PSScriptRoot '..\e2e_artifacts'
New-Item -ItemType Directory -Force -Path $art | Out-Null
try { Start-Transcript -Path (Join-Path $art 'e2e_features_transcript.txt') -Force | Out-Null } catch { }
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Add-Type -AssemblyName System.Drawing
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class Win32a {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, UIntPtr e);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte sc, uint f, UIntPtr e);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "FindWindowW")] public static extern IntPtr FindWindowClass(string c, IntPtr t);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] public static extern uint GetDpiForSystem();
    [DllImport("user32.dll")] public static extern IntPtr PostMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool CloseClipboard();
    [DllImport("user32.dll")] public static extern bool EmptyClipboard();
    [DllImport("user32.dll")] public static extern bool IsClipboardFormatAvailable(uint fmt);
    [DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint fmt);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr hmem);
    [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr hmem);
}
"@

[void][Win32a]::SetProcessDpiAwarenessContext([IntPtr](-4))
$dpi = [Win32a]::GetDpiForSystem()
$sw = [Win32a]::GetSystemMetrics(0)
$sh = [Win32a]::GetSystemMetrics(1)
Write-Host ("环境：屏幕 {0}x{1} 物理像素，系统 DPI {2}（{3}%）" -f $sw, $sh, $dpi, [int]($dpi / 96 * 100))
if (($SelX + $SelW) -ge $sw -or ($SelY + $SelH) -ge $sh) { throw "选区超出屏幕范围" }

# ─── 辅助函数 ─────────────────────────────────────────────
function Wait-Window([string]$Class, [int]$TimeoutMs = 5000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        $h = [Win32a]::FindWindowClass($Class, [IntPtr]::Zero)
        if ($h -ne [IntPtr]::Zero) { return $h }
        if ([Environment]::TickCount -ge $deadline) { return [IntPtr]::Zero }
        Start-Sleep -Milliseconds 80
    }
}
function Get-Rect2([IntPtr]$H) { $r = New-Object Win32a+RECT; [void][Win32a]::GetWindowRect($H, [ref]$r); $r }
function Get-Center2([IntPtr]$H) { $r = Get-Rect2 $H; @([int](($r.L + $r.R) / 2), [int](($r.T + $r.B) / 2)) }
function Mouse([uint32]$F) { [Win32a]::mouse_event($F, 0, 0, 0, [UIntPtr]::Zero) }
function Click-At([int]$X, [int]$Y) {
    [void][Win32a]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 90
    Mouse 0x02; Start-Sleep -Milliseconds 50; Mouse 0x04; Start-Sleep -Milliseconds 140
}
function DoubleClick-At([int]$X, [int]$Y) {
    [void][Win32a]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 80
    Mouse 0x02; Start-Sleep -Milliseconds 40; Mouse 0x04; Start-Sleep -Milliseconds 40
    Mouse 0x02; Start-Sleep -Milliseconds 40; Mouse 0x04; Start-Sleep -Milliseconds 250
}
function Ctrl-Wheel-At([int]$X, [int]$Y, [int]$Delta) {
    [void][Win32a]::SetCursorPos($X, $Y); Start-Sleep -Milliseconds 80
    $dw = if ($Delta -lt 0) { [uint32]([int64]4294967296 + [int64]$Delta) } else { [uint32]$Delta }
    [Win32a]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 50
    [Win32a]::mouse_event(0x0800, 0, 0, $dw, [UIntPtr]::Zero)
    Start-Sleep -Milliseconds 80
    [Win32a]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 150
}
function Drag-Mouse([int]$X1, [int]$Y1, [int]$X2, [int]$Y2) {
    [void][Win32a]::SetCursorPos($X1, $Y1); Start-Sleep -Milliseconds 160
    Mouse 0x02
    $n = 14
    for ($i = 1; $i -le $n; $i++) {
        [void][Win32a]::SetCursorPos(($X1 + [int](($X2 - $X1) * $i / $n)), ($Y1 + [int](($Y2 - $Y1) * $i / $n)))
        Start-Sleep -Milliseconds 30
    }
    Start-Sleep -Milliseconds 90
    Mouse 0x04; Start-Sleep -Milliseconds 140
}
function Press-Key([byte]$Vk) {
    [Win32a]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 110
}
function Press-Ctrl([byte]$Vk) {
    [Win32a]::keybd_event(0x11, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event($Vk, 0, 0, [UIntPtr]::Zero); Start-Sleep -Milliseconds 50
    [Win32a]::keybd_event($Vk, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 40
    [Win32a]::keybd_event(0x11, 0, 2, [UIntPtr]::Zero); Start-Sleep -Milliseconds 180
}
function Wait-WindowGone([string]$Class, [int]$TimeoutMs = 4000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ([Environment]::TickCount -lt $deadline) {
        if ([Win32a]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero) { return $true }
        Start-Sleep -Milliseconds 80
    }
    ([Win32a]::FindWindowClass($Class, [IntPtr]::Zero) -eq [IntPtr]::Zero)
}
function Find-ChildById([IntPtr]$Parent, [int]$Id) {
    $c = [Win32a]::GetWindow($Parent, 5)
    while ($c -ne [IntPtr]::Zero) {
        if ([Win32a]::GetDlgCtrlID($c) -eq $Id) { return $c }
        $c = [Win32a]::GetWindow($c, 2)
    }
    [IntPtr]::Zero
}
function Start-Overlay([int]$Attempts = 3, [int]$TimeoutMs = 8000) {
    # 程序启动后热键注册可能略晚于主窗口出现：首次 F1 可能丢失，这里重试几次
    for ($i = 0; $i -lt $Attempts; $i++) {
        Press-Key 0x70
        Start-Sleep -Milliseconds 400
        $o = Wait-Window 'PixShotOverlay' $TimeoutMs
        if ($o -ne [IntPtr]::Zero) { return $o }
        Start-Sleep -Milliseconds 300
    }
    [IntPtr]::Zero
}
function Invoke-Capture {
    $o = Start-Overlay
    if ($o -eq [IntPtr]::Zero) { return [IntPtr]::Zero }
    Start-Sleep -Milliseconds 500
    Drag-Mouse $SelX $SelY ($SelX + $SelW) ($SelY + $SelH)
    return (Wait-Window 'PixShotToolbar' 6000)
}
function Get-ChildIds([IntPtr]$Parent) {
    $ids = New-Object System.Collections.ArrayList
    $c = [Win32a]::GetWindow($Parent, 5)
    while ($c -ne [IntPtr]::Zero) {
        [void]$ids.Add([Win32a]::GetDlgCtrlID($c))
        $c = [Win32a]::GetWindow($c, 2)
    }
    @($ids)
}
function Wait-ToastText([string]$Match, [int]$Ms = 2500) {
    $deadline = [Environment]::TickCount + $Ms
    while ([Environment]::TickCount -lt $deadline) {
        $t = [Win32a]::FindWindowClass('PixShotToast', [IntPtr]::Zero)
        if ($t -ne [IntPtr]::Zero) {
            $txt = Get-TipText $t
            if ($txt -match $Match) { return $txt }
        }
        Start-Sleep -Milliseconds 50
    }
    ''
}
function Wait-ToastValueChanged([string]$Pattern, [int]$PrevValue, [int]$Ms = 2500) {
    $deadline = [Environment]::TickCount + $Ms
    while ([Environment]::TickCount -lt $deadline) {
        $t = [Win32a]::FindWindowClass('PixShotToast', [IntPtr]::Zero)
        if ($t -ne [IntPtr]::Zero) {
            $txt = Get-TipText $t
            if (($txt -match $Pattern) -and ([int]($txt -replace '\D', '') -ne $PrevValue)) { return $txt }
        }
        Start-Sleep -Milliseconds 50
    }
    ''
}
function Get-Pixel([int]$X, [int]$Y) {
    $bmp = New-Object System.Drawing.Bitmap(1, 1)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size(1, 1)))
    $g.Dispose()
    $c = $bmp.GetPixel(0, 0); $bmp.Dispose(); $c
}
function Test-Redish($C) { ($C.R -gt 150) -and ($C.G -lt 120) -and ($C.B -lt 120) }
function Test-Bluish($C) { ($C.B -gt 150) -and ($C.R -lt 120) -and ($C.G -lt 170) }
function Test-ClipDib {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            try { return [Win32a]::IsClipboardFormatAvailable(8) }
            finally { [void][Win32a]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
}
# 剪贴板 CF_DIB 中是否存在"矩形红"像素（R>200,G<90,B<90）—— 验证合成图确实含标注。
function Test-ClipDibHasRed {
    for ($i = 0; $i -lt 10; $i++) {
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            try {
                if (-not [Win32a]::IsClipboardFormatAvailable(8)) { return $false }
                $h = [Win32a]::GetClipboardData(8)
                if ($h -eq [IntPtr]::Zero) { return $false }
                $p = [Win32a]::GlobalLock($h)
                if ($p -eq [IntPtr]::Zero) { return $false }
                try {
                    $biSize = [Runtime.InteropServices.Marshal]::ReadInt32($p, 0)
                    $w = [Runtime.InteropServices.Marshal]::ReadInt32($p, 4)
                    $hh = [Runtime.InteropServices.Marshal]::ReadInt32($p, 8)
                    $bits = [Runtime.InteropServices.Marshal]::ReadInt16($p, 14)
                    if ($bits -ne 32) { Write-Host ("  [调试] CF_DIB 位深={0}（预期 32）" -f $bits); return $false }
                    $off = $biSize; if ($off -lt 40) { $off = 40 }
                    $aw = [Math]::Abs($w); $ah = [Math]::Abs($hh)
                    $len = $aw * $ah * 4
                    # 一次拷出像素块，再在内存数组里扫描（逐字节 Marshal 读取过慢）
                    $buf = New-Object byte[] $len
                    [Runtime.InteropServices.Marshal]::Copy([IntPtr]::Add($p, $off), $buf, 0, $len)
                    for ($k = 0; $k -lt $len; $k += 4) {
                        # BGRA：buf[k]=B, [k+1]=G, [k+2]=R
                        if ($buf[$k + 2] -gt 200 -and $buf[$k + 1] -lt 90 -and $buf[$k] -lt 90) { return $true }
                    }
                    return $false
                } finally { [void][Win32a]::GlobalUnlock($h) }
            } finally { [void][Win32a]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
}
function Shot([int]$X, [int]$Y, [int]$W, [int]$H, [string]$File) {
    $bmp = New-Object System.Drawing.Bitmap($W, $H)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.CopyFromScreen($X, $Y, 0, 0, (New-Object System.Drawing.Size($W, $H)))
    $g.Dispose(); $bmp.Save($File, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    Write-Host "  [存档] $File"
}

$script:Pass = 0; $script:Fail = 0
function Check([string]$Name, $Ok, [string]$Detail = '') {
    $isOk = $false
    if ($Ok -is [bool]) { $isOk = $Ok }
    else {
        $t = if ($null -eq $Ok) { 'null' } else { $Ok.GetType().Name }
        Write-Host ("  [断言告警]「{0}」的判定值不是布尔（实际 {1}），已按失败处理" -f $Name, $t) -ForegroundColor Yellow
    }
    if ($isOk) { $script:Pass++ } else { $script:Fail++ }
    Write-Host ("  [{0}] {1}{2}" -f $(if ($isOk) { 'PASS' } else { 'FAIL' }), $Name, $(if ($Detail) { " -- $Detail" } else { '' }))
}

# 矩形四边中点附近（避开选区红框，距离边界 ≥ 40px）
$rectP1 = @(($SelX + 60), ($SelY + 60))
$rectP2 = @(($SelX + 260), ($SelY + 200))
$rectEdgeProbe = @(($SelX + 60), ($SelY + 130))   # 矩形左边中点
# 步骤序号落点与探针（探针偏离圆心，避开圆心处的白色数字）
$stepClick = @(($SelX + 380), ($SelY + 120))
$stepProbe = @(($SelX + 380 - 7), ($SelY + 120))


# ─── 新功能：颜色面板 / 橡皮·水印按钮 / 贴图锁定与透明度 ───
Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class TipW {
    public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lp);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "GetClassNameW")]
    public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode, EntryPoint = "GetWindowTextW")]
    public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
}
"@
function Get-TipWindows {
    $list = New-Object System.Collections.ArrayList
    $cb = [TipW+EnumWindowsProc]{
        param([IntPtr]$h, [IntPtr]$l)
        if ([Win32a]::IsWindowVisible($h)) {
            $sb = New-Object System.Text.StringBuilder 256
            [void][TipW]::GetClassNameW($h, $sb, 256)
            if ($sb.ToString() -eq 'PixShotTip') { [void]$list.Add($h) }
        }
        return $true
    }
    [void][TipW]::EnumWindows($cb, [IntPtr]::Zero)
    @($list)
}
function Get-TipText([IntPtr]$H) {
    $sb = New-Object System.Text.StringBuilder 512
    [void][TipW]::GetWindowTextW($H, $sb, 512)
    $sb.ToString()
}
function Wait-TipVisible([int]$Ms = 2500) {
    $deadline = [Environment]::TickCount + $Ms
    while ([Environment]::TickCount -lt $deadline) {
        $tips = @(Get-TipWindows)
        if ($tips.Count -gt 0) { return $tips }
        Start-Sleep -Milliseconds 60
    }
    @()
}
function Get-MenuItemCenter2([IntPtr]$Menu, [int]$Index) {
    $d = [math]::Max([int][Win32a]::GetDpiForSystem(), [int]$dpi)
    $s = $d / 96.0
    $pad = 6; $itemH = 34; $sep = 9
    $sepBefore = @(3, 7)   # 重置缩放、关闭贴图前有分隔线
    $top = $pad
    $centerY = $top
    for ($i = 0; $i -le $Index; $i++) {
        if ($i -gt 0 -and ($sepBefore -contains $i)) { $top += $sep }
        if ($i -eq $Index) { $centerY = $top + [int]($itemH / 2); break }
        $top += $itemH
    }
    $r = Get-Rect2 $Menu
    @([int](($r.L + $r.R) / 2), [int]($r.T + [math]::Round($centerY * $s)))
}
function Open-PinMenu([IntPtr]$Pin) {
    $c = Get-Center2 $Pin
    [void][Win32a]::SetCursorPos($c[0], $c[1]); Start-Sleep -Milliseconds 90
    Mouse 0x08; Start-Sleep -Milliseconds 50; Mouse 0x10; Start-Sleep -Milliseconds 200
    Wait-Window 'PixShotMenu' 2500
}
function Select-PinMenuItem([IntPtr]$Pin, [int]$Index) {
    $menu = Open-PinMenu $Pin
    if ($menu -eq [IntPtr]::Zero) { return $false }
    Start-Sleep -Milliseconds 150
    $pt = Get-MenuItemCenter2 $menu $Index
    Click-At $pt[0] $pt[1]
    Start-Sleep -Milliseconds 300
    return $true
}

$proc = $null
try {
    $proc = Start-Process -FilePath $Exe -PassThru
    $mainH = Wait-Window 'PixShotMain' 10000
    Start-Sleep -Milliseconds 800
    if ($mainH -eq [IntPtr]::Zero) { throw '主窗口未创建' }

    # ── 智能选区：不拖动、单击即自动扩充为光标下窗口（默认 pick_level=region）──
    $ov = Start-Overlay
    if ($ov -ne [IntPtr]::Zero) {
        Start-Sleep -Milliseconds 500
        $probeX = $SelX + $SelW / 2
        $probeY = $SelY + $SelH / 2
        [void][Win32a]::SetCursorPos($probeX, $probeY)
        Start-Sleep -Milliseconds 250
        $px = Get-Pixel $probeX $probeY
        # 清空剪贴板后按 C（取色复制）
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            [void][Win32a]::EmptyClipboard(); [void][Win32a]::CloseClipboard()
        }
        Press-Key 0x43
        Start-Sleep -Milliseconds 300
        $colorText = ''
        if ([Win32a]::OpenClipboard([IntPtr]::Zero)) {
            try {
                if ([Win32a]::IsClipboardFormatAvailable(13)) {
                    $ch = [Win32a]::GetClipboardData(13)
                    $cp = [Win32a]::GlobalLock($ch)
                    if ($cp -ne [IntPtr]::Zero) {
                        $colorText = [Runtime.InteropServices.Marshal]::PtrToStringUni($cp)
                        [void][Win32a]::GlobalUnlock($ch)
                    }
                }
            } finally { [void][Win32a]::CloseClipboard() }
        }
        $expect = ('#{0:X2}{1:X2}{2:X2}' -f $px.R, $px.G, $px.B)
        # 放大镜/辅助层会覆盖光标像素，屏幕采样值不等于快照取样值；这里校验格式与成功复制。
        Check 'C 键取色复制到剪贴板（Hex RGB 格式）' `
            ($colorText -match '^#[0-9A-F]{6}$') ("剪贴板='{0}' 屏幕采样='{1}'" -f $colorText, $expect)
        Click-At $probeX $probeY
        $tbAuto = Wait-Window 'PixShotToolbar' 5000
        Check '单击（不拖动）自动扩充并进入标注' ($tbAuto -ne [IntPtr]::Zero)
        if ($tbAuto -ne [IntPtr]::Zero) {
            $bClose = Find-ChildById $tbAuto 5
            if ($bClose -ne [IntPtr]::Zero) {
                $bc = Get-Center2 $bClose
                Click-At $bc[0] $bc[1]
            }
            Check '关闭自动扩充会话' (Wait-WindowGone 'PixShotToolbar' 3000)
        }
    } else {
        Check 'F1 触发遮罩' $false '遮罩未出现'
    }

    $tb = Invoke-Capture
    for ($try = 1; $try -le 3 -and $tb -eq [IntPtr]::Zero; $try++) { $tb = Invoke-Capture }
    Check '框选完成 -> 工具条弹出' ($tb -ne [IntPtr]::Zero)
    if ($tb -eq [IntPtr]::Zero) { throw '工具条未出现' }

    Check '工具条含「更多」按钮（ID 18）' ((Find-ChildById $tb 18) -ne [IntPtr]::Zero)
    Check '工具条含颜色面板按钮（ID 17）' ((Find-ChildById $tb 17) -ne [IntPtr]::Zero)
    # ── 悬停「更多」超过 1 秒 → 子工具条弹出，只含马赛克/画笔/高亮/橡皮/水印 ──
    $bMore = Find-ChildById $tb 18
    if ($bMore -ne [IntPtr]::Zero) {
        $bm = Get-Center2 $bMore
        [void][Win32a]::SetCursorPos($bm[0], $bm[1])
        Start-Sleep -Milliseconds 1400
        $sub = Wait-Window 'PixShotToolbarMore' 2000
        Check '悬停「更多」超过 1 秒 -> 子工具条弹出' `
            (($sub -ne [IntPtr]::Zero) -and [Win32a]::IsWindowVisible($sub))
        if ($sub -ne [IntPtr]::Zero) {
            foreach ($id in 9, 14, 10, 15, 16) {
                Check "更多子工具条显示按钮（ID $id）" `
                    ([Win32a]::IsWindowVisible((Find-ChildById $sub $id)))
            }
            # 椭圆/线段/文字已分别移到矩形、箭头、文字按钮的子工具条
            foreach ($id in 11, 12, 13) {
                Check "更多子工具条不再显示按钮（ID $id）" `
                    (-not [Win32a]::IsWindowVisible((Find-ChildById $sub $id)))
            }
            # 主工具条不再直接展示椭圆/线段按钮
            Check '主工具条不再直接展示椭圆按钮（ID 12）' ((Find-ChildById $tb 12) -eq [IntPtr]::Zero)
        }
        # 光标移开 → 子工具条自动收起
        [void][Win32a]::SetCursorPos(($bm[0] - 200), $bm[1])
        Start-Sleep -Milliseconds 700
        Check '光标离开「更多」后子工具条收起' `
            (-not [Win32a]::IsWindowVisible($sub))
    }

    # ── 工具条分组：OCR（功能组末尾）与矩形（标注组首个）之间应留出竖杠占位 ──
    $bOcr = Find-ChildById $tb 4
    $bRectA = Find-ChildById $tb 6
    $bCopyA = Find-ChildById $tb 2
    if (($bOcr -ne [IntPtr]::Zero) -and ($bRectA -ne [IntPtr]::Zero) -and ($bCopyA -ne [IntPtr]::Zero)) {
        $rOcr = Get-Rect2 $bOcr
        $rRectA = Get-Rect2 $bRectA
        $rCopyA = Get-Rect2 $bCopyA
        $sepGap = $rRectA.L - $rOcr.R
        $normalGap = $rOcr.L - $rCopyA.R
        Check '标注按钮组与功能按钮组之间有分组竖杠间距' `
            ($sepGap -gt $normalGap) ("分组间距={0}px 常规间距={1}px" -f $sepGap, $normalGap)
    }

    # ── Ctrl+滚轮调整标注尺寸：选中工具后光标无需悬停在按钮上；Toast 实时刷新 ──
    $canvasPt = @([int]($SelX + $SelW / 2), [int]($SelY + $SelH / 2))
    Press-Key 0x52  # R -> 选中矩形工具
    Start-Sleep -Milliseconds 250
    Ctrl-Wheel-At $canvasPt[0] $canvasPt[1] 120
    $toast1 = Wait-ToastText '线宽' 2500
    Check '选中矩形后，光标在画布上 Ctrl+滚轮即可调线宽' `
        ($toast1 -match '线宽') ("toast='{0}'" -f $toast1)
    $line1 = [int]($toast1 -replace '\D', '')
    Ctrl-Wheel-At $canvasPt[0] $canvasPt[1] 120
    $toast2 = Wait-ToastValueChanged '线宽' $line1 2500
    $line2 = [int]($toast2 -replace '\D', '')
    Check '连续调整时 Toast 实时刷新为递增后的当前值' `
        ($line2 -eq ($line1 + 1)) ("第一次='{0}' 第二次='{1}'" -f $toast1, $toast2)

    Press-Key 0x54  # T -> 选中文字工具
    Start-Sleep -Milliseconds 250
    Ctrl-Wheel-At $canvasPt[0] $canvasPt[1] 120
    $toast3 = Wait-ToastText '字号' 2500
    Check '选中文字后，光标在画布上 Ctrl+滚轮即可调字号' `
        ($toast3 -match '字号') ("toast='{0}'" -f $toast3)

    # ── 提示气泡单例：旧气泡未消失时移到另一个按钮，只保留最新文本 ──
    $bSave = Find-ChildById $tb 3
    if ($bSave -ne [IntPtr]::Zero) {
        $sp = Get-Center2 $bSave
        [void][Win32a]::SetCursorPos($sp[0], $sp[1]); Start-Sleep -Milliseconds 250
        [void][Win32a]::SetCursorPos($sp[0] + 1, $sp[1])
        $tips1 = @(Wait-TipVisible 2500)
        Check '悬停「保存」出现唯一 tip' ($tips1.Count -eq 1)
    }
    $bRect2 = Find-ChildById $tb 6
    if ($bRect2 -ne [IntPtr]::Zero) {
        $rp = Get-Center2 $bRect2
        [void][Win32a]::SetCursorPos($rp[0], $rp[1]); Start-Sleep -Milliseconds 250
        [void][Win32a]::SetCursorPos($rp[0] + 1, $rp[1])
        $tips2 = @()
        $deadline = [Environment]::TickCount + 2500
        while ([Environment]::TickCount -lt $deadline) {
            $tips2 = @(Get-TipWindows)
            if ($tips2.Count -eq 1) {
                $t = Get-TipText $tips2[0]
                if ($t -match '矩形') { break }
            }
            Start-Sleep -Milliseconds 60
        }
        $tipText = if ($tips2.Count -gt 0) { Get-TipText $tips2[0] } else { '' }
        Check '切换到新按钮后旧 tip 被替换（仅显示最新一个）' `
            (($tips2.Count -eq 1) -and ($tipText -match '矩形')) ("可见 tip={0} 文本='{1}'" -f $tips2.Count, $tipText)
    }

    # ── 空快捷键 tip：悬停「颜色面板」按钮不应崩溃 ──
    # 回归背景：空快捷键后缀曾以空 Vec 的悬垂指针传给 DrawTextW(-1)，触发访问冲突。
    $b17tip = Find-ChildById $tb 17
    if ($b17tip -ne [IntPtr]::Zero) {
        $bp17 = Get-Center2 $b17tip
        [void][Win32a]::SetCursorPos($bp17[0], $bp17[1]); Start-Sleep -Milliseconds 250
        [void][Win32a]::SetCursorPos($bp17[0] + 1, $bp17[1])
        $tips17 = @()
        $deadline17 = [Environment]::TickCount + 2500
        while ([Environment]::TickCount -lt $deadline17) {
            $tips17 = @(Get-TipWindows)
            if ($tips17.Count -ge 1 -and ((Get-TipText $tips17[0]) -match '颜色面板')) { break }
            Start-Sleep -Milliseconds 60
        }
        $tip17 = if ($tips17.Count -gt 0) { Get-TipText $tips17[0] } else { '' }
        Check '悬停「颜色面板」提示气泡不崩溃且文案正确' `
            (($tip17 -match '颜色面板') -and (-not $proc.HasExited)) ("tip='{0}'" -f $tip17)
    }

    # ── 颜色面板：打开 -> 点色块 -> 关闭 ──
    $b17 = Find-ChildById $tb 17
    $rc17 = Get-Center2 $b17
    Click-At $rc17[0] $rc17[1]
    $panel = Wait-Window 'PixShotStylePanel' 3000
    Check '点击颜色面板按钮 -> 面板弹出' ($panel -ne [IntPtr]::Zero)
    if ($panel -ne [IntPtr]::Zero) {
        $tr = Get-Rect2 $tb
        $pr = Get-Rect2 $panel
        Check '颜色面板位于工具条下方并与工具条右侧对齐' `
            (($pr.T -gt $tr.B) -and ([Math]::Abs($pr.R - $tr.R) -le 2)) `
            ("工具条 right={0} bottom={1}；面板 top={2} right={3}" -f $tr.R, $tr.B, $pr.T, $pr.R)
        $c101 = Find-ChildById $panel 101
        Check '颜色面板含预设色块（ID 101）' ($c101 -ne [IntPtr]::Zero)
        $ids = @(Get-ChildIds $panel)
        $nonColor = @($ids | Where-Object { $_ -lt 101 -or $_ -gt 108 })
        Check '颜色面板仅含颜色控件（8 个色块，无其他控件）' `
            (($ids.Count -eq 8) -and ($nonColor.Count -eq 0)) ("子控件 ID={0}" -f ($ids -join ','))
        # 选色后应自动收起面板（不需要再点按钮）
        if ($c101 -ne [IntPtr]::Zero) {
            $cc = Get-Center2 $c101
            Click-At $cc[0] $cc[1]
            Check '点击色块后颜色面板自动隐藏' (Wait-WindowGone 'PixShotStylePanel' 2500)
        }
        # 自动收起后工具条句柄已清空：再次点按钮应能重新打开，再点一次关闭
        Click-At $rc17[0] $rc17[1]
        $panel2 = Wait-Window 'PixShotStylePanel' 3000
        Check '自动隐藏后再次点击颜色面板按钮 -> 重新弹出' ($panel2 -ne [IntPtr]::Zero)
        if ($panel2 -ne [IntPtr]::Zero) {
            Click-At $rc17[0] $rc17[1]
            Check '再次点击颜色面板按钮 -> 面板关闭' (Wait-WindowGone 'PixShotStylePanel' 2500)
        }
    }

    # ── 贴图锁定 / 透明度 / 关闭 ──
    Press-Ctrl 0x54
    $pin = Wait-Window 'PixShotPin' 6000
    Check 'Ctrl+T 创建贴图' ($pin -ne [IntPtr]::Zero)
    if ($pin -eq [IntPtr]::Zero) { throw '贴图未创建' }
    $before = Get-Rect2 $pin
    Check '菜单点击「锁定位置」' (Select-PinMenuItem $pin 4)
    $c = Get-Center2 $pin
    Drag-Mouse ($c[0] - 80) ($c[1] - 40) ($c[0] + 80) ($c[1] + 40)
    $locked = Get-Rect2 $pin
    Check '锁定后拖动贴图位置不变' (($locked.L -eq $before.L) -and ($locked.T -eq $before.T))

    Check '菜单点击「解锁位置」' (Select-PinMenuItem $pin 4)
    $c = Get-Center2 $pin
    Drag-Mouse ($c[0] - 80) ($c[1] - 40) ($c[0] + 80) ($c[1] + 40)
    $moved = Get-Rect2 $pin
    Check '解锁后贴图可拖动' (($moved.L -ne $locked.L) -or ($moved.T -ne $locked.T))

    # Ctrl+滚轮调透明度：窗口尺寸不变（证明没有落到普通滚轮缩放）
    $sizeBefore = Get-Rect2 $pin
    Ctrl-Wheel-At $c[0] $c[1] -120
    Ctrl-Wheel-At $c[0] $c[1] -120
    $sizeAfter = Get-Rect2 $pin
    Check 'Ctrl+滚轮调整透明度后贴图仍存在' ([Win32a]::IsWindow($pin))
    Check 'Ctrl+滚轮不改变贴图尺寸（未走普通滚轮缩放）' `
        ((($sizeAfter.R - $sizeAfter.L) -eq ($sizeBefore.R - $sizeBefore.L)) -and (($sizeAfter.B - $sizeAfter.T) -eq ($sizeBefore.B - $sizeBefore.T)))

    Check '菜单点击「关闭贴图」' (Select-PinMenuItem $pin 7)
    Check '关闭贴图后窗口销毁' (Wait-WindowGone 'PixShotPin' 2500)

    # ── 双击图像正文关闭贴图（HTCAPTION 后以非客户区双击到达）──
    $tb3 = Invoke-Capture
    if ($tb3 -ne [IntPtr]::Zero) {
        Press-Ctrl 0x54
        $pin2 = Wait-Window 'PixShotPin' 6000
        Check '再次创建贴图（供双击关闭）' ($pin2 -ne [IntPtr]::Zero)
        if ($pin2 -ne [IntPtr]::Zero) {
            $c2 = Get-Center2 $pin2
            DoubleClick-At $c2[0] $c2[1]
            Check '双击贴图图像区 -> 关闭贴图' (Wait-WindowGone 'PixShotPin' 2500)
        }
    }

    Shot 0 0 $sw $sh (Join-Path $art 'features_final.png')
} finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })
