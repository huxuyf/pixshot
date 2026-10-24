<#
PixShot GUI 端到端测试（消息驱动版 v2）
========================================
与 gui_e2e_test.ps1（物理鼠标/键盘版）覆盖同一核心流程，但通过
SendMessage/PostMessage 直接驱动各窗口的窗口过程；窗口查找采用
EnumWindows（按 PID + 类名），不依赖 FindWindowW。

  消息驱动框选截图 → 复制图像（剪贴板校验）→ 贴图置顶（位置/缩放/Esc 关闭验证）
  → OCR（结果窗口文本 + 复制文本剪贴板比对）

优点：不依赖物理输入注入，可在受限宿主（禁止 SetCursorPos/mouse_event 的沙箱）中运行，
同样逐条执行应用真实的窗口过程、截屏、贴图、OCR 与剪贴板逻辑。
未覆盖（需物理输入，请用 gui_e2e_test.ps1）：贴图拖动、右键菜单、保存对话框。

用法：
  pwsh -File scripts\gui_e2e_msg_test.ps1 [-Exe target\release\pixshot.exe]
输出：
  PASS/FAIL 清单；e2e_artifacts\e2e_msg_transcript.txt 与 pin_visible.png 存档
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\pixshot.exe')
)

$env:PIXSHOT_NO_UI_ALERTS = '1'   # 沙箱下托盘注册必失败，屏蔽启动提示框以保证测试确定性
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Exe)) { throw "找不到 $Exe，请先执行 cargo build --release" }
$art = Join-Path $PSScriptRoot '..\e2e_artifacts'
New-Item -ItemType Directory -Force -Path $art | Out-Null
try { Start-Transcript -Path (Join-Path $art 'e2e_msg_transcript.txt') -Force | Out-Null } catch { }
Get-Process pixshot -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 400

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class W {
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    // 本脚本一律用 EnumWindows 按 PID + 类名查找窗口，
    // 不提供 FindWindowW（把 $null 传给它的标题参数会被 PowerShell 转成 ""，
    // 语义变成「标题必须为空」，是个静默踩坑点）。
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern int GetWindowLongW(IntPtr h, int i);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder sb, int max);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    // 剪贴板
    [DllImport("user32.dll")] public static extern bool OpenClipboard(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool CloseClipboard();
    [DllImport("user32.dll")] public static extern bool IsClipboardFormatAvailable(uint fmt);
    [DllImport("user32.dll")] public static extern IntPtr GetClipboardData(uint fmt);
    [DllImport("kernel32.dll")] public static extern IntPtr GlobalLock(IntPtr hmem);
    [DllImport("kernel32.dll")] public static extern bool GlobalUnlock(IntPtr hmem);
}
"@
[void][W]::SetProcessDpiAwarenessContext([IntPtr](-4))

# WinForms（文字靶标窗口用）
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$form = $null

# ── 常量 ──
Set-Variable -Name 'WM_KEYDOWN' -Value 0x0100 -Option Constant
Set-Variable -Name 'WM_CLOSE' -Value 0x0010 -Option Constant
Set-Variable -Name 'WM_LBUTTONDOWN' -Value 0x0201 -Option Constant
Set-Variable -Name 'WM_MOUSEMOVE' -Value 0x0200 -Option Constant
Set-Variable -Name 'WM_MOUSELEAVE' -Value 0x02A3 -Option Constant   # 工具条图标按钮的悬停离开
Set-Variable -Name 'WM_LBUTTONUP' -Value 0x0202 -Option Constant
Set-Variable -Name 'WM_MOUSEWHEEL' -Value 0x020A -Option Constant
Set-Variable -Name 'WM_CONTEXTMENU' -Value 0x007B -Option Constant
Set-Variable -Name 'BM_CLICK' -Value 0x00F5 -Option Constant

function LP([int]$x, [int]$y) { [IntPtr](($x -band 0xFFFF) -bor (($y -band 0xFFFF) -shl 16)) }

