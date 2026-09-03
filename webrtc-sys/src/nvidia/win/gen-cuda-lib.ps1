$ErrorActionPreference = "Stop"
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -property installationPath
$vcvars = Join-Path $vs "VC\Auxiliary\Build\vcvars64.bat"
$dll = "$env:WINDIR\System32\nvcuda.dll"
if (-not (Test-Path $dll)) { throw "nvcuda.dll not found (no NVIDIA driver?)" }
cmd /c "`"$vcvars`" && dumpbin /exports `"$dll`" > nvcuda-exports.txt"
# build a .def from the exports
$lines = Get-Content nvcuda-exports.txt
$start = ($lines | Select-String -Pattern '^\s+ordinal\s+hint').LineNumber
$names = @()
for ($i = $start; $i -lt $lines.Count; $i++) {
  $l = $lines[$i]
  if ($l -match '^\s+\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\S+)') { $names += $Matches[1] }
  elseif ($l.Trim() -eq '' -and $names.Count -gt 0) { break }
}
"EXPORTS" | Out-File -Encoding ascii nvcuda.def
$names | Sort-Object -Unique | Out-File -Encoding ascii -Append nvcuda.def
cmd /c "`"$vcvars`" && lib /def:nvcuda.def /out:cuda.lib /machine:x64"
"generated cuda.lib with $($names.Count) exports"
