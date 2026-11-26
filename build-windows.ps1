[CmdletBinding()]
param(
    [string] $GtkRoot = 'C:\gtk-build\gtk\x64\release',
    [string] $PopplerRoot,
    [switch] $RunTests
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$cargoCommand = Get-Command cargo.exe -CommandType Application -ErrorAction SilentlyContinue
if (-not $cargoCommand) {
    throw 'Cargo was not found. Install the x64 Rust MSVC toolchain and reopen PowerShell.'
}
$cargo = $cargoCommand.Source
$gtkPrefix = (Resolve-Path -LiteralPath $GtkRoot).Path
$gtkBin = Join-Path $gtkPrefix 'bin'
$gtkLib = Join-Path $gtkPrefix 'lib'
$gtkPkgConfig = Join-Path $gtkLib 'pkgconfig'
if (-not (Test-Path -LiteralPath (Join-Path $gtkPkgConfig 'gtk4.pc') -PathType Leaf)) {
    throw 'GtkRoot must point to an x64 MSVC GTK 4 installation with lib\pkgconfig\gtk4.pc.'
}

$popplerPrefix = $null
if ($PopplerRoot) {
    $popplerPrefix = (Resolve-Path -LiteralPath $PopplerRoot).Path
    foreach ($program in @('pdfinfo.exe', 'pdftotext.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $popplerPrefix "bin\$program") -PathType Leaf)) {
            throw "PopplerRoot must contain bin\$program."
        }
    }
}

$previousEnvironment = @{
    Path = $env:Path
    Lib = $env:Lib
    PKG_CONFIG = $env:PKG_CONFIG
    PKG_CONFIG_PATH = $env:PKG_CONFIG_PATH
}

try {
    $env:Path = "$gtkBin;$env:Path"
    $env:Lib = "$gtkLib;$env:Lib"
    $env:PKG_CONFIG_PATH = "$gtkPkgConfig;$env:PKG_CONFIG_PATH"
    if (-not $env:PKG_CONFIG) {
        $pkgConfig = Get-Command pkg-config.exe, pkgconf.exe -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if (-not $pkgConfig) {
            throw 'Install pkg-config or pkgconf for the MSVC GTK installation.'
        }
        $env:PKG_CONFIG = $pkgConfig.Source
    }
    & $env:PKG_CONFIG --atleast-version=4.12 gtk4
    if ($LASTEXITCODE -ne 0) {
        throw 'GTK 4.12 or newer must be visible to pkg-config.'
    }

    $target = 'x86_64-pc-windows-msvc'
    $targetDir = Join-Path $PSScriptRoot 'target'
    $manifest = Join-Path $PSScriptRoot 'Cargo.toml'
    Push-Location -LiteralPath $PSScriptRoot
    try {
        if ($RunTests) {
            & $cargo test --locked --target $target --target-dir $targetDir --manifest-path $manifest
            if ($LASTEXITCODE -ne 0) {
                throw 'The Windows tests failed.'
            }
        }
        & $cargo build --release --locked --target $target --target-dir $targetDir --manifest-path $manifest
        if ($LASTEXITCODE -ne 0) {
            throw 'The Windows build failed.'
        }
    } finally {
        Pop-Location
    }

    $stage = Join-Path $targetDir (Join-Path 'windows-package' ([guid]::NewGuid().ToString('N')))
    $package = Join-Path $stage 'Clueless'
    $bin = Join-Path $package 'bin'
    New-Item -ItemType Directory -Path $bin | Out-Null
    Copy-Item -LiteralPath (Join-Path $targetDir "$target\release\clueless.exe") -Destination $bin
    $dlls = @(Get-ChildItem -LiteralPath $gtkBin -Filter '*.dll' -File)
    if ($dlls.Count -eq 0) {
        throw "No GTK DLLs found in $gtkBin."
    }
    foreach ($dll in $dlls) {
        Copy-Item -LiteralPath $dll.FullName -Destination $bin
    }
    foreach ($dir in @('share', 'lib\gdk-pixbuf-2.0', 'lib\gio')) {
        $source = Join-Path $gtkPrefix $dir
        if (Test-Path -LiteralPath $source -PathType Container) {
            $destination = Join-Path $package $dir
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            Copy-Item -LiteralPath $source -Destination $destination -Recurse
        }
    }
    if ($popplerPrefix) {
        Copy-Item -LiteralPath $popplerPrefix -Destination (Join-Path $package 'poppler') -Recurse
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'README.md') -Destination $package

    $archive = Join-Path $targetDir 'clueless-win11-x64.zip'
    Compress-Archive -LiteralPath $package -DestinationPath $archive -Force
    Write-Output "Package: $archive"
    Write-Output "Launch:  $(Join-Path $bin 'clueless.exe')"
} finally {
    foreach ($name in $previousEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process')
    }
}
