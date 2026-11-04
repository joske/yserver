@echo off
rem Start yserver on display :0, in the mode chosen in its Settings
rem (rootless by default), with the MIT cookie in the file
rem YSERVER_AUTH names, made on first use. Extra arguments go to yserver.
setlocal
if not defined YSERVER_AUTH set "YSERVER_AUTH=%LOCALAPPDATA%\yserver\Xauthority"
if exist "%YSERVER_AUTH%" goto have_cookie
rem One FamilyWild record (any host, display 0): yserver loads every cookie
rem in the file, and X clients can use the file as their XAUTHORITY.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ErrorActionPreference = 'Stop';" ^
  "$f = $env:YSERVER_AUTH; [void](New-Item -ItemType Directory -Force (Split-Path $f));" ^
  "$cookie = [byte[]]::new(16);" ^
  "[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($cookie);" ^
  "$out = [IO.MemoryStream]::new(); $out.WriteByte(255); $out.WriteByte(255);" ^
  "foreach ($field in @([byte[]]@(), [Text.Encoding]::ASCII.GetBytes('0')," ^
  "    [Text.Encoding]::ASCII.GetBytes('MIT-MAGIC-COOKIE-1'), $cookie)) {" ^
  "  $out.WriteByte($field.Length -shr 8); $out.WriteByte($field.Length -band 255);" ^
  "  $out.Write($field, 0, $field.Length) }" ^
  "[IO.File]::WriteAllBytes($f, $out.ToArray())"
if errorlevel 1 (
  echo Could not create the cookie file %YSERVER_AUTH%
  pause
  exit /b 1
)
:have_cookie
for /f %%c in ('powershell -NoProfile -Command "$b = [IO.File]::ReadAllBytes($env:YSERVER_AUTH); -join ($b[-16..-1] | ForEach-Object { $_.ToString('x2') })"') do set "COOKIE=%%c"
echo yserver on display :0
echo   cookie file  %YSERVER_AUTH%
echo   cookie       MIT-MAGIC-COOKIE-1 %COOKIE%
echo Settings and Exit are on its icon in the notification area.
"%~dp0yserver.exe" :0 -auth "%YSERVER_AUTH%" %*
if errorlevel 1 pause
