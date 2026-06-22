# elevated_run.ps1 — runs a command elevated and captures output
$exe = "E:\Desktop\Scripts\Nova Cache\target\debug\nova-cache-service.exe"
$log = "E:\Desktop\Scripts\Nova Cache\test_data\service_console.log"
Start-Process -FilePath "cmd.exe" -ArgumentList "/c", "`"$exe`" --console > `"$log`" 2>&1" -Verb RunAs -Wait
