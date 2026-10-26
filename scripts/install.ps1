# Install the latest novacode release on Windows (L11.14):
#   irm https://raw.githubusercontent.com/Novaturiente/novacode/main/scripts/install.ps1 | iex
# Puts novacode.exe in $env:NOVACODE_BIN_DIR (default %LOCALAPPDATA%\novacode\bin) and adds
# that folder to your user PATH. Verifies the checksum. Never touches ~/.novacode.
# novacode runs commands in Git Bash: install Git for Windows first. Update later with `novacode update`.
$ErrorActionPreference = 'Stop'
$repo = if ($env:NOVACODE_REPO) { $env:NOVACODE_REPO } else { 'Novaturiente/novacode' }
$bin = if ($env:NOVACODE_BIN_DIR) { $env:NOVACODE_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'novacode\bin' }
if (-not [Environment]::Is64BitOperatingSystem) { throw 'novacode has a release build for 64-bit Windows only' }
$target = 'x86_64-pc-windows-msvc'
$tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
if (-not $tag) { throw "could not find the latest release of $repo" }
$asset = "novacode-$($tag.TrimStart('v'))-$target.zip"
$url = "https://github.com/$repo/releases/download/$tag/$asset"
$tmp = Join-Path ([IO.Path]::GetTempPath()) "novacode-install-$PID"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
    Write-Host "downloading $asset"
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile (Join-Path $tmp "$asset.sha256")
    $want = ((Get-Content -Raw (Join-Path $tmp "$asset.sha256")) -split '\s+')[0].ToLower()
    $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLower()
    if ($want -ne $got) { throw 'checksum mismatch; nothing installed' }
    Expand-Archive -Force -Path (Join-Path $tmp $asset) -DestinationPath $tmp
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    Move-Item -Force (Join-Path $tmp 'novacode.exe') (Join-Path $bin 'novacode.exe')
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Write-Host "installed novacode $tag to $bin\novacode.exe"
$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not (($path -split ';') -contains $bin)) {
    [Environment]::SetEnvironmentVariable('Path', ($(if ($path) { "$path;" } else { '' }) + $bin), 'User')
    Write-Host "added $bin to your user PATH; open a new terminal to use it"
}
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host 'novacode runs commands in Git Bash: install Git for Windows from https://git-scm.com/download/win'
}
