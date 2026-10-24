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
Set-Variable -Name 'WM_LBUTTONUP' -Value 0x0202 -Option Constant
Set-Variable -Name 'WM_MOUSEWHEEL' -Value 0x020A -Option Constant
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
Write-Host '==== PixShot E2E（消息驱动版 v2）：框选截图 -> 复制 -> 贴图置顶(缩放/Esc) -> OCR -> 复制 ===='
Write-Host ''

# 框选区域（虚拟屏幕坐标）
$SelX = 300; $SelY = 300; $SelW = 600; $SelH = 400

$proc = $null
try {
    # ── 0. 文字靶标窗口（为 OCR 提供确定性内容，完全位于选区内）──
    $form = New-Object System.Windows.Forms.Form
    $form.StartPosition = 'Manual'
    $form.Location = New-Object System.Drawing.Point(350, 350)
    $form.Size = New-Object System.Drawing.Size(500, 300)
    $form.FormBorderStyle = 'None'
    $form.BackColor = [System.Drawing.Color]::White
    $form.TopMost = $true
    $lbl = New-Object System.Windows.Forms.Label
    $lbl.Text = "PixShot OCR E2E`n识别测试文本 ABC 12345"
    $lbl.Font = New-Object System.Drawing.Font('Microsoft YaHei', 22, [System.Drawing.FontStyle]::Bold)
    $lbl.ForeColor = [System.Drawing.Color]::Black
    $lbl.AutoSize = $false
    $lbl.Size = New-Object System.Drawing.Size(480, 280)
    $lbl.Location = New-Object System.Drawing.Point(10, 10)
    $form.Controls.Add($lbl)
    $form.Show()
    for ($i = 0; $i -lt 15; $i++) { [void][System.Windows.Forms.Application]::DoEvents(); Start-Sleep -Milliseconds 50 }
    Write-Host '  [靶标] 已在选区内显示确定性文字窗口（PixShot OCR E2E / 识别测试文本 ABC 12345）'

    # ── 1~3. 启动 → 消息拖拽框选 → 工具栏（带选区验证与重试）
    # 注：SetCapture 期间真实鼠标移动也会进入选区（用户可能正在操作），
    #     拖拽后从工具栏标签读取实际选区，若被干扰则重试整个流程。
    $SelX = 300; $SelY = 300; $SelW = 600; $SelH = 400
    $ov = [IntPtr]::Zero; $tb = [IntPtr]::Zero; $regionOk = $false; $attempt = 0
    $startupOk = $false; $f1Ok = $false
    while (-not $regionOk -and $attempt -lt 3) {
        $attempt++
        if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force; Start-Sleep -Milliseconds 500 }
        $proc = Start-Process -FilePath $Exe -PassThru
        $pid0 = $proc.Id
        # ── 需求验证：启动后托盘驻留（不弹遮罩）；F1（WM_HOTKEY 路径）触发截图 ──
        $mainH = Wait-WindowByEnum $pid0 'PixShotMain' 10000
        if ($mainH -eq [IntPtr]::Zero) { continue }
        if (-not $startupOk) {
            Start-Sleep -Milliseconds 500
            # 启动后 0.5s 仍无遮罩 = 托盘驻留（未进入截图流程）
            $startupOk = (Find-WindowByEnum $pid0 'PixShotOverlay') -eq [IntPtr]::Zero
            Check '启动后常驻托盘（无遮罩/主窗口弹出）' $startupOk
        }
        # F1 全局热键路径：WM_HOTKEY(0x0312) wp=HOTKEY_ID —— Post 异步投递
        [void][W]::PostMessageW($mainH, 0x0312, [IntPtr]1, [IntPtr]0)
        $ov = Wait-WindowByEnum $pid0 'PixShotOverlay' 12000
        if (-not $f1Ok) {
            $f1Ok = $ov -ne [IntPtr]::Zero
            Check 'F1 触发截图流程（遮罩出现）' $f1Ok
        }
        if ($ov -eq [IntPtr]::Zero) { continue }
        # 快速拖拽（缩短受真实输入干扰的窗口）
        [void][W]::SendMessageW($ov, $WM_LBUTTONDOWN, [IntPtr]1, (LP $SelX $SelY))
        foreach ($p in @(@(560, 480), @(($SelX + $SelW), ($SelY + $SelH)))) {
            [void][W]::SendMessageW($ov, $WM_MOUSEMOVE, [IntPtr]1, (LP $p[0] $p[1]))
            Start-Sleep -Milliseconds 20
        }
        [void][W]::SendMessageW($ov, $WM_LBUTTONUP, [IntPtr]0, (LP ($SelX + $SelW) ($SelY + $SelH)))
        $dragOk = -not [W]::IsWindow($ov)
        if (-not $dragOk) { continue }
        $tb = Wait-WindowByEnum $pid0 'PixShotToolbar' 15000
        if ($tb -eq [IntPtr]::Zero) { continue }
        $label = (Get-StaticTexts $tb) | Where-Object { $_ -match '已截取' } | Select-Object -First 1
        Write-Host ("  [尝试 $attempt] 工具栏标签: {0}" -f $label)
        if ($label -match '已截取\s*(\d+)\s*[×x]\s*(\d+)') {
            $lw = [int]$Matches[1]; $lh = [int]$Matches[2]
            if ($lw -eq $SelW -and $lh -eq $SelH) { $regionOk = $true }
        }
    }
    Check '拖拽框选完成（遮罩按选区销毁）' $dragOk
    Check '框选完成 -> 操作工具栏弹出，选区 600x400' ($regionOk) "第 $attempt 次尝试"
    if (-not $regionOk) { throw '选区被真实鼠标干扰，3 次尝试均不符' }
    Write-Host ("  [操作] 消息驱动框选 ({0},{1}) {2}x{3}" -f $SelX, $SelY, $SelW, $SelH)

    $btnCopy = Find-ChildById $tb 2      # BTN_COPY = 2
    $btnOcr  = Find-ChildById $tb 4      # BTN_OCR  = 4
    $btnPin  = Find-ChildById $tb 1      # BTN_PIN  = 1
    Check '工具栏含 贴图/复制/OCR 按钮' ($btnPin -ne [IntPtr]::Zero -and $btnCopy -ne [IntPtr]::Zero -and $btnOcr -ne [IntPtr]::Zero)

    # ── 4. 复制图像 -> 剪贴板 CF_DIB ──
    [void][W]::SendMessageW($btnCopy, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 500
    $dibOk = Test-ClipDib
    Check '截图复制到剪贴板（CF_DIB 位图）' $dibOk
    # 复制提示已改为非模态 Toast（PixShotToast），不再有 #32770 消息框需要关闭。

    # ── 5. 发起 OCR（工具栏按钮，后台线程识别）──
    [void][W]::SendMessageW($btnOcr, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    $ocrw = Wait-WindowByEnum $pid0 'PixShotOcrResult' 8000
    Check 'OCR 结果窗口已打开' ($ocrw -ne [IntPtr]::Zero)
    if ($ocrw -eq [IntPtr]::Zero) { throw 'OCR 结果窗口未出现' }

    # ── 6. 贴图 ──
    [void][W]::SendMessageW($btnPin, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
    $pin = Wait-WindowByEnum $pid0 'PixShotPin' 8000
    Check '贴图窗口已创建' ($pin -ne [IntPtr]::Zero)
    if ($pin -eq [IntPtr]::Zero) { throw '贴图窗口未出现' }
    $r0 = Get-Rect $pin
    Check '贴图位于原截图位置（初始 100% 缩放）' `
        (($r0.L -eq $SelX) -and ($r0.T -eq $SelY) -and (($r0.R - $r0.L) -eq $SelW) -and (($r0.B - $r0.T) -eq $SelH)) `
        ("rect=({0},{1})-({2},{3})" -f $r0.L, $r0.T, $r0.R, $r0.B)
    $ex = [W]::GetWindowLongW($pin, -20)     # GWL_EXSTYLE
    Check '贴图窗口置顶（WS_EX_TOPMOST）' (($ex -band 8) -ne 0) ("exstyle=0x{0:X8}" -f $ex)
    try {
        Add-Type -AssemblyName System.Drawing
        $bmp = New-Object System.Drawing.Bitmap(($SelW + 60), ($SelH + 60))
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen(($SelX - 30), ($SelY - 30), 0, 0, $bmp.Size)
        $g.Dispose(); $bmp.Save((Join-Path $art 'pin_visible.png'), [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
        Write-Host '  [存档] pin_visible.png'
    } catch { Write-Host "  [跳过] 截屏存档不可用：$($_.Exception.Message)" }

    # ── 7. 滚轮缩放贴图（WM_MOUSEWHEEL，delta=+120 ×2）──
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

    # ── 8. OCR 识别完成并读取文本 ──
    $title = ''
    $deadline = [Environment]::TickCount + 25000
    while ([Environment]::TickCount -lt $deadline) {
        $title = Get-Text $ocrw
        if ($title -match '识别结果' -or $title -match '识别失败') { break }
        [void][System.Windows.Forms.Application]::DoEvents()
        Start-Sleep -Milliseconds 250
    }
    Check 'OCR 后台识别完成' ($title -match '识别结果') $title
    $edit = [IntPtr]::Zero
    $c = [W]::GetWindow($ocrw, 5)
    while ($c -ne [IntPtr]::Zero) {
        $sb = New-Object System.Text.StringBuilder 64
        [void][W]::GetClassNameW($c, $sb, 64)
        if ($sb.ToString() -eq 'EDIT') { $edit = $c; break }
        $c = [W]::GetWindow($c, 2)
    }
    $text = ''
    if ($edit -ne [IntPtr]::Zero) { $text = Get-ControlText $edit }
    $hit = ($text -match '(?i)pixshot|识别测试|测试文本|ABC|12345|OCR')
    Check 'OCR 识别出靶标文字（中文/英文/数字）' ($text.Trim().Length -gt 0 -and $hit) "共 $($text.Length) 字符"
    if ($text) {
        $prev = if ($text.Length -gt 200) { $text.Substring(0, 200) + '...' } else { $text }
        Write-Host ("  [OCR 文本预览] {0}" -f (($prev -replace "[\r\n]+", " | ")))
    }

    # ── 9. 复制文本 -> 剪贴板比对 ──
    $btnCopyText = Find-ChildById $ocrw 100     # ID_COPY = 100
    Check '结果窗口含「复制文本」按钮' ($btnCopyText -ne [IntPtr]::Zero)
    if ($btnCopyText -ne [IntPtr]::Zero) {
        # 复制提示已改为非模态 Toast，不再弹 #32770 模态框、也无须关闭，
        # 异步点击后留出写剪贴板的时间即可。
        [void][W]::PostMessageW($btnCopyText, $BM_CLICK, [IntPtr]::Zero, [IntPtr]::Zero)
        Start-Sleep -Milliseconds 700
        $clip = Get-ClipText
        Check '识别结果可复制到剪贴板（内容一致）' ($clip.Trim() -eq $text.Trim()) `
            ("剪贴板 $($clip.Length) 字符 / 窗口 $($text.Length) 字符")
    }

    # ── 10. Esc 关闭贴图 ──
    [void][W]::SendMessageW($pin, $WM_KEYDOWN, [IntPtr]0x1B, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 600
    $pinGone = (Find-WindowByEnum $pid0 'PixShotPin') -eq [IntPtr]::Zero
    Check 'Esc 关闭贴图窗口' $pinGone
} finally {
    if ($form) { try { $form.Close(); [void][System.Windows.Forms.Application]::DoEvents() } catch { } }
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue }
}

Write-Host ''
Write-Host ("==== 测试结束：{0} 项通过，{1} 项失败 ====" -f $script:Pass, $script:Fail)
try { Stop-Transcript | Out-Null } catch { }
exit $(if ($script:Fail -gt 0) { 1 } else { 0 })

