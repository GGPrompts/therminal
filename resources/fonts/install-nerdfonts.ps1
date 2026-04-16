# Install Nerd Font Mono families used by therminal's font selector.
# Run from PowerShell (Windows): .\install-nerdfonts.ps1
# Requires internet access — downloads from github.com/ryanoasis/nerd-fonts.

param(
    [string[]]$Fonts = @(
        "JetBrainsMono",
        "FiraCode",
        "CascadiaCode",
        "Hack",
        "Inconsolata",
        "SourceCodePro",
        "UbuntuMono",
        "Iosevka",
        "RobotoMono",
        "Meslo"
    ),
    [switch]$AllWeights
)

$fontDir = "$env:LOCALAPPDATA\Microsoft\Windows\Fonts"
$regPath = "HKCU:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts"
if (!(Test-Path $fontDir)) { New-Item -ItemType Directory -Path $fontDir -Force | Out-Null }

foreach ($font in $Fonts) {
    Write-Host "Installing $font Nerd Font..." -ForegroundColor Cyan
    $url = "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/$font.zip"
    $zip = "$env:TEMP\$font.zip"
    $dir = "$env:TEMP\NF_$font"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        if (Test-Path $dir) { Remove-Item $dir -Recurse -Force }
        Expand-Archive -Path $zip -DestinationPath $dir -Force
        $filter = if ($AllWeights) {
            { $_.Name -match "Mono" -and $_.Name -notmatch "Propo" }
        } else {
            { $_.Name -match "Mono" -and $_.Name -match "Regular" -and $_.Name -notmatch "Propo" }
        }
        $installed = 0
        Get-ChildItem "$dir\*.ttf" | Where-Object $filter | ForEach-Object {
            Copy-Item $_.FullName $fontDir -Force
            $destPath = Join-Path $fontDir $_.Name
            New-ItemProperty -Path $regPath -Name $_.BaseName -Value $destPath -PropertyType String -Force | Out-Null
            $installed++
        }
        Write-Host "  -> $installed TTFs installed" -ForegroundColor Green
        Remove-Item $zip -Force -ErrorAction SilentlyContinue
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
    } catch {
        Write-Host "  -> FAILED: $_" -ForegroundColor Red
    }
}
Write-Host "`nDone! Restart therminal to pick up new fonts." -ForegroundColor Cyan
