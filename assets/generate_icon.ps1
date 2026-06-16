# Regenerates assets/icon.ico (multi-resolution, BMP-DIB + PNG) and
# assets/icon.png (256 px, for the README) from the BSP layout described
# in assets/icon.svg.
#
# Run from anywhere:
#     pwsh -File assets/generate_icon.ps1
#
# This is a one-shot tool — the build does NOT call it. We commit the
# generated `.ico` and `.png` so a fresh checkout builds with no extra
# tooling. Only re-run when the design changes.

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing

# --- design constants --------------------------------------------------------

# Brand colour = default `focused_border_color` from assets/config.toml.
$BrandColor = [System.Drawing.Color]::FromArgb(0xFF, 0x4F, 0xC3, 0xF7)

# BSP tiles in normalised [0..1] coordinates. The four rects below are the
# leaf rectangles of a three-split dwindle tree:
#   V-split 50/50, then on the right H-split 50/50, then on the bottom-right
#   V-split 50/50.
$Tiles = @(
    @{ x = 0.00; y = 0.00; w = 0.50; h = 1.00 },   # Tile 1 — left half
    @{ x = 0.50; y = 0.00; w = 0.50; h = 0.50 },   # Tile 2 — upper right
    @{ x = 0.50; y = 0.50; w = 0.25; h = 0.50 },   # Tile 3 — lower-right left
    @{ x = 0.75; y = 0.50; w = 0.25; h = 0.50 }    # Tile 4 — lower-right right
)

# ICO sizes Windows actually picks from. <= 64 go in as BMP-DIB so older
# loaders (and rc.exe in any oddball SDK build) accept them; 128 / 256 go
# in as PNG to keep the file small.
$BmpSizes = 16, 24, 32, 48, 64
$PngSizes = 128, 256

# --- helpers -----------------------------------------------------------------

