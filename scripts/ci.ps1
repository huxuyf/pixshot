<#
PixShot 本地 CI 流水线
======================
一条命令跑完「编译检查 → 单元/集成测试 → Debug 构建 → Release 构建 → 体积护栏 → GUI 端到端」，
任一环节失败立即中止并返回该环节编号，便于快速定位。

设计原则
--------
- 不做任何隐式修复：只做检查和断言，不自动改写源码或产物。
- 原生命令（cargo）失败不会触发 PowerShell 异常，因此每步都显式校验 $LASTEXITCODE。
- E2E 会接管鼠标键盘约 20 秒，默认开启但可用 -SkipE2E 关闭（无桌面/CI 环境必须关闭）。

用法
----
  pwsh -File scripts\ci.ps1                 # 全流程
  pwsh -File scripts\ci.ps1 -SkipE2E        # 只跑静态检查与构建
  pwsh -File scripts\ci.ps1 -MaxSizeMB 3.0  # 放宽体积护栏

参数
----
  -SkipE2E        跳过 GUI 端到端测试
  -MaxSizeMB      release 产物体积上限（默认 2.6，用于防止依赖/特性膨胀）
  -TargetDir      自定义 CARGO_TARGET_DIR（默认仓库内 target\，不设置该环境变量）

退出码
------
  0  全部通过
  1  初始化/前置条件失败
  2..7  对应步骤失败（见输出中的 [n/6] 编号）
#>

[CmdletBinding()]
param(
    [switch]$SkipE2E,
    [double]$MaxSizeMB = 2.6,
    [string]$TargetDir = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$root = Split-Path -Parent $PSScriptRoot
$targetRoot = if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    Join-Path $root 'target'
} else {
    $TargetDir
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Host '找不到 cargo，请先安装 Rust 工具链（https://rustup.rs）。' -ForegroundColor Red
    exit 1
}
if (-not (Test-Path (Join-Path $root 'Cargo.toml'))) {
    Write-Host "仓库根目录无效：$root" -ForegroundColor Red
    exit 1
}

Push-Location $root
$script:Step = 0
$script:Total = if ($SkipE2E) { 5 } else { 6 }

function Assert-Exit {
    param([string]$What)
    if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
        throw ("{0} 失败（exit {1}）" -f $What, $LASTEXITCODE)
    }
}

function Invoke-Step {
    param(
        [string]$Name,
        [scriptblock]$Action
    )
    $script:Step++
    $n = $script:Step
    Write-Host ''
    Write-Host ('[{0}/{1}] {2}' -f $n, $script:Total, $Name) -ForegroundColor Cyan
    $sw = [Diagnostics.Stopwatch]::StartNew()
    try {
        & $Action
    } catch {
        $sw.Stop()
        Write-Host ('  x {0}' -f $_.Exception.Message) -ForegroundColor Red
        Write-Host ('  在 {0:N1}s 处中止。' -f $sw.Elapsed.TotalSeconds) -ForegroundColor DarkGray
        Pop-Location
        exit ($n + 1)
    }
    $sw.Stop()
    Write-Host ('  v 通过（{0:N1}s）' -f $sw.Elapsed.TotalSeconds) -ForegroundColor Green
}

# ---------------------------------------------------------------- 0. 环境
Write-Host ''
Write-Host 'PixShot CI' -ForegroundColor White
Write-Host ("  仓库    {0}" -f $root) -ForegroundColor DarkGray
Write-Host ("  产物    {0}" -f $targetRoot) -ForegroundColor DarkGray
Write-Host ("  E2E     {0}" -f $(if ($SkipE2E) { '跳过' } else { '执行' })) -ForegroundColor DarkGray

if (-not [string]::IsNullOrWhiteSpace($TargetDir)) {
    $env:CARGO_TARGET_DIR = $TargetDir
}

# ------------------------------------------------------- 1. 编译检查（快）
Invoke-Step 'cargo check（编译检查，含 build.rs 图标生成）' {
    cargo check --all-targets --offline
    Assert-Exit 'cargo check'
}

# ---------------------------------------------------- 2. 单元 + 集成测试
Invoke-Step 'cargo test（单元测试 + tests\ 集成测试）' {
    cargo test --offline
    Assert-Exit 'cargo test'
}

# ------------------------------------------------------------ 3. Debug 构建
Invoke-Step 'cargo build --profile dev（供 E2E 使用）' {
    cargo build --offline
    Assert-Exit 'cargo build'
}

# ---------------------------------------------------------- 4. Release 构建
Invoke-Step 'cargo build --release（发布产物）' {
    cargo build --release --offline
    Assert-Exit 'cargo build --release'
}

# ------------------------------------------------------------ 5. 体积护栏
Invoke-Step '发布体积护栏' {
    $exe = Join-Path $targetRoot 'release\pixshot.exe'
    if (-not (Test-Path $exe)) {
        throw "未找到 $exe"
    }
    $mb = (Get-Item $exe).Length / 1MB
    Write-Host ('  体积 {0:N2} MB / 上限 {1:N2} MB' -f $mb, $MaxSizeMB) -ForegroundColor DarkGray
    if ($mb -gt $MaxSizeMB) {
        throw ('体积 {0:N2} MB 超出上限 {1:N2} MB —— 检查是否新增了重量级依赖或未裁剪的 crate feature' -f $mb, $MaxSizeMB)
    }
}

# -------------------------------------------------------------- 6. GUI E2E
if (-not $SkipE2E) {
    Invoke-Step 'GUI 端到端（框选 → 贴图 → OCR，会接管鼠标键盘）' {
        $e2e = Join-Path $PSScriptRoot 'gui_e2e_test.ps1'
        if (-not (Test-Path $e2e)) {
            throw "找不到 $e2e"
        }
        $debugExe = Join-Path $targetRoot 'debug\pixshot.exe'
        if (-not (Test-Path $debugExe)) {
            throw "找不到 $debugExe"
        }
        # 用独立进程执行：E2E 脚本内部以 exit 返回结果，
        # 若直接 & 调用会终止当前会话，故必须隔离。
        $hostExe = (Get-Process -Id $PID).Path
        $p = Start-Process -FilePath $hostExe `
            -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $e2e, '-Exe', $debugExe) `
            -NoNewWindow -Wait -PassThru
        if ($p.ExitCode -ne 0) {
            throw ("GUI 端到端失败（exit {0}），详见 e2e_artifacts\e2e_transcript.txt" -f $p.ExitCode)
        }
    }
}

Pop-Location

Write-Host ''
Write-Host ('全部 {0} 个步骤通过。' -f $script:Total) -ForegroundColor Green
exit 0