# 枚举某 PID 的所有顶层窗口（返回 @{句柄=类名} 列表）
function Enum-WindowsOfPid([int]$TargetPid) {
    $script:enumPid = $TargetPid
    $script:enumHits = New-Object System.Collections.Generic.List[object]
    $cb = [W+EnumProc]{ param($h, $l)
        $wpid = 0
        [void][W]::GetWindowThreadProcessId($h, [ref]$wpid)
        if ($wpid -eq $script:enumPid) {
            $sb = New-Object System.Text.StringBuilder 256
            [void][W]::GetClassNameW($h, $sb, 256)
            $script:enumHits.Add([pscustomobject]@{ Hwnd = $h; Class = $sb.ToString() })
        }
        return $true
    }
    [void][W]::EnumWindows($cb, [IntPtr]::Zero)
    $script:enumHits
}

# 按 PID + 类名查找窗口（EnumWindows 实现，不依赖 FindWindowW）
function Find-WindowByEnum([int]$TargetPid, [string]$Class) {
    foreach ($w in (Enum-WindowsOfPid $TargetPid)) {
        if ($w.Class -eq $Class) { return $w.Hwnd }
    }
    [IntPtr]::Zero
}

function Wait-WindowByEnum([int]$TargetPid, [string]$Class, [int]$TimeoutMs = 12000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        $h = Find-WindowByEnum $TargetPid $Class
        if ($h -ne [IntPtr]::Zero) { return $h }
        if ([Environment]::TickCount -ge $deadline) { return [IntPtr]::Zero }
        [void][System.Windows.Forms.Application]::DoEvents()   # 保持靶标窗口消息泵
        Start-Sleep -Milliseconds 100
    }
}
# 等待某 PID 的指定类名窗口消失（用于「工具条已关闭」这类异步销毁断言）
function Wait-WindowGone([int]$TargetPid, [string]$Class, [int]$TimeoutMs = 5000) {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        if ((Find-WindowByEnum $TargetPid $Class) -eq [IntPtr]::Zero) { return $true }
        if ([Environment]::TickCount -ge $deadline) { return $false }
        [void][System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 100
    }
}
function Get-Rect([IntPtr]$H) {
    $r = New-Object W+RECT
    [void][W]::GetWindowRect($H, [ref]$r); $r
}
function Get-Text([IntPtr]$H) {
    $sb = New-Object System.Text.StringBuilder 8192
    [void][W]::GetWindowTextW($H, $sb, 8192); $sb.ToString()
}
# 跨进程读取控件真实文本（EDIT/STATIC 必须用 WM_GETTEXT，GetWindowTextW 读到的是标题缓存）
function Get-ControlText([IntPtr]$H) {
    $len = [W]::SendMessageW($H, 0x000E, [IntPtr]::Zero, [IntPtr]::Zero).ToInt32()   # WM_GETTEXTLENGTH
    if ($len -lt 0) { $len = 0 }
    $buf = [Runtime.InteropServices.Marshal]::AllocHGlobal((($len + 1) * 2))
    try {
        [void][W]::SendMessageW($H, 0x000D, [IntPtr]($len + 1), $buf)                 # WM_GETTEXT
        [Runtime.InteropServices.Marshal]::PtrToStringUni($buf, $len)
    } finally { [Runtime.InteropServices.Marshal]::FreeHGlobal($buf) }
}
# 枚举父窗口的 STATIC 子控件文本
function Get-StaticTexts([IntPtr]$Parent) {
    $out = New-Object System.Collections.Generic.List[string]
    $c = [W]::GetWindow($Parent, 5)
    while ($c -ne [IntPtr]::Zero) {
        $sb = New-Object System.Text.StringBuilder 64
        [void][W]::GetClassNameW($c, $sb, 64)
        if ($sb.ToString() -eq 'STATIC') { $out.Add((Get-ControlText $c)) }
        $c = [W]::GetWindow($c, 2)
    }
    $out
}
function Find-ChildById([IntPtr]$Parent, [int]$Id) {
    $c = [W]::GetWindow($Parent, 5)          # GW_CHILD
    while ($c -ne [IntPtr]::Zero) {
        if ([W]::GetDlgCtrlID($c) -eq $Id) { return $c }
        $c = [W]::GetWindow($c, 2)           # GW_HWNDNEXT
    }
    [IntPtr]::Zero
}
function Get-ClipText {
    for ($i = 0; $i -lt 10; $i++) {
        if ([W]::OpenClipboard([IntPtr]::Zero)) {
            $h = [IntPtr]::Zero          # 预置，供 finally 安全引用
            try {
                if (-not [W]::IsClipboardFormatAvailable(13)) { return '' }
                $h = [W]::GetClipboardData(13)
                if ($h -eq [IntPtr]::Zero) { return '' }
                $p = [W]::GlobalLock($h)
                if ($p -eq [IntPtr]::Zero) { return '' }
                $len = 0
                while ($len -lt 200000 -and [Runtime.InteropServices.Marshal]::ReadInt16($p, $len * 2) -ne 0) { $len++ }
                # 必须 return，否则 for 会继续跑满 10 轮，函数返回 11 个元素的数组。
                return [Runtime.InteropServices.Marshal]::PtrToStringUni($p, $len)
            } finally {
                [void][W]::GlobalUnlock($h)
                [void][W]::CloseClipboard()
            }
        }
        Start-Sleep -Milliseconds 40
    }
    return ''
}
# 等待剪贴板出现文本（OCR 完成判定；CF_UNICODETEXT = 13）。
# NotEqual 用于排除点击前已有的旧文本，确保读到的是本次写入的内容。
function Wait-ClipText([int]$TimeoutMs = 25000, [string]$NotEqual = '') {
    $deadline = [Environment]::TickCount + $TimeoutMs
    while ($true) {
        $t = Get-ClipText
        if ($t -and $t.Trim().Length -gt 0 -and $t -ne $NotEqual) { return $t }
        if ([Environment]::TickCount -ge $deadline) { return '' }
        [void][System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 200
    }
}
function Test-ClipDib {
    for ($i = 0; $i -lt 10; $i++) {
        if ([W]::OpenClipboard([IntPtr]::Zero)) {
            try { return [W]::IsClipboardFormatAvailable(8) }
            finally { [void][W]::CloseClipboard() }
        }
        Start-Sleep -Milliseconds 40
    }
    return $false
}

$script:Pass = 0; $script:Fail = 0
function Check([string]$Name, $Ok, [string]$Detail = '') {
    # 严格布尔判定（与物理驱动版一致）：PowerShell 的 -eq 作用于集合时返回筛选结果
    # 而非布尔，非空数组在 if 中恒为真，会造成自相矛盾的 PASS 记录。
    $isOk = $false
    if ($Ok -is [bool]) {
        $isOk = $Ok
    } else {
        $t = if ($null -eq $Ok) { 'null' } else { $Ok.GetType().Name }
        Write-Host ("  [断言告警]「{0}」的判定值不是布尔（实际 {1}），已按失败处理" -f $Name, $t) -ForegroundColor Yellow
    }
    if ($isOk) { $script:Pass++ } else { $script:Fail++ }
    Write-Host ("  [{0}] {1}{2}" -f $(if ($isOk) { 'PASS' } else { 'FAIL' }), $Name, $(if ($Detail) { " -- $Detail" } else { '' }))
}

Write-Host ''
Write-Host '==== PixShot E2E（消息驱动版 v3）：工具条结构 -> 复制/OCR/贴图 均关闭工具条 ===='
Write-Host ''

# 框选区域（虚拟屏幕坐标）
$SelX = 300; $SelY = 300; $SelW = 600; $SelH = 400

$proc = $null
try {
    # ── 0. 文字靶标窗口（为 OCR 提供确定性内容，完全位于选区内）──
    $form = New-Object System.Windows.Forms.Form
    $form.StartPosition = 'Manual'
    # 靶标窗口精确覆盖框选区域：否则选区边缘会露出桌面/其它窗口，
    # 混入无关文字干扰 OCR 断言。
    $form.Location = New-Object System.Drawing.Point($SelX, $SelY)
    $form.Size = New-Object System.Drawing.Size($SelW, $SelH)
    $form.FormBorderStyle = 'None'
    $form.BackColor = [System.Drawing.Color]::White
    $form.TopMost = $true
    $lbl = New-Object System.Windows.Forms.Label
    $lbl.Text = "PixShot OCR E2E`n识别测试文本 ABC 12345"
    $lbl.Font = New-Object System.Drawing.Font('Microsoft YaHei', 22, [System.Drawing.FontStyle]::Bold)
    $lbl.ForeColor = [System.Drawing.Color]::Black
    $lbl.AutoSize = $false
    $lbl.Size = New-Object System.Drawing.Size(($SelW - 20), ($SelH - 20))
    $lbl.Location = New-Object System.Drawing.Point(10, 10)
    $form.Controls.Add($lbl)
    $form.Show()
    for ($i = 0; $i -lt 15; $i++) { [void][System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 50 }
    Write-Host '  [靶标] 已在选区内显示确定性文字窗口（PixShot OCR E2E / 识别测试文本 ABC 12345）'

    # ── 启动一次；后续每个场景各自触发一次全新截图（因为每个操作都会关闭工具条）──
    $proc = Start-Process -FilePath $Exe -PassThru
    $pid0 = $proc.Id
    $mainH = Wait-WindowByEnum $pid0 'PixShotMain' 10000
    if ($mainH -eq [IntPtr]::Zero) { throw '主窗口（托盘宿主）未创建' }
    Start-Sleep -Milliseconds 500
    # 启动后 0.5s 仍无遮罩 = 托盘驻留（未进入截图流程）
    Check '启动后常驻托盘（无遮罩/主窗口弹出）' ((Find-WindowByEnum $pid0 'PixShotOverlay') -eq [IntPtr]::Zero)

    # F1（WM_HOTKEY 路径，Post 异步投递）→ 遮罩 → 消息拖拽框选 → 返回工具条窗口
    # 由工具条几何反推本次拖拽得到的选区是否等于期望值。
    # 工具条贴在选区下方 8 逻辑像素：左边界 = 选区左边界，
    # 上边界 = 选区下边界 + scaled(8)；缩放系数由工具条高度（逻辑 104）反推。
    function Test-CaptureValid([IntPtr]$Tb) {
        if ($Tb -eq [IntPtr]::Zero) { return $false }
        $r = Get-Rect $Tb
        $dpi = [W]::GetDpiForWindow($Tb)
        if ($dpi -lt 96) { $dpi = 96 }
        $gap = [int][Math]::Round(8 * $dpi / 96.0)
        return (([Math]::Abs($r.L - $SelX) -le 1) -and ([Math]::Abs($r.T - ($SelY + $SelH + $gap)) -le 2))
    }

    # 关掉当前工具条（工具条已无关闭按钮，直接发 WM_CLOSE），用于选区不符时重来
    function Close-Toolbar([IntPtr]$Tb) {
        [void][W]::PostMessageW($Tb, $WM_CLOSE, [IntPtr]::Zero, [IntPtr]::Zero)
        [void](Wait-WindowGone $pid0 'PixShotToolbar' 4000)
    }

    function Invoke-Capture {
        # 消息驱动的拖拽偶发会被干扰成退化选区（0 尺寸或落到别的窗口），
        # 因此每次都用工具条几何校验选区，不符则关掉重来（最多 3 次）。
        for ($try = 1; $try -le 3; $try++) {
            # 先把靶标窗口重新置顶：其它置顶浮窗（编辑器/取色器等）可能盖住它，
            # 导致 OCR 读到的是背景内容而不是靶标文字。
            $form.TopMost = $false; $form.TopMost = $true
            [void][System.Windows.Forms.Application]::DoEvents()
            Start-Sleep -Milliseconds 150
            [void][W]::PostMessageW($mainH, 0x0312, [IntPtr]1, [IntPtr]0)
            $o = Wait-WindowByEnum $pid0 'PixShotOverlay' 12000
            if ($o -eq [IntPtr]::Zero) { continue }
            Start-Sleep -Milliseconds 200   # 等遮罩完成创建（WM_CREATE 设置状态指针）
            [void][W]::SendMessageW($o, $WM_LBUTTONDOWN, [IntPtr]1, (LP $SelX $SelY))
            foreach ($p in @(@(560, 480), @(($SelX + $SelW), ($SelY + $SelH)))) {
                [void][W]::SendMessageW($o, $WM_MOUSEMOVE, [IntPtr]1, (LP $p[0] $p[1]))
                Start-Sleep -Milliseconds 20
            }
            [void][W]::SendMessageW($o, $WM_LBUTTONUP, [IntPtr]0, (LP ($SelX + $SelW) ($SelY + $SelH)))
            $t = Wait-WindowByEnum $pid0 'PixShotToolbar' 6000
            if ($t -eq [IntPtr]::Zero) {
                # 未得到工具条：框选未生效（如拖动过小），Esc 关掉遮罩再来
                $o2 = Find-WindowByEnum $pid0 'PixShotOverlay'
                if ($o2 -ne [IntPtr]::Zero) {
                    [void][W]::SendMessageW($o2, $WM_KEYDOWN, [IntPtr]0x1B, [IntPtr]::Zero)
                    Start-Sleep -Milliseconds 300
                }
                continue
            }
            Start-Sleep -Milliseconds 250   # 等子控件就绪
            if (Test-CaptureValid $t) { return $t }
            Write-Host ("  [重试] 第 {0} 次拖拽选区不符，关闭工具条重来" -f $try)
            Close-Toolbar $t
        }
        [IntPtr]::Zero
    }

    $tb0 = Invoke-Capture
    Check 'F1 触发截图流程（框选层出现 -> 工具条弹出）' ($tb0 -ne [IntPtr]::Zero)
    if ($tb0 -eq [IntPtr]::Zero) { throw '截图流程未走通（遮罩或工具条未出现）' }
    Write-Host ("  [操作] 消息驱动框选 ({0},{1}) {2}x{3}" -f $SelX, $SelY, $SelW, $SelH)

    # ── 需求 1/4：工具条为「纯图标」按钮，从左到右：贴图/保存/复制/OCR/关闭 ──
    # 控件 ID 保持稳定：1=贴图 2=复制 3=保存 4=OCR 5=关闭（E2E 按 ID 查找，与图标顺序无关）
    $btnPin  = Find-ChildById $tb0 1     # BTN_PIN   = 1
    $btnCopy = Find-ChildById $tb0 2     # BTN_COPY  = 2
    $btnSave = Find-ChildById $tb0 3     # BTN_SAVE  = 3
    $btnOcr  = Find-ChildById $tb0 4     # BTN_OCR   = 4
    $btnClose= Find-ChildById $tb0 5     # BTN_CLOSE = 5
    # 窗口内从左到右的实际顺序 = 需求顺序
    $btnRects = @($btnPin, $btnSave, $btnCopy, $btnOcr, $btnClose) | ForEach-Object { Get-Rect $_ }
    $tbR = Get-Rect $tb0
    $layoutOk = $true
    for ($i = 0; $i -lt $btnRects.Count; $i++) {
        $r = $btnRects[$i]
        # 每个按钮都要有实际尺寸、落在工具条内、且不与左侧按钮重叠
        # （防止 layout() 提前返回时全部控件堆在 (0,0) 10x10 而不被发现）
        # 工具条为紧凑图标条（按钮 32 逻辑像素方形），故下限设 24px 物理像素
        if (($r.R - $r.L) -lt 24 -or ($r.B - $r.T) -lt 20) { $layoutOk = $false }
        if ($r.L -lt $tbR.L -or $r.R -gt $tbR.R) { $layoutOk = $false }
        if ($i -gt 0) { if ($r.L -lt $btnRects[$i - 1].R) { $layoutOk = $false } }
        # 按钮到顶部与到底部的距离必须一致（需求），允许 1px 取整误差
        if ([Math]::Abs(($r.T - $tbR.T) - ($tbR.B - $r.B)) -gt 1) { $layoutOk = $false }
    }
    Check '工具条含 贴图/保存/复制/OCR/关闭 五个图标按钮（排布正确、上下留白一致）' `
        ($btnPin -ne [IntPtr]::Zero -and $btnCopy -ne [IntPtr]::Zero -and $btnSave -ne [IntPtr]::Zero -and $btnOcr -ne [IntPtr]::Zero -and $btnClose -ne [IntPtr]::Zero -and $layoutOk) `
        ("按钮宽 {0}px、起点 x={1}、上留白 {2}px/下留白 {3}px" -f ($btnRects[0].R - $btnRects[0].L), $btnRects[0].L, ($btnRects[0].T - $tbR.T), ($tbR.B - $btnRects[0].B))
    $statics = @(Get-StaticTexts $tb0)
    Check '工具条无标签/状态文本（按钮为纯图标）' ($statics.Count -eq 0) ("STATIC 子控件数：{0}" -f $statics.Count)
    # 悬停提示文案即按钮窗口文本（`src/toolbar.rs` 的 TIP_TEXTS），按窗口顺序核对
    $tips = @($btnPin, $btnSave, $btnCopy, $btnOcr, $btnClose) | ForEach-Object { Get-ControlText $_ }
    Check '五个图标按钮的悬停提示文案正确' `
        (($tips -join '|') -eq '贴图|保存图像|复制并关闭|复制识别到的文字|关闭') ("实际：{0}" -f ($tips -join ' / '))
    # ── 悬停提示：向「复制」按钮发 WM_MOUSEMOVE，延时后应弹出 PixShotTip 气泡 ──
    # 气泡自绘（非 comctl32），文案同步写入窗口标题（src/tooltip.rs SetWindowTextW），便于核对。
    [void][W]::SendMessageW($btnCopy, $WM_MOUSEMOVE, [IntPtr]::Zero, (LP 5 5))
    $tipH = [IntPtr]::Zero
    $tipDeadline = [Environment]::TickCount + 3000
    while ([Environment]::TickCount -lt $tipDeadline) {
        $h = Find-WindowByEnum $pid0 'PixShotTip'
        if ($h -ne [IntPtr]::Zero -and [W]::IsWindowVisible($h)) { $tipH = $h; break }
        Start-Sleep -Milliseconds 50
    }
    $tipText = if ($tipH -ne [IntPtr]::Zero) { Get-Text $tipH } else { '' }
    # 气泡文案 = 说明文字 + 快捷键（Ctrl+C 见 src/toolbar.rs 的 SHORTCUTS）
    Check '悬停图标弹出提示气泡（延时 + 文案含快捷键）' `
        ($tipH -ne [IntPtr]::Zero -and $tipText -eq '复制并关闭（Ctrl+C）') ("PixShotTip 可见={0} 文本='{1}'" -f ($tipH -ne [IntPtr]::Zero), $tipText)
    # 鼠标离开按钮（WM_MOUSELEAVE）→ 气泡收起（窗口仍存在但不可见）
    [void][W]::SendMessageW($btnCopy, $WM_MOUSELEAVE, [IntPtr]::Zero, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 250
    $h2 = Find-WindowByEnum $pid0 'PixShotTip'
    $tipGone = ($h2 -eq [IntPtr]::Zero) -or (-not [W]::IsWindowVisible($h2))
    Check '移开鼠标后提示气泡收起' $tipGone

    # 存档工具条外观：工具条设了 WDA_EXCLUDEFROMCAPTURE（截图工具惯例），
    # 屏幕抓屏只会拍到它背后的内容，故这里只记录几何，不做像素存档。
    $tr0 = Get-Rect $tb0
    Write-Host ("  [调试] 工具条 rect=({0},{1})-({2},{3}) 尺寸 {4}x{5}" -f $tr0.L, $tr0.T, $tr0.R, $tr0.B, ($tr0.R - $tr0.L), ($tr0.B - $tr0.T))

    # ── ✕ 关闭按钮：不执行任何操作，直接收起工具条 ──
    [void][W]::SendMessageW($btnClose, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    Check '点「✕ 关闭」收起工具条（不执行操作）' (Wait-WindowGone $pid0 'PixShotToolbar' 4000)

    # ── Esc 同样是"不执行操作"的取消退路（与 ✕ 等价）──
    $tbEsc = Invoke-Capture
    if ($tbEsc -eq [IntPtr]::Zero) { throw 'Esc 场景截图未出现工具条' }
    [void][W]::PostMessageW($tbEsc, $WM_KEYDOWN, [IntPtr]0x1B, [IntPtr]::Zero)
    Check '工具条按 Esc 直接关闭' (Wait-WindowGone $pid0 'PixShotToolbar' 4000)

    # ── 需求 3：点「复制」→ 写剪贴板（CF_DIB）并关闭工具条 ──
    $tbC = Invoke-Capture
    if ($tbC -eq [IntPtr]::Zero) { throw '「复制」场景截图未出现工具条' }
    $btnCopy = Find-ChildById $tbC 2
    if ($btnCopy -eq [IntPtr]::Zero) { throw '「复制」场景未找到复制按钮' }
    [void][W]::SendMessageW($btnCopy, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 500
    Check '「复制」写入剪贴板（CF_DIB 位图）' (Test-ClipDib)
    Check '「复制」后工具条已关闭' (Wait-WindowGone $pid0 'PixShotToolbar')

    # ── 需求 2：点「OCR 识别」→ 不弹窗口，文字进剪贴板并关闭工具条 ──
    $tb1 = Invoke-Capture
    if ($tb1 -eq [IntPtr]::Zero) { throw '第二次截图未出现工具条' }
    $before = Get-ClipText
    $btnOcr1 = Find-ChildById $tb1 4
    if ($btnOcr1 -eq [IntPtr]::Zero) { throw '第二次工具条未找到 OCR 按钮' }
    [void][W]::SendMessageW($btnOcr1, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    Check '「OCR 识别」后工具条已关闭' (Wait-WindowGone $pid0 'PixShotToolbar')
    Start-Sleep -Milliseconds 300
    Check '「OCR 识别」不再弹出结果窗口' ((Find-WindowByEnum $pid0 'PixShotOcrResult') -eq [IntPtr]::Zero)
    $ocrText = Wait-ClipText 25000 $before
    $hit = ($ocrText -match '(?i)pixshot|识别测试|测试文本|ABC|12345|OCR')
    Check '「OCR 识别」把文字复制到剪贴板' ($ocrText.Trim().Length -gt 0 -and $hit) ("共 $($ocrText.Length) 字符")
    if ($ocrText) {
        $prev = if ($ocrText.Length -gt 200) { $ocrText.Substring(0, 200) + '...' } else { $ocrText }
        Write-Host ("  [OCR 文本预览] {0}" -f (($prev -replace "[\r\n]+", " | ")))
    }

    # ── 需求 3：点「贴图」→ 建贴图窗口并关闭工具条；附带验证选区/置顶/缩放/Esc ──
    $tb2 = Invoke-Capture
    if ($tb2 -eq [IntPtr]::Zero) { throw '第三次截图未出现工具条' }
    $btnPin2 = Find-ChildById $tb2 1
    if ($btnPin2 -eq [IntPtr]::Zero) { throw '第三次工具条未找到贴图按钮' }
    [void][W]::SendMessageW($btnPin2, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    $pin = Wait-WindowByEnum $pid0 'PixShotPin' 8000
    Check '「贴图」创建贴图窗口' ($pin -ne [IntPtr]::Zero)
    if ($pin -eq [IntPtr]::Zero) { throw '贴图窗口未出现' }
    Check '「贴图」后工具条已关闭' (Wait-WindowGone $pid0 'PixShotToolbar')
    $r0 = Get-Rect $pin
    # 贴图窗口 = 图像 + 四周投影留白（SHADOW_MARGIN_LOGICAL=16 逻辑像素 × 显示器 DPI）：
    # 图像内容仍严格落在原截图位置，窗口整体外扩一圈用于绘制柔和投影。
    $pinDpi = [W]::GetDpiForWindow($pin)
    if ($pinDpi -lt 96) { $pinDpi = 96 }
    $pad = [int][Math]::Round(16 * $pinDpi / 96.0)
    Check '贴图位于原截图位置（图像内容偏移一圈留白，同时校验选区）' `
        (($r0.L -eq ($SelX - $pad)) -and ($r0.T -eq ($SelY - $pad)) -and (($r0.R - $r0.L) -eq ($SelW + 2 * $pad)) -and (($r0.B - $r0.T) -eq ($SelH + 2 * $pad))) `
        ("rect=({0},{1})-({2},{3}) 图像应位于 ({4},{5}) {6}x{7}，投影留白 {8}px" -f $r0.L, $r0.T, $r0.R, $r0.B, $SelX, $SelY, $SelW, $SelH, $pad)
    $ex = [W]::GetWindowLongW($pin, -20)     # GWL_EXSTYLE
    Check '贴图窗口置顶（WS_EX_TOPMOST）' (($ex -band 8) -ne 0) ("exstyle=0x{0:X8}" -f $ex)

    # ── 回归：贴图右键菜单弹出后，在贴图上（= 菜单之外）左键单击应立即关闭菜单 ──
    # 菜单用 SetCapture 捕获鼠标，落在贴图（同进程窗口）上的点击也会送到菜单窗口，
    # 且不会引起失焦（不走 WM_KILLFOCUS）。若菜单不判"坐标落在客户区之外"，
    # 该点击会被当成"点在菜单内空白处"而忽略，菜单就残留在贴图上。
    [void][W]::PostMessageW($pin, $WM_CONTEXTMENU, [IntPtr]::Zero, (LP 600 420))
    $menu = Wait-WindowByEnum $pid0 'PixShotMenu' 5000
    $menuDetail = if ($menu -eq [IntPtr]::Zero) { '菜单未弹出' } else { '菜单已弹出，在菜单外左键单击' }
    $menuGone = $false
    if ($menu -ne [IntPtr]::Zero) {
        [void][W]::PostMessageW($menu, $WM_LBUTTONDOWN, [IntPtr]1, (LP -40 -40))   # 客户区外坐标
        $menuGone = Wait-WindowGone $pid0 'PixShotMenu' 3000
    }
    Check '贴图右键菜单：点击菜单外（贴图上）即关闭' $menuGone $menuDetail
    try {
        Add-Type -AssemblyName System.Drawing
        $bmp = New-Object System.Drawing.Bitmap(($SelW + 60), ($SelH + 60))
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen(($SelX - 30), ($SelY - 30), 0, 0, $bmp.Size)
        $g.Dispose(); $bmp.Save((Join-Path $art 'pin_visible.png'), [System.Drawing.Imaging.ImageFormat]::Png)
        Write-Host '  [存档] pin_visible.png'
        # 投影观感采样（仅诊断输出）：图像外 pad/2 处应有灰阶投影，窗口更外侧应回到背景色。
        # 注：非 DPI 感知进程的 CopyFromScreen 取物理像素，而 GetWindowRect 为逻辑像素，
        # 在 125%/150% 缩放下两者存在偏移，故这里只打印数值供人工核对，不做断言。
        try {
            $midY = [int](30 + $SelH / 2)
            $near = $bmp.GetPixel([Math]::Max(1, 30 - [int]($pad / 2)), $midY)
            $far = $bmp.GetPixel(2, $midY)
            Write-Host ("  [阴影采样] 贴图像素外 {0}px=(R{1},G{2},B{3}) / 窗口外 2px=(R{4},G{5},B{6})" -f [int]($pad / 2), $near.R, $near.G, $near.B, $far.R, $far.G, $far.B)
        } catch { Write-Host "  [阴影采样] 跳过：$($_.Exception.Message)" }
        $bmp.Dispose()
    } catch { Write-Host "  [跳过] 截屏存档不可用：$($_.Exception.Message)" }

    # ── 滚轮缩放贴图（WM_MOUSEWHEEL，delta=+120 ×2 → 约 1.21 倍）──
    [void][W]::SendMessageW($pin, $WM_MOUSEWHEEL, [IntPtr](120 -shl 16), [IntPtr]::Zero)
    Start-Sleep -Milliseconds 200
    [void][W]::SendMessageW($pin, $WM_MOUSEWHEEL, [IntPtr](120 -shl 16), [IntPtr]::Zero)
    Start-Sleep -Milliseconds 300
    $r1 = Get-Rect $pin
    $w0 = $r0.R - $r0.L; $h0 = $r0.B - $r0.T
    $gw = ($r1.R - $r1.L) - $w0
    $gh = ($r1.B - $r1.T) - $h0
    $zoomOk = ($gw -ge [int]($w0 * 0.17)) -and ($gw -le [int]($w0 * 0.30)) -and ($gh -ge [int]($h0 * 0.17)) -and ($gh -le [int]($h0 * 0.30))
    Check '滚轮缩放贴图（两次放大 ≈ ×1.21）' $zoomOk ("尺寸 ${w0}x${h0} -> +${gw}x+${gh}px")

    # ── Esc 关闭贴图 ──
    [void][W]::SendMessageW($pin, $WM_KEYDOWN, [IntPtr]0x1B, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 600
    Check 'Esc 关闭贴图窗口' (Wait-WindowGone $pid0 'PixShotPin')
} finally {
    if ($form) { try { $form.Close(); [void][System.Windows.Forms.Application]::DoEvents() } catch { } }
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })

