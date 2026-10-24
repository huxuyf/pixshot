# Windows「无法验证发布者」安全警告完整解决方案

> 适用对象：未数字签名的 `.exe`（含本仓库 `target\release\pixshot.exe` 的分发）。
> 原则：**优先做"验证 + 单文件放行/正式签名"，系统级关闭防护仅用于受控测试环境且用完即恢复。**
> 标注约定：🧑‍💻 = 普通用户权限即可；🔴 **管理员** = 需以管理员身份运行的 PowerShell/CMD。

---

## 1. 先定位警告来源（三种机制，对应不同解法）

| 你看到的对话框 | 触发机制 | 触发条件 | 对应章节 |
|---|---|---|---|
| **「打开文件 - 安全警告」**：无法验证发布者 / 此文件没有包含有效的数字签名 | 附件管理器（Attachment Manager） | 文件带"网络标记"（MotW，Zone.Identifier）且未签名 | 方案一 / 方案三 / 签名 |
| **UAC 提权框**："你想允许来自未知发布者的此应用对你的设备进行更改吗？" | UAC | 未签名 exe 请求管理员权限 | 方案四 / 签名 |
| **「Windows 已保护你的电脑」**（蓝色全屏） | SmartScreen | 从 Edge/浏览器下载 + 无信誉 | 方案二 / EV 或 Artifact Signing |

**自检命令（🧑‍💻）**——判断属于哪种情况：

```powershell
# ① 文件是否带"网络标记"（有输出 = 带 MotW，是"打开文件-安全警告"的根源）
Get-Item .\pixshot.exe -Stream Zone.Identifier -ErrorAction SilentlyContinue

# ② 签名状态（NotSigned = 未签名；Valid = 签名有效）
Get-AuthenticodeSignature .\pixshot.exe | Format-List Status, StatusMessage, @{n='签发者';e={$_.SignerCertificate.Subject}}
```

> 说明：在本机用 `cargo build` 编译出的 exe **不会**触发警告（无 MotW）；警告通常出现在把 exe 经网络（微信/浏览器/网盘）传到另一台机器运行时。

---

## 2. 安全评估：放行前必须完成的验证

**判定原则：来源可信 + 哈希一致 才可放行；二者缺一，宁可不运行。**

### 2.1 验证文件来源可信
- 只从官方渠道（项目官网、官方 GitHub Releases 页、可信镜像）获取文件；
- 下载链接必须是 HTTPS，且域名与项目方一致（警惕仿冒域名）；
- 经他人转发的压缩包/安装包视为不可信，须重新从官方渠道核对。

### 2.2 验证文件完整性（哈希比对）
```powershell
# 🧑‍💻 计算文件 SHA-256
Get-FileHash .\pixshot.exe -Algorithm SHA256
# 或使用 certutil（CMD 下）
certutil -hashfile pixshot.exe SHA256
```
将结果与发布页/发布者公布的哈希**逐字符比对**；不一致 = 文件被篡改或传输损坏，**必须删除**。
（本仓库可自行构建后比对：同一源码同一工具链构建的哈希应与你分发的哈希一致。）

### 2.3 验证签名与证书链（若文件有签名）
```powershell
# 🧑‍💻
Get-AuthenticodeSignature .\someapp.exe | Format-List Status, StatusMessage, @{n='签发者';e={$_.SignerCertificate.Subject}}, @{n='链';e={$_.SignerCertificate.Chain.ChainElements.Subject}}
```
```cmd
signtool verify /pa /v someapp.exe
```
- `Status: Valid` 且签发者与官方一致 → 可信；
- `HashMismatch`（哈希不符）或 `NotTimeValid` → 不可信；
- GUI 方式：右键文件 → 属性 → 「数字签名」选项卡 → 查看证书。