function New-DwmendBitmap {
    param([int]$Size)

    # Adaptive metrics. At 16 px the gap+margin collapse to 1 px and the
    # corner radius to ~1 px so the smallest sub-tiles (3 px wide) still
    # read as four distinct shapes.
    $margin = [int][Math]::Max(1, [Math]::Round($Size * 0.0625))
    $gap    = [int][Math]::Max(1, [Math]::Round($Size * 0.04))
    $radius = [int][Math]::Max(0, [Math]::Round($Size * 0.08))
    $innerW = $Size - 2 * $margin
    $innerH = $Size - 2 * $margin

    $bmp = New-Object System.Drawing.Bitmap(
        $Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $g.SmoothingMode    = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $g.PixelOffsetMode  = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
        $g.Clear([System.Drawing.Color]::Transparent)

        $brush = New-Object System.Drawing.SolidBrush($BrandColor)
        try {
            foreach ($t in $Tiles) {
                # Each tile starts at its normalised origin and shrinks by
                # half-a-gap on each side abutting another tile. We just
                # subtract a full gap from width/height and translate by
                # half a gap — equivalent and simpler.
                $half = [int]($gap / 2)
                $x = $margin + [int][Math]::Round($t.x * $innerW) + $half
                $y = $margin + [int][Math]::Round($t.y * $innerH) + $half
                $w = [int][Math]::Round($t.w * $innerW) - $gap
                $h = [int][Math]::Round($t.h * $innerH) - $gap
                if ($w -lt 1) { $w = 1 }
                if ($h -lt 1) { $h = 1 }
                Add-RoundedTile -Graphics $g -Brush $brush `
                    -X $x -Y $y -W $w -H $h -R $radius
            }
        }
        finally { $brush.Dispose() }
    }
    finally { $g.Dispose() }

    return $bmp
}

function Add-RoundedTile {
    param(
        [System.Drawing.Graphics] $Graphics,
        [System.Drawing.Brush]    $Brush,
        [int] $X, [int] $Y, [int] $W, [int] $H, [int] $R
    )
    if ($R -le 0 -or $W -lt 2 * $R -or $H -lt 2 * $R) {
        $Graphics.FillRectangle($Brush, $X, $Y, $W, $H)
        return
    }
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    try {
        $d = 2 * $R
        $path.AddArc($X,             $Y,             $d, $d, 180, 90)
        $path.AddArc($X + $W - $d,   $Y,             $d, $d, 270, 90)
        $path.AddArc($X + $W - $d,   $Y + $H - $d,   $d, $d,   0, 90)
        $path.AddArc($X,             $Y + $H - $d,   $d, $d,  90, 90)
        $path.CloseFigure()
        $Graphics.FillPath($Brush, $path)
    }
    finally { $path.Dispose() }
}

function Get-PngBytes {
    param([System.Drawing.Bitmap] $Bitmap)
    $ms = New-Object System.IO.MemoryStream
    try {
        $Bitmap.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
        return ,$ms.ToArray()
    }
    finally { $ms.Dispose() }
}

function Get-IcoBmpDibBytes {
    # ICO entries < 256 px conventionally store a BITMAPINFOHEADER + 32-bpp
    # XOR mask + 1-bpp AND mask, with biHeight set to 2*height (XOR + AND
    # masks stacked). The XOR rows are bottom-up; the AND mask is all
    # zeros because alpha lives in the XOR mask.
    param([System.Drawing.Bitmap] $Bitmap, [int] $Size)

    $rect    = New-Object System.Drawing.Rectangle 0, 0, $Size, $Size
    $bmpData = $Bitmap.LockBits(
        $rect,
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $stride = $bmpData.Stride
        $raw    = New-Object byte[] ($stride * $Size)
        [System.Runtime.InteropServices.Marshal]::Copy(
            $bmpData.Scan0, $raw, 0, $raw.Length)
    }
    finally { $Bitmap.UnlockBits($bmpData) }

    # Flip rows to bottom-up.
    $rowSize  = $Size * 4
    $xor      = New-Object byte[] ($rowSize * $Size)
    for ($row = 0; $row -lt $Size; $row++) {
        $src = ($Size - 1 - $row) * $stride
        $dst = $row * $rowSize
        [Array]::Copy($raw, $src, $xor, $dst, $rowSize)
    }

    # AND mask: 1 bit per pixel, rows padded to 4-byte boundary. All zeros
    # = "use the alpha channel from XOR".
    $andRow  = [int]([Math]::Floor(($Size + 31) / 32) * 4)
    $andMask = New-Object byte[] ($andRow * $Size)

    # BITMAPINFOHEADER (40 bytes).
    $hdr = New-Object byte[] 40
    [BitConverter]::GetBytes([Int32]40         ).CopyTo($hdr,  0)  # biSize
    [BitConverter]::GetBytes([Int32]$Size      ).CopyTo($hdr,  4)  # biWidth
    [BitConverter]::GetBytes([Int32](2 * $Size)).CopyTo($hdr,  8)  # biHeight (XOR+AND)
    [BitConverter]::GetBytes([Int16]1          ).CopyTo($hdr, 12)  # biPlanes
    [BitConverter]::GetBytes([Int16]32         ).CopyTo($hdr, 14)  # biBitCount
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 16)  # BI_RGB
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 20)  # biSizeImage (0 OK for BI_RGB)
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 24)  # X DPI
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 28)  # Y DPI
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 32)  # biClrUsed
    [BitConverter]::GetBytes([Int32]0          ).CopyTo($hdr, 36)  # biClrImportant

    $out = New-Object byte[] ($hdr.Length + $xor.Length + $andMask.Length)
    [Array]::Copy($hdr,     0, $out, 0,                                  $hdr.Length)
    [Array]::Copy($xor,     0, $out, $hdr.Length,                        $xor.Length)
    [Array]::Copy($andMask, 0, $out, $hdr.Length + $xor.Length,          $andMask.Length)
    return ,$out
}

# --- main --------------------------------------------------------------------

$here    = Split-Path -Parent $MyInvocation.MyCommand.Path
$icoPath = Join-Path $here 'icon.ico'
$pngPath = Join-Path $here 'icon.png'

# Render every size once, retain the bitmap so we can also save the PNG.
$bitmaps = @{}
foreach ($s in ($BmpSizes + $PngSizes)) {
    $bitmaps[$s] = New-DwmendBitmap -Size $s
}

# The README PNG is the largest size we have. `$PngSizes` is declared in
# ascending order so the last element is the biggest; using `Measure-Object`
# would return a [double] and miss the [int] hashtable key.
$readmeSize = $PngSizes[-1]
$bitmaps[$readmeSize].Save(
    $pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
Write-Host "Wrote $pngPath"

# Build the ICO entries.
$entries = @()
foreach ($s in $BmpSizes) {
    $entries += [pscustomobject]@{
        Size  = $s
        IsPng = $false
        Data  = (Get-IcoBmpDibBytes -Bitmap $bitmaps[$s] -Size $s)
    }
}
foreach ($s in $PngSizes) {
    $entries += [pscustomobject]@{
        Size  = $s
        IsPng = $true
        Data  = (Get-PngBytes -Bitmap $bitmaps[$s])
    }
}

# Write the ICO file: 6-byte ICONDIR + N * 16-byte ICONDIRENTRY + payloads.
$fs = New-Object System.IO.FileStream(
    $icoPath, [System.IO.FileMode]::Create,
    [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
$bw = New-Object System.IO.BinaryWriter($fs)
try {
    $bw.Write([UInt16]0)                # idReserved
    $bw.Write([UInt16]1)                # idType = 1 (icon)
    $bw.Write([UInt16]$entries.Count)   # idCount

    $offset = 6 + 16 * $entries.Count
    foreach ($e in $entries) {
        $w = if ($e.Size -ge 256) { [byte]0 } else { [byte]$e.Size }
        $h = if ($e.Size -ge 256) { [byte]0 } else { [byte]$e.Size }
        $bw.Write([byte]$w)             # bWidth   (0 = 256)
        $bw.Write([byte]$h)             # bHeight
        $bw.Write([byte]0)              # bColorCount
        $bw.Write([byte]0)              # bReserved
        $bw.Write([UInt16]1)            # wPlanes
        $bw.Write([UInt16]32)           # wBitCount
        $bw.Write([UInt32]$e.Data.Length)  # dwBytesInRes
        $bw.Write([UInt32]$offset)         # dwImageOffset
        $offset += $e.Data.Length
    }
    foreach ($e in $entries) { $bw.Write($e.Data) }
}
finally {
    $bw.Dispose()
    $fs.Dispose()
    foreach ($b in $bitmaps.Values) { $b.Dispose() }
}
Write-Host "Wrote $icoPath ($($entries.Count) sub-images)"
