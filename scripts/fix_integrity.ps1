<#
PixShot 完整性标签修复
======================
症状：程序在运行，但托盘没有图标、按 F1 也无反应；诊断日志显示
      Shell_NotifyIcon 返回失败、Win32 错误 5（拒绝访问）。

原因：本程序 exe 文件带有「Low Mandatory Level」强制完整性标签（沙箱类工具
      写入目录时会打标，并被目录中的 exe 继承）。从该文件启动的进程以
      **低完整性** 运行，Windows 的 UIPI 机制会拒绝其访问资源管理器（中完整性）
      的托盘窗口 —— 因此托盘注册必然失败。

修复：把程序目录及其子项的完整性标签设为 Medium（普通用户级别）。
      设置后，目录内新建的文件（如 cargo 重新编译产出的 exe）会自动继承 Medium，
      因此 `cargo clean` 后重新构建也不会复发。

用法：
  pwsh -File scripts\fix_integrity.ps1                 # 修复本工程目录
  pwsh -File scripts\fix_integrity.ps1 -Path D:\app    # 修复指定目录
  pwsh -File scripts\fix_integrity.ps1 -CheckOnly      # 仅检查、不修改

需要权限：对自己拥有的目录无需管理员；若目录属其他用户/受保护，请以管理员运行。
#>
param(
    [string]$Path = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path,
    [switch]$CheckOnly
)

$ErrorActionPreference = 'Continue'
Write-Host "目标目录: $Path"

if (-not (Test-Path $Path)) { Write-Host "路径不存在" -ForegroundColor Red; exit 1 }

function Show-Label([string]$p) {
    $line = (icacls $p 2>&1 | Select-String "Mandatory Label") -replace '\s+', ' '
    if (-not $line) { $line = '  (无完整性标签——正常)' }
    Write-Host ("  {0,-58} {1}" -f $p, $line.Trim())
}

Write-Host ''
Write-Host '── 当前标签 ──'
Show-Label $Path
Get-ChildItem $Path -Filter '*.exe' -Recurse -ErrorAction SilentlyContinue |
    Select-Object -First 5 | ForEach-Object { Show-Label $_.FullName }

if ($CheckOnly) { Write-Host ''; Write-Host '（仅检查模式，未做修改）'; exit 0 }

Write-Host ''
Write-Host '── 应用修复：(OI)(CI)Medium ──'
$r = icacls $Path /setintegritylevel "(OI)(CI)Medium" /T 2>&1
$r | Select-Object -Last 2

Write-Host ''
Write-Host '── 修复后标签 ──'
Show-Label $Path
Get-ChildItem $Path -Filter '*.exe' -Recurse -ErrorAction SilentlyContinue |
    Select-Object -First 5 | ForEach-Object { Show-Label $_.FullName }

Write-Host ''
Write-Host '完成。请重新启动 pixshot.exe 验证：托盘图标应出现，F1 应能截图。' -ForegroundColor Green