### 2.4 多引擎扫描（可选，谨慎上传）
- 在 [VirusTotal](https://www.virustotal.com) 上传哈希查询多引擎结果；
- **注意**：上传文件本身有泄露/公开风险（内部软件、含敏感信息的文件不要上传），商业环境建议用企业 EDR 替代。

### 2.5 放行判定矩阵

| 情形 | 处置 |
|---|---|
| 官方渠道 + 哈希一致 | 可放行（走方案一/二） |
| 官方渠道但无公开哈希 | 可放行，风险略高，先 2.4 扫描 |
| 哈希不一致 / 来源不明 | **禁止运行**，警告是正确保护 |
| "破解版/绿色版/激活工具" | 警告正确，此类文件是恶意软件重灾区 |

---

## 3. 用户端操作（按优先级从高到低）

### 方案一（最优先）：单文件「解除锁定」——风险最小

**适用**：第 1 节中的"打开文件 - 安全警告"（MotW 引起）。仅影响当前文件，不改变系统防护。
**权限**：🧑‍💻 普通用户。**系统兼容**：Windows 7 及以上全部版本。

GUI 步骤：
1. 右键该 `.exe` → **属性**；
2. 「常规」选项卡底部若显示"安全：此文件来自其他计算机…"，勾选 **解除锁定**；
3. 确定 → 重新运行，警告消失。

PowerShell（与 GUI 等效，可批量）：
```powershell
# 🧑‍💻 解除单个文件
Unblock-File -Path .\pixshot.exe
# 🧑‍💻 解除整个目录
Get-ChildItem .\dist -Recurse -File | Unblock-File
# 验证（应无输出）
Get-Item .\pixshot.exe -Stream Zone.Identifier -ErrorAction SilentlyContinue
```

**回退方法**：`Unblock-File` 只是删除了文件的 `Zone.Identifier` 备用数据流，无系统副作用；如需恢复标记（例如测试）：
```powershell
Set-Content -Path .\pixshot.exe -Stream Zone.Identifier -Value "[ZoneTransfer]`nZoneId=3"
```

**局限**：对 UAC"未知发布者"和 SmartScreen 蓝屏警告**无效**（那些不是 MotW 机制）。

---

### 方案二：SmartScreen——先单次放行，系统级调整须谨慎

**2a. 单次放行（推荐，🧑‍💻 普通用户，Win10/Win11 全版本）**
警告界面点击 **更多信息** → **仍要运行**。仅对该文件本次运行生效，不改变任何设置。

**2b. 系统级关闭"检查应用和文件"（🔴 管理员，Win11）**
设置 → 隐私和安全性 → Windows 安全中心 → 应用和浏览器控制 → 基于信誉的保护设置 → 关闭 **"检查应用和文件"**。

**2c. 组策略（Windows 10/11 专业版/企业版，🔴 管理员）**
`Win+R` → `gpedit.msc` → 计算机配置 → 管理模板 → Windows 组件 → **文件资源管理器** → 「配置 Windows Defender SmartScreen」→ 已启用 → 选项选 **"关闭 SmartScreen"**。

**2d. 注册表（家庭版/专业版通用，🔴 管理员 CMD/PowerShell）**
```cmd
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer" /v SmartScreenEnabled /t REG_SZ /d Off /f
reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows\System" /v EnableSmartScreen /t REG_DWORD /d 0 /f
```
**回退命令**：
```cmd
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer" /v SmartScreenEnabled /t REG_SZ /d Warn /f
reg delete "HKLM\SOFTWARE\Policies\Microsoft\Windows\System" /v EnableSmartScreen /f
```

**⚠ 安全风险（2b/2c/2d）**：关闭后系统不再对任何下载文件做信誉与恶意检测，显著增大中招概率。仅建议隔离虚拟机/测试机使用，测试完立即回退；日常上网主力机**禁止**关闭。

---

### 方案三：治理「打开文件 - 安全警告」的系统级配置（不推荐日常使用）

适用：批量处理大量下载文件、且你已理解风险的测试环境。其含义是"今后下载的文件不再打网络标记"。

组策略（专业版/企业版，🔴 管理员）：`gpedit.msc` → 用户配置 → 管理模板 → Windows 组件 → **附件管理器** → 「不保留区域信息」→ 已启用。

注册表（🧑‍💻 用户级即可，写入 HKCU 不需要管理员）：
```cmd
reg add "HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Attachments" /v SaveZoneInformation /t REG_DWORD /d 1 /f
```
**回退**：
```cmd
reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Attachments" /v SaveZoneInformation /f
```
**注意**：① 对已下载文件不追溯，存量文件仍需方案一解除；② 与 2d 同级别的高风险，日常主机不要开启。

---

### 方案四：UAC「无法验证发布者」——Windows 没有单文件豁免

未签名 exe 若请求管理员权限，UAC 必然标注"未知发布者"，**无法像 MotW 那样按文件放行**——这是设计使然。正确解法是第 4 节的开发者签名。

若仅在隔离测试机上临时需要（**强烈不推荐**，🔴 管理员，风险极高——等于关闭 UAC）：
```cmd
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" /v ConsentPromptBehaviorAdmin /t REG_DWORD /d 0 /f
```
**回退（恢复默认）**：
```cmd
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System" /v ConsentPromptBehaviorAdmin /t REG_DWORD /d 5 /f
```

---

### 方案五：企业受控环境（正规做法）

由 IT 部门统一实施，终端用户无需任何手工操作：
1. 采购/自建代码签名证书，对内部分发的 exe 统一签名；
2. 通过域组策略把内网签名 CA 推送到「受信任的根证书颁发机构」+「受信任的发布者」；
3. 用 **AppLocker / WDAC** 建立应用允许列表，未签名/未授权程序默认拒绝。

---

### 用户端方案总对比

| 方案 | 针对的警告 | 权限 | 兼容性 | 风险 | 可逆性 |
|---|---|---|---|---|---|
| 一 解除锁定 | 打开文件-安全警告 | 🧑‍💻 | Win7+ | 极低（单文件） | 天然可逆 |
| 二2a 仍要运行 | SmartScreen | 🧑‍💻 | Win10/11 | 低（单次） | 天然可逆 |
| 二2b-2d 关 SmartScreen | SmartScreen | 🔴 | Win10/11 | **高**（全局） | 有回退命令 |
| 三 附件管理器策略 | 打开文件-安全警告 | 🧑‍💻/🔴 | Win7+ | **高**（全局） | 有回退命令 |
| 四 关 UAC | UAC 未知发布者 | 🔴 | Win7+ | **极高** | 有回退命令 |
| 五 企业统一签名 | 全部 | IT 管理员 | 域环境 | 低 | — |

---

## 4. 开发者端修复：数字签名（根治方案）

### 4.1 证书方案选型

| 方案 | 大致成本 | 信任范围 | SmartScreen 信誉 | 适用场景 |
|---|---|---|---|---|
| 自签名证书 | 免费 | 仅本机/导入信任的内网机器 | 不建立 | 开发自测、企业内网 |
| OV 代码签名证书（DigiCert/Sectigo/SSL.com 等） | 数百元/年 | 全球 | 下载量积累后逐步建立 | 正式分发起步 |
| EV 代码签名证书（硬件令牌/云 HSM） | 千元级/年 | 全球 | **立即建立** | 商业软件 |
| **Azure Trusted Signing（现更名 Artifact Signing）** | 按月订阅，个人可申请 | 全球（微软平台维护证书） | 快速建立 | 个人/小团队推荐 |

### 4.2 signtool 标准签名（OV/EV 证书）

获取 signtool（任选其一）：
- 安装 [Windows SDK](https://developer.microsoft.com/windows/downloads/windows-sdk/)（signtool 位于 `C:\Program Files (x86)\Windows Kits\10\bin\<版本>\x64\signtool.exe`）；
- 或不装 SDK 直接取：
```powershell
Invoke-WebRequest -Uri https://dist.nuget.org/win-x86-commandline/latest/nuget.exe -OutFile .\nuget.exe
.\nuget.exe install Microsoft.Windows.SDK.BuildTools -x
# signtool 位于 .\Microsoft.Windows.SDK.BuildTools.<版本>\bin\<架构>\signtool.exe
```

**签名（PFX 文件形式，🧑‍💻）**——SHA-256 + RFC3161 时间戳（缺时间戳会导致证书到期后签名失效！）：
```cmd
signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com ^
  /f mycert.pfx /p 你的PFX密码 ^
  /du "https://你的官网" ^
  target\release\pixshot.exe
```

**签名（证书在证书存储/EV 令牌中，🧑‍💻）**：
```cmd
signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com ^
  /sha1 <证书SHA1指纹> ^
  target\release\pixshot.exe
```
指纹获取：`Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Select-Object Subject, Thumbprint`（EV 硬件令牌插入后同样可见）。

**验证**：
```cmd
signtool verify /pa /v target\release\pixshot.exe
```

常用公共时间戳服务器：`http://timestamp.digicert.com`、`http://timestamp.globalsign.com`、`http://timestamp.sectigo.com`。

### 4.3 PowerShell 签名（无需 Windows SDK，🧑‍💻）

```powershell
# 列出本机带"代码签名"用途的证书
Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert | Select-Object Subject, Thumbprint, NotAfter

# 签名（SHA256 + 时间戳）
Set-AuthenticodeSignature -FilePath .\target\release\pixshot.exe `
  -Certificate (Get-ChildItem Cert:\CurrentUser\My -CodeSigningCert) `
  -HashAlgorithm SHA256 `
  -TimestampServer http://timestamp.digicert.com

# 验证：Status 应为 Valid
Get-AuthenticodeSignature .\target\release\pixshot.exe
```

### 4.4 测试环境自签名证书完整流程（企业内网/自测）

**步骤 1**：创建自签代码签名证书（🧑‍💻，证书存于当前用户存储）
```powershell
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
  -Subject "CN=PixShot Dev Test, O=PixShotLab" `
  -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
  -NotAfter (Get-Date).AddYears(3) `
  -CertStoreLocation Cert:\CurrentUser\My
$cert.Thumbprint   # 记下指纹
```

**步骤 2**：导出公钥证书与 PFX（🧑‍💻）
```powershell
Export-Certificate -Cert $cert -FilePath .\PixShotDev.cer
$pwd = Read-Host -AsSecureString -Prompt "设置 PFX 导出密码"
Export-PfxCertificate -Cert $cert -FilePath .\PixShotDev.pfx -Password $pwd
```

**步骤 3**：在**每台需要运行该 exe 的机器**上建立信任（🔴 **管理员** PowerShell；内网批量分发用域组策略推送"受信任的根"与"受信任的发布者"）
```powershell
Import-Certificate -FilePath .\PixShotDev.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath .\PixShotDev.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

**步骤 4**：签名并验证（🧑‍💻）
```powershell
Set-AuthenticodeSignature -FilePath .\target\release\pixshot.exe -Certificate $cert -HashAlgorithm SHA256
Get-AuthenticodeSignature .\target\release\pixshot.exe   # Status: Valid
```

**局限**：未导入信任的机器上警告依旧；SmartScreen 不会给自签证书建立信誉；**仅限开发/内网场景，不可用于对外分发**。

### 4.5 Azure Trusted Signing（现更名 Artifact Signing）——个人开发者正式方案

微软托管的云端签名服务（原 Trusted Signing），证书由微软维护、全球受信任，成本远低于商业证书。官方文档：[Set up signing integrations to use Artifact Signing](https://learn.microsoft.com/en-us/azure/artifact-signing/how-to-signing-integrations)。

**前置**：Azure 订阅 → 开通 Artifact Signing 账户 → 完成身份验证（个人 3 年以上身份证明）→ 创建证书 profile → 为自己分配 **Certificate Profile Signer** 角色。

**步骤 1**：安装客户端工具（🔴 **管理员**，安装 MSI 需提权；winget 会自动请求）
```cmd
winget install -e --id Microsoft.Azure.ArtifactSigningClientTools
```
依赖（MSI 自动安装）：Windows SDK 的 signtool（≥ 10.0.2261.755）、.NET 8 Runtime、VC++ 运行库。

**步骤 2**：创建 `metadata.json`（区域 Endpoint 必须与账户所在区域一致，否则 403）
```json
{
  "Endpoint": "https://wus2.codesigning.azure.net",
  "CodeSigningAccountName": "<你的账户名>",
  "CertificateProfileName": "<你的证书 profile 名>"
}
```
（Endpoint 按账户区域选择，如 West US 2 = `https://wus2.codesigning.azure.net`、North Europe = `https://neu.codesigning.azure.net`，完整列表见官方文档。）

**步骤 3**：签名（🧑‍💻；**必须使用微软时间戳**——其证书仅 3 天有效期，时间戳保证长期有效）
```cmd
signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.acs.microsoft.com ^
  /dlib "C:\Program Files (x86)\Windows Kits\10\bin\<SDK版本>\x64\Azure.CodeSigning.Dlib.dll" ^
  /dmdf C:\sign\metadata.json ^
  target\release\pixshot.exe
```

**步骤 4**：验证
```cmd
signtool verify /pa /v target\release\pixshot.exe
```

### 4.6 签名最佳实践

1. **算法**：一律 SHA-256（`/fd SHA256 /td SHA256`），不再使用 SHA-1；
2. **时间戳**：必加 RFC3161 时间戳，否则证书过期即签名失效；
3. **覆盖面**：exe、DLL、安装包（MSI）全部要签；按目标系统选 OV/EV；
4. **顺序**：签名必须放在构建最后一步（任何修改都会使签名失效）；
5. **CI 集成**（GitHub Actions）：PFX 以 Base64 存入 Secrets，工作流中还原后签名：
```yaml
- name: Decode cert & sign
  run: |
    [IO.File]::WriteAllBytes("cert.pfx", [Convert]::FromBase64String($env:PFX_B64))
    signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /f cert.pfx /p $env:PFX_PWD target\release\pixshot.exe
  env:
    PFX_B64: ${{ secrets.PFX_B64 }}
    PFX_PWD: ${{ secrets.PFX_PWD }}
```
   EV 硬件令牌无法进 CI，可使用 SignPath / SSL.com eSigner 等云签名服务或 GitHub 的 Artifact Signing 集成（[azure/artifact-signing-action](https://github.com/azure/artifact-signing-action)）。

**本仓库 pixshot 一条龙（构建 → 签名 → 验证）**：
```powershell
cargo build --release
signtool sign /fd SHA256 /td SHA256 /tr http://timestamp.digicert.com /f mycert.pfx /p 你的PFX密码 target\release\pixshot.exe
signtool verify /pa target\release\pixshot.exe
```

### 4.7 pixshot.exe 分发落地建议

| 场景 | 建议 |
|---|---|
| 自用 / 测试 | 无需签名；传到其他机器时验证哈希后用方案一 `Unblock-File` 放行 |
| 团队内网分发 | 4.4 自签证书 + 组策略统一推送信任根/受信任发布者 |
| 公开免费发布 | 4.5 Artifact Signing（个人低成本）或 OV 证书起步 |
| 商业产品 | EV 证书（SmartScreen 即时信誉）+ 安装包一并签名 |

---

## 5. 一句话决策

1. 先按第 2 节**验证来源与哈希**——这是所有放行动作的前提；
2. 单文件警告 → **方案一（解除锁定）/ 方案二 2a（仍要运行）**，普通用户权限、风险最低；
3. 你是开发者 → **第 4 节签名是唯一正解**（测试自签、正式用 OV/EV 或 Azure Artifact Signing）；
4. 系统级关闭 SmartScreen / 附件管理器 / UAC（2b-2d、三、四）**只用于隔离测试环境，用完按回退命令恢复**。
