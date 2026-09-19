<#
borhan installer, Windows.

  powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/pouriya/borhan/master/install.ps1 | iex"

The same job install.sh does on Unix: download a release, check it against its
published checksum, put the binary in a directory on your PATH. What differs is
the PATH. There is no ~/.local/bin convention on Windows and no rc file to tell
you to edit, so the directory is added to your user PATH here, and only a new
terminal sees it.

It sets nothing up beyond that: whether this machine keeps memories or only
talks to a server that does is a decision, and `borhan --help` is where it is
made.

Knobs, all optional, all environment variables so they match install.sh:
  BORHAN_VERSION   version to install, without the leading v  (default: latest)
  BORHAN_BIN_DIR   where borhan.exe goes           (default: %LOCALAPPDATA%\Programs\borhan)
  BORHAN_ARCHIVE   install this local .zip, skip the download entirely
  BORHAN_REPO      owner/name to download from                (default: pouriya/borhan)
#>
$ErrorActionPreference = 'Stop'

$repo = if ($env:BORHAN_REPO) { $env:BORHAN_REPO } else { 'pouriya/borhan' }
$binDir = if ($env:BORHAN_BIN_DIR) { $env:BORHAN_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\borhan' }

# One build, and ARM64 deliberately gets it too. Windows on ARM runs x64 binaries
# under emulation, and a second target would be a release asset nobody has ever
# run.
switch ($env:PROCESSOR_ARCHITECTURE) {
    { $_ -in 'AMD64', 'ARM64' } { $target = 'x86_64-pc-windows-msvc' }
    default {
        Write-Error "borhan: no release for $env:PROCESSOR_ARCHITECTURE`nbuilt for: windows x86_64 (which ARM64 runs too)`non anything else, build from source: https://github.com/$repo"
    }
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("borhan-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    if ($env:BORHAN_ARCHIVE) {
        Write-Host "borhan: unpacking $env:BORHAN_ARCHIVE"
        Expand-Archive -Path $env:BORHAN_ARCHIVE -DestinationPath $tmp -Force
    }
    else {
        # Windows PowerShell 5.1 still defaults to protocols GitHub hangs up on.
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

        $version = $env:BORHAN_VERSION
        if (-not $version) {
            $latest = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/latest"
            $version = $latest.tag_name -replace '^v', ''
            if (-not $version) { Write-Error "borhan: no release published for $repo yet" }
        }

        $name = "borhan-$version-$target.zip"
        $url = "https://github.com/$repo/releases/download/v$version/$name"
        Write-Host "borhan: downloading $name"
        $archive = Join-Path $tmp $name
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archive
        Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile "$archive.sha256"

        # The .sha256 is `<hex>  <name>`, the shape sha256sum writes on the build
        # machine; only the hex half means anything here.
        $want = ((Get-Content "$archive.sha256" -Raw).Trim() -split '\s+')[0]
        $have = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash
        if ($want -ne $have) {
            Write-Error "borhan: checksum mismatch on $name - refusing to install"
        }

        Expand-Archive -Path $archive -DestinationPath $tmp -Force
    }

    $src = Get-ChildItem -Path $tmp -Directory -Filter 'borhan-*' | Select-Object -First 1
    if (-not $src -or -not (Test-Path (Join-Path $src.FullName 'borhan.exe'))) {
        Write-Error "borhan: archive does not look like a borhan release"
    }

    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    $binary = Join-Path $binDir 'borhan.exe'

    # Windows will not let anything write to a running image, and unlike Unix it
    # will not let you rename over one either. It does allow renaming the running
    # image itself away, which is the whole trick: a `borhan serve` keeps running
    # from the moved file, the new binary takes the real path, and the leftover
    # is swept on the next install once that server has stopped.
    Get-ChildItem -Path $binDir -Filter 'borhan.exe.old*' -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item $_.FullName -Force -ErrorAction SilentlyContinue }
    if (Test-Path $binary) {
        try { Remove-Item $binary -Force }
        catch { Move-Item $binary "$binary.old.$(Get-Random)" -Force }
    }
    Copy-Item (Join-Path $src.FullName 'borhan.exe') $binary -Force

    Write-Host "borhan: installed $(& $binary --version) at $binary"

    # The user PATH, read from and written to the registry rather than $env:Path,
    # which is this process's merged copy of the machine and user values: writing
    # that back would copy the machine PATH into the user one.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @()
    if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
    if ($entries -notcontains $binDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $binDir) -join ';'), 'User')
        Write-Host "borhan: added $binDir to your user PATH - open a new terminal to pick it up"
    }

    Write-Host @"

Next, start here:

  borhan --help

Or read Getting started:

  https://github.com/pouriya/borhan#getting-started
"@
}
finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
