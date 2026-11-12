<#
Exercise the file-open path in a packaged Windows ZIP. Use -VerifyPlayback
on a desktop with a working audio output to also check that the clock advances.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $PackagePath,
    [switch] $VerifyPlayback
)

$ErrorActionPreference = 'Stop'
$package = (Resolve-Path -LiteralPath $PackagePath).ProviderPath
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Write-TestMp3([string] $Path, [switch] $OversizedCover) {
    $file = [IO.BinaryWriter]::new([IO.File]::Create($Path))
    try {
        if ($OversizedCover) {
            # A 65535x1, 1-bit BMP fits in a small ID3 APIC frame but exceeds GPU texture limits.
            $bitmap = [byte[]]::new(62 + 8192)
            $bitmap[0] = 0x42
            $bitmap[1] = 0x4d
            [BitConverter]::GetBytes($bitmap.Length).CopyTo($bitmap, 2)
            $bitmap[10] = 62
            $bitmap[14] = 40
            [BitConverter]::GetBytes(65535).CopyTo($bitmap, 18)
            $bitmap[22] = 1
            $bitmap[26] = 1
            $bitmap[28] = 1
            $pictureHeader = [Text.Encoding]::ASCII.GetBytes("`0image/bmp`0$([char]3)`0")
            $pictureSize = $pictureHeader.Length + $bitmap.Length
            $tagSize = 10 + $pictureSize
            [byte[]] $tagHeader = @(0x49, 0x44, 0x33, 3, 0, 0, 0, 0, ($tagSize -shr 7), ($tagSize -band 0x7f))
            [byte[]] $frameHeader = @(0x41, 0x50, 0x49, 0x43, 0, 0, ($pictureSize -shr 8), ($pictureSize -band 0xff), 0, 0)
            $file.Write($tagHeader)
            $file.Write($frameHeader)
            $file.Write($pictureHeader)
            $file.Write($bitmap)
        }

        # MPEG-1 Layer III at 44.1 kHz, 128 kbps stereo. Zero side info and main data decode to silence.
        $frame = [byte[]]::new(417)
        $frame[0] = 0xff
        $frame[1] = 0xfb
        $frame[2] = 0x90
        for ($index = 0; $index -lt 120; $index++) {
            $file.Write($frame)
        }
    } finally {
        $file.Dispose()
    }
}

function Start-TestPlayer([string] $Track) {
    $start = [Diagnostics.ProcessStartInfo]::new($exe)
    $start.UseShellExecute = $false
    $start.WorkingDirectory = $scratch
    $start.Environment['LOCALAPPDATA'] = $localAppData
    $start.Environment['MUSICPLAYER_SETTINGS_PATH'] = Join-Path $scratch 'app.ron'
    $start.ArgumentList.Add($Track)
    [Diagnostics.Process]::Start($start)
}

function Wait-PlayerText([Diagnostics.Process] $Player, [string] $Name) {
    $match = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty, $Name
    )
    $timer = [Diagnostics.Stopwatch]::StartNew()
    do {
        $Player.Refresh()
        if ($Player.HasExited) { throw "Packaged player exited with $($Player.ExitCode) while waiting for '$Name'." }
        if ($Player.MainWindowHandle -ne [IntPtr]::Zero) {
            try {
                $window = [Windows.Automation.AutomationElement]::FromHandle($Player.MainWindowHandle)
                if ($window.FindFirst([Windows.Automation.TreeScope]::Descendants, $match)) { return }
            } catch [Windows.Automation.ElementNotAvailableException] {
                # The next repaint may rebuild the accessibility node.
            }
        }
        Start-Sleep -Milliseconds 125
    } while ($timer.Elapsed.TotalSeconds -lt 20)
    throw "The first MusicPlayer window did not show '$Name'."
}

$scratch = [IO.Directory]::CreateTempSubdirectory('MusicPlayer-smoke-').FullName
$listeners = @()
$primary = $secondary = $null
try {
    $packageDirectory = Join-Path $scratch 'package'
    [IO.Compression.ZipFile]::ExtractToDirectory($package, $packageDirectory)
    $exe = Join-Path $packageDirectory 'MusicPlayer.exe'
    $localAppData = (New-Item -ItemType Directory -Path (Join-Path $scratch 'local')).FullName
    $firstTitle = 'Initial smoke track'
    $secondTitle = 'Café smoke [oversized cover]'
    $first = Join-Path $scratch "$firstTitle.mp3"
    $second = Join-Path $scratch "$secondTitle.mp3"
    Write-TestMp3 $first
    Write-TestMp3 $second -OversizedCover

    foreach ($port in @(49677, 39677)) {
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port)
        $listener.ExclusiveAddressUse = $true
        try {
            $listener.Start()
            $listeners += $listener
        } catch {
            $listener.Stop()
            if ($_.Exception.GetBaseException().SocketErrorCode -notin @('AddressAlreadyInUse', 'AccessDenied')) { throw }
        }
    }

    $primary = Start-TestPlayer $first
    Wait-PlayerText $primary $firstTitle
    $secondary = Start-TestPlayer $second
    if (!$secondary.WaitForExit(8000)) { throw 'The second launch did not hand off its MP3 and exit.' }
    if ($secondary.ExitCode -ne 0) { throw "The second launch exited with $($secondary.ExitCode)." }
    Wait-PlayerText $primary $secondTitle
    if ($VerifyPlayback) { Wait-PlayerText $primary '0:03 / 0:03' }
    if ($listeners | Where-Object { $_.Pending() }) { throw 'MusicPlayer connected to a legacy port.' }
    Write-Host 'Packaged MP3 startup and handoff passed.'
} finally {
    foreach ($player in @($secondary, $primary)) {
        if ($null -eq $player) { continue }
        if (!$player.HasExited -and (!$player.CloseMainWindow() -or !$player.WaitForExit(2000))) {
            $player.Kill()
            $null = $player.WaitForExit(2000)
        }
        $player.Dispose()
    }
    foreach ($listener in $listeners) { $listener.Stop() }
    $resolved = (Resolve-Path -LiteralPath $scratch).ProviderPath
    $tempRoot = [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetTempPath())
    if ($resolved -eq $scratch -and [IO.Path]::GetDirectoryName($resolved) -eq $tempRoot) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
