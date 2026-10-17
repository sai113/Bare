# Draws the Bare icon (a white "b" on a blue-violet rounded square) at every size Windows
# uses and packs them into assets/bare.ico, plus assets/bare.png (256px) for docs.
# Usage: pwsh scripts/make-icon.ps1

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
$assets = Join-Path $PSScriptRoot "..\assets"
New-Item -ItemType Directory -Force $assets | Out-Null
$assets = (Resolve-Path $assets).Path

function Draw-Icon([int]$s) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = "AntiAlias"
    $g.PixelOffsetMode = "HighQuality"

    # Rounded-square background with a diagonal gradient.
    $r = [single]($s * 0.22); $d = 2 * $r; $e = [single]($s - 0.5)
    $bg = New-Object System.Drawing.Drawing2D.GraphicsPath
    $bg.AddArc(0, 0, $d, $d, 180, 90); $bg.AddArc($e - $d, 0, $d, $d, 270, 90)
    $bg.AddArc($e - $d, $e - $d, $d, $d, 0, 90); $bg.AddArc(0, $e - $d, $d, $d, 90, 90)
    $bg.CloseFigure()
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush (New-Object System.Drawing.PointF 0, 0), (New-Object System.Drawing.PointF $s, $s), ([System.Drawing.Color]::FromArgb(255, 59, 130, 246)), ([System.Drawing.Color]::FromArgb(255, 124, 58, 237))
    $g.FillPath($grad, $bg)

    # The "b": a stem plus a round bowl, drawn with one stroke width.
    $w = [single]($s * 0.13)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), $w
    $pen.StartCap = "Round"; $pen.EndCap = "Round"
    $stemX = [single]($s * 0.31)
    $g.DrawLine($pen, $stemX, [single]($s * 0.25), $stemX, [single]($s * 0.75))
    $br = [single]($s * 0.19)            # bowl radius (to the stroke centre)
    $cx = [single]($stemX + $br); $cy = [single]($s * 0.75 - $br)
    $g.DrawEllipse($pen, $cx - $br, $cy - $br, 2 * $br, 2 * $br)

    $g.Dispose()
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    if ($s -eq 256) { $bmp.Save("$assets\bare.png", [System.Drawing.Imaging.ImageFormat]::Png) }
    $bmp.Dispose()
    , $ms.ToArray()
}

# ICO container with PNG-compressed entries (supported since Windows Vista).
$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$images = foreach ($s in $sizes) { , (Draw-Icon $s) }
$out = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $out
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $dim = if ($sizes[$i] -ge 256) { 0 } else { $sizes[$i] }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]$images[$i].Length); $bw.Write([uint32]$offset)
    $offset += $images[$i].Length
}
foreach ($img in $images) { $bw.Write($img) }
$bw.Flush()
[IO.File]::WriteAllBytes("$assets\bare.ico", $out.ToArray())
"Wrote assets/bare.ico ($($sizes -join ', ') px) and assets/bare.png"
