<#
.SYNOPSIS
    Signs markdown-remarkable.exe with a self-signed certificate, for personal use.

.DESCRIPTION
    Running this creates a self-signed code-signing certificate scoped to this
    machine and applies an Authenticode signature to markdown-remarkable.exe.
    The goal is only to swap "Publisher: Unknown" for
    "Publisher: markdown-remarkable local" — nothing more.

    Important: this does NOT make Windows SmartScreen's warning go away.
    SmartScreen decides whether to warn based on the "reputation" that
    executable (and certificate) has built up, not on whether it's signed at
    all, and a self-signed certificate never builds up reputation no matter
    how many times it's used. The trust settings registered here are also
    confined to this machine's local store, so they have no effect on a copy
    handed to someone else's machine (it will still show up there as
    "unknown publisher").

    If you actually want to get rid of the SmartScreen warning itself, look
    into an OV code-signing certificate or Azure Trusted Signing, both
    covered in the README's "Code signing" section.

.PARAMETER ExePath
    Path to the exe to sign. Defaults to target\release\markdown-remarkable.exe.

.EXAMPLE
    # From an elevated PowerShell prompt:
    .\scripts\sign-windows-selfsign.ps1
    .\scripts\sign-windows-selfsign.ps1 -ExePath C:\path\to\markdown-remarkable.exe

.NOTES
    Run this from an elevated PowerShell prompt.
    Writing to Cert:\LocalMachine\Root / TrustedPublisher requires
    administrator privileges (creating the certificate itself in
    Cert:\CurrentUser\My does not).
#>
[CmdletBinding()]
param(
    [string]$ExePath = "target\release\markdown-remarkable.exe"
)

$ErrorActionPreference = "Stop"

# Check for administrator privileges. Writing to the LocalMachine store requires them.
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Please run this from an elevated PowerShell prompt (right-click -> Run as administrator)."
}

$resolvedExePath = Resolve-Path -Path $ExePath -ErrorAction Stop
Write-Host "Signing target: $resolvedExePath"

# (a) Create a self-signed code-signing certificate.
#     Created with its private key in Cert:\CurrentUser\My.
Write-Host "Creating self-signed certificate..."
$cert = New-SelfSignedCertificate `
    -Type CodeSigningCert `
    -Subject "CN=markdown-remarkable local" `
    -CertStoreLocation Cert:\CurrentUser\My `
    -KeyUsage DigitalSignature `
    -FriendlyName "markdown-remarkable local code signing" `
    -NotAfter (Get-Date).AddYears(5)

# (b) Import the certificate (public key only) into both the LocalMachine
#     Root (trusted root certification authorities) and TrustedPublisher
#     stores, so it's treated as a trusted publisher on this machine.
#     Note: this is purely a local trust setting for this machine and has
#     no effect whatsoever on anyone else's machine.
Write-Host "Registering the certificate in this machine's trust stores..."
$certBytes = $cert.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert)
$tempCerPath = Join-Path ([IO.Path]::GetTempPath()) "markdown-remarkable-local-signing.cer"
[IO.File]::WriteAllBytes($tempCerPath, $certBytes)

try {
    Import-Certificate -FilePath $tempCerPath -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
    Import-Certificate -FilePath $tempCerPath -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
} finally {
    Remove-Item -Path $tempCerPath -Force -ErrorAction SilentlyContinue
}

# (c) Apply an Authenticode signature to markdown-remarkable.exe. Specifying
#     a timestamp server lets the signature still verify as "valid at the
#     time of signing" even after the certificate itself expires.
Write-Host "Signing markdown-remarkable.exe..."
$signResult = Set-AuthenticodeSignature `
    -FilePath $resolvedExePath `
    -Certificate $cert `
    -TimestampServer "http://timestamp.digicert.com"

if ($signResult.Status -ne "Valid") {
    throw "Signing failed: $($signResult.Status) - $($signResult.StatusMessage)"
}

# (d) Remove the "downloaded from the internet" mark (the Zone.Identifier
#     alternate data stream). An exe you just built on this machine
#     normally doesn't have one, but it's harmless to always call this,
#     since it also covers an exe obtained via a downloaded zip.
Unblock-File -Path $resolvedExePath

Write-Host ""
Write-Host "Done: self-signed $resolvedExePath (publisher shown as: CN=markdown-remarkable local)."
Write-Host "Note: this only improves the 'unknown publisher' display on this machine."
Write-Host "      It does not remove SmartScreen's reputation-based warning, and has no effect on other machines."
