#   pwsh ci/abgen-sanity.ps1 [-Tree <repo root>] [-Profile <cargo profile dir>]

param(
    [string]$Tree = ".",
    [string]$Profile = "release"
)

$ErrorActionPreference = "Stop"

$host_exe = Join-Path $Tree "target\$Profile\abgen-host.exe"
$dll      = Join-Path $Tree "target\$Profile\abgen.dll"
$glb      = Join-Path $Tree "crate\abgen-wasm\test\fixtures\normal-quad.glb"

$script:failures = 0
function Check($cond, $what) {
    if ($cond) { Write-Output "  ok   $what" }
    else       { Write-Output "  FAIL $what"; $script:failures++ }
}

Check (Test-Path $host_exe) "abgen-host.exe present"
Check (Test-Path $dll)      "abgen.dll present"
Check (Test-Path $glb)      "fixture glb present"
if ($script:failures -gt 0) { Write-Output "FAILED (missing artifacts)"; exit 1 }

function New-Request([byte[]]$glbBytes, [string]$platform, [byte]$mode) {
    $ms = New-Object System.IO.MemoryStream
    $bw = New-Object System.IO.BinaryWriter($ms)
    function PutBytes($b) { $bw.Write([uint32]$b.Length); if ($b.Length) { $bw.Write($b) } }

    $bw.Write([uint32]1)                                             # file_count
    PutBytes ([System.Text.Encoding]::UTF8.GetBytes("model.glb"))
    PutBytes $glbBytes
    PutBytes ([System.Text.Encoding]::UTF8.GetBytes($platform))
    PutBytes ([byte[]]@())                                           # entity_type: detect
    $bw.Write([byte]0)                                               # magenta
    $bw.Write([byte]0)                                               # lod
    $bw.Write([byte]$mode)
    $bw.Write([byte]0)                                               # crop
    $bw.Write([uint32]0)                                             # tri_cap
    PutBytes ([byte[]]@())                                           # entity_hash
    PutBytes ([byte[]]@())                                           # only_glb
    $bw.Write([uint32]0)                                             # content table
    $bw.Flush()
    return $ms.ToArray()
}

function Invoke-Host([byte[]]$request, [string[]]$extraArgs) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $host_exe
    $psi.Arguments = ($extraArgs -join " ")
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true

    $p = [System.Diagnostics.Process]::Start($psi)

    # A full stderr pipe deadlocks a child still writing to stdout.
    $errTask = $p.StandardError.ReadToEndAsync()

    $stdin = $p.StandardInput.BaseStream
    $stdin.Write([BitConverter]::GetBytes([uint32]$request.Length), 0, 4)
    $stdin.Write($request, 0, $request.Length)
    $stdin.Flush()
    $stdin.Close()

    $out = New-Object System.IO.MemoryStream
    $p.StandardOutput.BaseStream.CopyTo($out)
    $p.WaitForExit()

    return [pscustomobject]@{
        Exit   = $p.ExitCode
        Frames = $out.ToArray()
        Err    = $errTask.Result
    }
}

function Read-Frames([byte[]]$buf) {
    $r = [pscustomobject]@{ Events = 0; Outputs = @(); Errors = @(); Manifest = $null; Code = $null }
    $off = 0
    while ($off + 8 -le $buf.Length) {
        $kind = [BitConverter]::ToUInt32($buf, $off)
        if ($kind -eq [uint32]::MaxValue) {
            $r.Code = [BitConverter]::ToInt32($buf, $off + 4); break
        }
        $len = [BitConverter]::ToUInt32($buf, $off + 4)
        $off += 8
        if ($off + $len -gt $buf.Length) { break }
        $payload = New-Object byte[] $len
        [Array]::Copy($buf, $off, $payload, 0, $len)
        $off += $len
        switch ($kind) {
            0 { $r.Events++ }
            1 {
                $nameLen = [BitConverter]::ToUInt32($payload, 0)
                $name = [System.Text.Encoding]::UTF8.GetString($payload, 4, $nameLen)
                $dataLen = [BitConverter]::ToUInt32($payload, 4 + $nameLen)
                $r.Outputs += [pscustomobject]@{ Name = $name; Length = $dataLen }
            }
            2 { $r.Errors += [System.Text.Encoding]::UTF8.GetString($payload) }
            3 { $r.Manifest = [System.Text.Encoding]::UTF8.GetString($payload) }
        }
    }
    return $r
}

Write-Output "abgen Windows sanity"
Write-Output ("  version = " + (& $host_exe --version))

$glbBytes = [System.IO.File]::ReadAllBytes($glb)

$req = New-Request $glbBytes "windows" 0
$res = Invoke-Host $req @()
$f = Read-Frames $res.Frames

Check ($res.Exit -eq 0)      "helper exited 0"
Check ($f.Code -eq 0)        "trailer carries exit code 0"
Check ($f.Errors.Count -eq 0) ("no fatal errors (" + ($f.Errors -join "; ") + ")")
Check ($f.Outputs.Count -eq 1) ("one bundle (got " + $f.Outputs.Count + ")")
if ($f.Outputs.Count -ge 1) {
    Write-Output ("  bundle: " + $f.Outputs[0].Name + " (" + $f.Outputs[0].Length + " bytes)")
    Check ($f.Outputs[0].Length -gt 0) "bundle carries bytes"
}
Check ($f.Events -gt 0) ("progress events (" + $f.Events + ")")
Check ($f.Manifest -match '"exitCode":0') "manifest exitCode 0"
Check ($f.Manifest -match 'v-abgen-host') "manifest identifies the host"

$bad = New-Request ([byte[]]@(0xde, 0xad, 0xbe, 0xef)) "windows" 0
$badRes = Invoke-Host $bad @()
$bf = Read-Frames $badRes.Frames
Check ($badRes.Exit -eq 0)      "corrupt asset is a file error, not a crash"
Check ($bf.Outputs.Count -eq 0) "corrupt asset produced no bundle"

# A job object limits *committed* memory where RLIMIT_AS limits *reserved*
# address space: measured, this binds at 1-4 MB where Linux needs gigabytes.
$tiny = Invoke-Host $req @("--max-memory-mb", "2")
Check ($tiny.Exit -ne 0) "a 2 MB job-object cap stops the conversion"

$ample = Invoke-Host $req @("--max-memory-mb", "2048")
$af = Read-Frames $ample.Frames
Check ($ample.Exit -eq 0)       "a 2 GB cap still converts"
Check ($af.Outputs.Count -eq 1) "capped run still produced its bundle"

if ($script:failures -eq 0) { Write-Output "`nPASS"; exit 0 }
else { Write-Output ("`nFAILED (" + $script:failures + ")"); exit 1 }
