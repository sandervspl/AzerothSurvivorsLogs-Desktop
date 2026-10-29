[CmdletBinding()]
param(
    [string] $Version = '',
    [string] $Output = 'artifacts/desktop-release',
    [string] $Vpk = 'vpk',
    [switch] $Unsigned,
    [switch] $Development,
    [switch] $SkipBuild
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).ProviderPath
Push-Location -LiteralPath $repo
try {
    if ($Development -and $Unsigned) { throw 'Development packages are already unsigned; do not mix release modes.' }
    $cargoVersion = [regex]::Match((Get-Content Cargo.toml -Raw), '(?m)^version = "([^"]+)"').Groups[1].Value
    if (-not $Version) { $Version = $cargoVersion }
    if ($Version -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') { throw 'Use a semantic release version.' }
    if (-not $Development) {
        if ($SkipBuild) { throw 'Public packages must rebuild their embedded trust configuration.' }
        if ($Version -ne $cargoVersion) { throw 'Release package version must match Cargo.toml.' }
        if (-not $env:AZS_RELEASE_PUBLIC_KEYS -or -not $env:AZS_RELEASE_CATALOG_URL) { throw 'Configure release public keys and catalog URL before a production build.' }
        $trustEntries = @($env:AZS_RELEASE_PUBLIC_KEYS -split ',')
        if (@($trustEntries | Where-Object { $_ -notmatch '^[A-Za-z0-9_-]{1,64}:[a-fA-F0-9]{64}$' }).Count -or @($trustEntries | ForEach-Object { ($_ -split ':')[0] } | Select-Object -Unique).Count -ne $trustEntries.Count) { throw 'Invalid or duplicate release trust roots.' }
        $catalogUri = [uri]$env:AZS_RELEASE_CATALOG_URL
        if ($catalogUri.Scheme -ne 'https' -or $catalogUri.UserInfo -or $catalogUri.Query -or $catalogUri.Fragment) { throw 'Public release catalogs require a plain HTTPS URL.' }
        if ($Unsigned -and $env:AZS_WINDOWS_SIGNING_METADATA) { throw 'Unsigned mode conflicts with Windows signing metadata; choose one mode explicitly.' }
        if (-not $Unsigned -and (-not $env:AZS_WINDOWS_SIGNING_METADATA -or -not (Test-Path -LiteralPath $env:AZS_WINDOWS_SIGNING_METADATA -PathType Leaf))) { throw 'Configure Windows signing metadata or explicitly select -Unsigned.' }
    }
    $buildProfile = if ($Development) { 'debug' } else { 'release' }
    if (-not $SkipBuild) {
        $build = @('build', '--locked', '--workspace')
        if (-not $Development) { $build += '--release' }
        & cargo @build
        if ($LASTEXITCODE -ne 0) { throw 'Desktop build failed.' }
    }
    $target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $repo 'target' }
    if (-not $Development) {
        $embeddedText = & (Join-Path $target 'release/azs-upload.exe') release-config
        if ($LASTEXITCODE) { throw 'Production update trust is not configured in the release binary.' }
        $embedded = $embeddedText | ConvertFrom-Json
        $expectedKeys = @($trustEntries | ForEach-Object { $parts = $_ -split ':'; $parts[0] + ':' + $parts[1].ToLowerInvariant() } | Sort-Object)
        $actualKeys = @($embedded.publicKeys.PSObject.Properties | ForEach-Object { $_.Name + ':' + $_.Value } | Sort-Object)
        if ($embedded.catalogUrl -ne $env:AZS_RELEASE_CATALOG_URL -or (Compare-Object $expectedKeys $actualKeys)) { throw 'Compiled release trust differs from the packaging configuration.' }
    }
    $out = [IO.Path]::GetFullPath((Join-Path $repo $Output))
    if (-not $out.StartsWith($repo + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw 'Package output must stay within this workspace.' }
    $stage = Join-Path $out "input-$Version"
    if (Test-Path -LiteralPath $stage) { throw 'Choose a fresh output directory; existing package input is preserved.' }
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    foreach ($name in @('azs-uploader.exe', 'azs-update-guard.exe')) {
        Copy-Item -LiteralPath (Join-Path $target "$buildProfile/$name") -Destination (Join-Path $stage $name)
    }
    $packageId = if ($Development) { 'AZSLogsUploaderDevelopment' } else { 'AZSLogsUploader' }
    $packageTitle = if ($Development) { 'Azeroth Survivors Uploader Development' } else { 'Azeroth Survivors Uploader' }
    $pack = @('pack', '--packId', $packageId, '--packTitle', $packageTitle, '--packAuthors', 'AZS Logs', '--packVersion', $Version, '--packDir', $stage, '--mainExe', 'azs-uploader.exe', '--channel', 'stable', '--runtime', 'win-x64', '--delta', 'None', '--shortcuts', 'StartMenuRoot', '--outputDir', $out)
    if (-not $Development -and -not $Unsigned) { $pack += @('--azureTrustedSignFile', $env:AZS_WINDOWS_SIGNING_METADATA) }
    & $Vpk @pack
    if ($LASTEXITCODE -ne 0) { throw 'Managed desktop packaging failed.' }
    $env:AZS_MANAGED_PACKAGE_DIR = $out
    $env:AZS_MANAGED_PACKAGE_DEVELOPMENT = if ($Development) { '1' } else { '0' }
    & python scripts/verify-managed-package.py
    if ($LASTEXITCODE -ne 0) { throw 'Managed package verification failed.' }
    if (-not $Development) {
        $verification = Get-Content -LiteralPath (Join-Path $out 'managed-package-verification.json') -Raw | ConvertFrom-Json
        $signatures = @()
        foreach ($file in $verification.signatureFiles) {
            $signature = Get-AuthenticodeSignature -LiteralPath $file
            if (-not $Unsigned -and $signature.Status -ne 'Valid') { throw "Invalid Windows publisher signature: $file" }
            $entry = @{ file = [IO.Path]::GetFileName($file); status = $signature.Status.ToString() }
            if ($signature.SignerCertificate) { $entry.publisher = $signature.SignerCertificate.Subject; $entry.thumbprint = $signature.SignerCertificate.Thumbprint }
            $signatures += $entry
        }
        $signatures | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'publisher-signature-verification.json') -Encoding UTF8
        $installer = Get-Item -LiteralPath (Join-Path $out 'AZSLogsUploader-stable-Setup.exe')
        $signingMode = if ($Unsigned) { 'unsigned' } else { 'authenticode' }
        @{ version = $Version; filename = $installer.Name; size = $installer.Length; sha256 = (Get-FileHash -LiteralPath $installer.FullName -Algorithm SHA256).Hash.ToLowerInvariant(); codeSigning = $signingMode } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'installer.json') -Encoding UTF8
        $embedded | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $out 'embedded-update-trust.json') -Encoding UTF8
    }
} finally { Pop-Location }
