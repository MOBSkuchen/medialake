# Generates ui/MediaLake.App/Assets/MediaLake.ico (rounded accent square + two stacked waves), multi-size PNG-in-ICO.
Add-Type -AssemblyName System.Drawing
$out = Join-Path (Split-Path $PSScriptRoot -Parent) "ui\MediaLake.App\Assets\MediaLake.ico"
$sizes = 16, 24, 32, 48, 64, 128, 256
$pngs = foreach ($s in $sizes) {
    $bmp = New-Object System.Drawing.Bitmap $s, $s
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    $r = [int]($s * 0.22)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = $r * 2
    $path.AddArc(0, 0, $d, $d, 180, 90); $path.AddArc($s - $d - 1, 0, $d, $d, 270, 90)
    $path.AddArc($s - $d - 1, $s - $d - 1, $d, $d, 0, 90); $path.AddArc(0, $s - $d - 1, $d, $d, 90, 90)
    $path.CloseFigure()
    $brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush ([System.Drawing.Point]::new(0,0)), ([System.Drawing.Point]::new($s,$s)), ([System.Drawing.Color]::FromArgb(255, 0, 120, 215)), ([System.Drawing.Color]::FromArgb(255, 88, 60, 220))
    $g.FillPath($brush, $path)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::White), ([Math]::Max(1.5, $s * 0.1))
    $pen.StartCap = 'Round'; $pen.EndCap = 'Round'
    # Two tildes stacked vertically, each one a single S-curve spanning the middle of the square.
    $x0 = $s * 0.2; $x1 = $s * 0.8; $w = $x1 - $x0; $amp = $s * 0.075
    foreach ($cy in @(($s * 0.36), ($s * 0.64))) {
        $g.DrawBezier($pen,
            [single]$x0, [single]($cy + $amp),
            [single]($x0 + $w / 3), [single]($cy - $amp * 1.33),
            [single]($x0 + 2 * $w / 3), [single]($cy + $amp * 1.33),
            [single]$x1, [single]($cy - $amp))
    }
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    ,@{ Size = $s; Bytes = $ms.ToArray() }
    $g.Dispose(); $bmp.Dispose()
}
$fs = [System.IO.File]::Create($out)
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$pngs.Count)
$offset = 6 + 16 * $pngs.Count
foreach ($p in $pngs) {
    $dim = if ($p.Size -ge 256) { 0 } else { $p.Size }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32); $bw.Write([uint32]$p.Bytes.Length); $bw.Write([uint32]$offset)
    $offset += $p.Bytes.Length
}
foreach ($p in $pngs) { $bw.Write($p.Bytes) }
$bw.Close(); $fs.Close()
Write-Host "wrote $out"
