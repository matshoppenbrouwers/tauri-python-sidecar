<#
.SYNOPSIS
    Sign Windows executables with a certificate stored in Azure Key Vault.

.DESCRIPTION
    Uses AzureSignTool to sign EXE/MSI files with an Azure Key Vault-stored
    certificate (Azure Trusted Signing / Azure Code Signing).

    The important design property: when the signing environment variables are
    NOT set, this script prints a warning and exits 0 instead of failing. A
    clone of this template must build end to end with no signing infrastructure
    at all, and CI must not go red just because a fork has no secrets. Signing
    is an opt-in overlay, never a build prerequisite.

.PARAMETER FilePath
    Path to the file to sign.

.PARAMETER Description
    Description embedded in the signature (optional).

.EXAMPLE
    ./sign.ps1 -FilePath "src-tauri/binaries/py-sidecar-x86_64-pc-windows-msvc.exe"

.EXAMPLE
    ./sign.ps1 -FilePath "tauri-python-sidecar_0.1.0_x64-setup.exe" -Description "Installer"

.NOTES
    Required environment variables (set them as GitHub secrets for CI):
    - AZURE_KEY_VAULT_URI: the vault's DNS name, copied from the Azure portal
                           ("Vault URI" on the Key Vault overview blade)
    - AZURE_TENANT_ID:     Entra ID tenant ID
    - AZURE_CLIENT_ID:     Service principal client ID
    - AZURE_CLIENT_SECRET: Service principal client secret
    - AZURE_CERT_NAME:     Certificate name inside the Key Vault

    None of these are baked into this file on purpose. A committed vault URI is
    a standing invitation to point someone else's build at your signing account.
#>

param(
    [Parameter(Mandatory=$true)]
    [string]$FilePath,

    [Parameter(Mandatory=$false)]
    [string]$Description = "TODO_YOUR_APP_NAME"
)

$ErrorActionPreference = "Stop"

# Validate file exists
if (-not (Test-Path $FilePath)) {
    Write-Error "File not found: $FilePath"
    exit 1
}

# Get credentials from environment
$KeyVaultUri = $env:AZURE_KEY_VAULT_URI
$TenantId = $env:AZURE_TENANT_ID
$ClientId = $env:AZURE_CLIENT_ID
$ClientSecret = $env:AZURE_CLIENT_SECRET
$CertName = $env:AZURE_CERT_NAME

# Validate required environment variables
$missingVars = @()
if (-not $KeyVaultUri) { $missingVars += "AZURE_KEY_VAULT_URI" }
if (-not $TenantId) { $missingVars += "AZURE_TENANT_ID" }
if (-not $ClientId) { $missingVars += "AZURE_CLIENT_ID" }
if (-not $ClientSecret) { $missingVars += "AZURE_CLIENT_SECRET" }
if (-not $CertName) { $missingVars += "AZURE_CERT_NAME" }

if ($missingVars.Count -gt 0) {
    # Use GitHub Actions workflow commands for visibility in CI logs
    Write-Host "##[warning]CODE SIGNING SKIPPED - Missing credentials"
    Write-Host "##[warning]Missing environment variables: $($missingVars -join ', ')"
    Write-Host ""
    Write-Host "The following file will NOT be signed: $FilePath"
    Write-Host ""
    Write-Host "To enable signing, configure these as GitHub secrets:"
    foreach ($var in $missingVars) {
        Write-Host "  - $var"
    }
    Write-Host ""
    # Exit successfully so an unsigned build is still a usable build.
    # This is what lets a fresh clone, and any fork without secrets, build.
    exit 0
}

# Install AzureSignTool if not present
if (-not (Get-Command AzureSignTool -ErrorAction SilentlyContinue)) {
    Write-Host "Installing AzureSignTool..."
    dotnet tool install --global AzureSignTool
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Failed to install AzureSignTool"
        exit 1
    }
}

Write-Host "Signing: $FilePath"
Write-Host "  Certificate: $CertName"
Write-Host "  Key Vault: $KeyVaultUri"

# Sign the file.
# --timestamp-rfc3161 matters more than it looks: without a countersigned
# timestamp the signature stops validating the day the certificate expires,
# retroactively, for every copy already installed.
$signArgs = @(
    "sign",
    "--azure-key-vault-url", $KeyVaultUri,
    "--azure-key-vault-tenant-id", $TenantId,
    "--azure-key-vault-client-id", $ClientId,
    "--azure-key-vault-client-secret", $ClientSecret,
    "--azure-key-vault-certificate", $CertName,
    "--timestamp-rfc3161", "http://timestamp.sectigo.com",
    "--timestamp-digest", "sha256",
    "--file-digest", "sha256",
    "--description", $Description,
    "--description-url", "https://example.com",
    "--verbose",
    $FilePath
)

& AzureSignTool @signArgs

if ($LASTEXITCODE -ne 0) {
    Write-Error "Signing failed for $FilePath (exit code: $LASTEXITCODE)"
    exit 1
}

Write-Host "Signed successfully: $FilePath"

# Verify signature
Write-Host "Verifying signature..."
$sig = Get-AuthenticodeSignature -FilePath $FilePath
if ($sig.Status -eq "Valid") {
    Write-Host "Signature verified: $($sig.SignerCertificate.Subject)"
    Write-Host "Signature is VALID and trusted."
} else {
    # Not fatal: a brand-new certificate is genuinely untrusted until it has
    # built SmartScreen reputation, and that takes downloads, not fixes.
    Write-Host "##[warning]Signature verification status: $($sig.Status)"
    Write-Host "##[warning]This may cause Windows SmartScreen warnings for users."
    Write-Host ""
    Write-Host "Common causes:"
    Write-Host "  - New certificate not yet trusted (needs reputation building)"
    Write-Host "  - Using a test/development certificate"
    Write-Host "  - Certificate chain not fully trusted on this machine"
}
