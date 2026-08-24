<#
.SYNOPSIS
    個人利用向けに mdview.exe を自己署名証明書で署名するスクリプト。

.DESCRIPTION
    実行すると、このマシン専用の自己署名コードサイニング証明書を作り、
    mdview.exe に Authenticode 署名を付ける。狙いは「発行元: 不明」の
    代わりに「発行元: mdview local」と表示させる、その程度のもの。

    重要: これは Windows SmartScreen の警告を消すものではない。
    SmartScreen は署名の有無ではなく、その実行ファイル（および証明書）が
    積み上げてきた「評判」で警告を出すかどうかを決めており、自己署名
    証明書はどれだけ使っても評判を積み上げない。また、ここで登録する
    信頼設定はこのマシンのローカルストアに閉じているため、他人の
    マシンに配ったコピーには何の効果もない（そちらでは相変わらず
    「発行元不明」として扱われる）。

    SmartScreen 警告そのものを解消したい場合は、README の
    "Code signing" 節にある OV コードサイニング証明書、または
    Azure Trusted Signing の利用を検討すること。

.PARAMETER ExePath
    署名対象の exe へのパス。既定値は target\release\mdview.exe。

.EXAMPLE
    # 管理者権限の PowerShell から:
    .\scripts\sign-windows-selfsign.ps1
    .\scripts\sign-windows-selfsign.ps1 -ExePath C:\path\to\mdview.exe

.NOTES
    管理者権限の PowerShell で実行すること。
    Cert:\LocalMachine\Root / TrustedPublisher への書き込みに管理者権限が
    必須なため（Cert:\CurrentUser\My への証明書作成自体は非管理者でも可）。
#>
[CmdletBinding()]
param(
    [string]$ExePath = "target\release\mdview.exe"
)

$ErrorActionPreference = "Stop"

# 管理者権限チェック。LocalMachine ストアへの書き込みは管理者権限必須。
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "管理者権限の PowerShell で実行してください（右クリック → 管理者として実行）。"
}

$resolvedExePath = Resolve-Path -Path $ExePath -ErrorAction Stop
Write-Host "署名対象: $resolvedExePath"

# (a) コードサイニング用の自己署名証明書を作る。
#     秘密鍵つきで Cert:\CurrentUser\My に作成される。
Write-Host "自己署名証明書を作成しています..."
$cert = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject "CN=mdview local" `
    -CertStoreLocation Cert:\CurrentUser\My `
    -KeyUsage DigitalSignature `
    -FriendlyName "mdview local code signing" `
    -NotAfter (Get-Date).AddYears(5)

# (b) 作った証明書（公開鍵のみ）を、このマシン上で「信頼された発行元」として
#     扱われるように LocalMachine の Root（信頼されたルート証明機関）と
#     TrustedPublisher（信頼された発行元）の両ストアにインポートする。
#     ※ あくまでこのマシンだけのローカルな信頼設定であり、他人のマシンには
#        一切反映されない。
Write-Host "証明書をこのマシンの信頼ストアに登録しています..."
$certBytes = $cert.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert)
$tempCerPath = Join-Path ([IO.Path]::GetTempPath()) "mdview-local-signing.cer"
[IO.File]::WriteAllBytes($tempCerPath, $certBytes)

try {
    Import-Certificate -FilePath $tempCerPath -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $tempCerPath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
} finally {
    Remove-Item -Path $tempCerPath -Force -ErrorAction SilentlyContinue
}

# (c) mdview.exe に Authenticode 署名を付ける。タイムスタンプサーバーを
#     指定しておくと、証明書の有効期限が切れた後も「署名した時点では
#     有効だった」ことを検証できる。
Write-Host "mdview.exe に署名しています..."
$signResult = Set-AuthenticodeSignature `
    -FilePath $resolvedExePath `
    -Certificate $cert `
    -TimestampServer "http://timestamp.digicert.com"

if ($signResult.Status -ne "Valid") {
    throw "署名に失敗しました: $($signResult.Status) - $($signResult.StatusMessage)"
}

# (d) ダウンロード起源のマーク（Zone.Identifier 代替ストリーム）を外す。
#     このマシンでビルドした直後の exe には通常付いていないが、zip 経由で
#     取得したファイルに対して実行しても無害なので常に呼んでおく。
Unblock-File -Path $resolvedExePath

Write-Host ""
Write-Host "完了: $resolvedExePath に自己署名しました（発行元表示: CN=mdview local）。"
Write-Host "注意: これは『発行元不明』の表示をこのマシン上でだけ改善するものです。"
Write-Host "      SmartScreen の評判ベースの警告は消えず、他人のマシンにも効果はありません。"
