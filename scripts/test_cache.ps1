# Nova Cache Integration Test Script
# Tests: write, read-back, IPC, flush, performance

$testDir = "E:\Desktop\Scripts\Nova Cache\test_data"
$testFile = "$testDir\test_write.bin"
$testFile2 = "$testDir\test_write2.bin"
$logFile = "$testDir\test_log.txt"

function Log($msg) {
    $ts = Get-Date -Format "HH:mm:ss.fff"
    $line = "[$ts] $msg"
    Write-Host $line
    Add-Content -Path $logFile -Value $line
}

function Send-IpcRequest($json) {
    try {
        $client = New-Object System.IO.Pipes.NamedPipeClientStream(".", "NovaCacheIpc", [System.IO.Pipes.PipeDirection]::InOut)
        $client.Connect(3000)
        $writer = New-Object System.IO.StreamWriter($client)
        $writer.AutoFlush = $true
        $reader = New-Object System.IO.StreamReader($client)
        $writer.WriteLine($json)
        $response = $reader.ReadLine()
        $client.Close()
        $client.Dispose()
        return $response
    } catch {
        return "ERROR: $_"
    }
}

# Cleanup
if (Test-Path $logFile) { Remove-Item $logFile }
New-Item -ItemType Directory -Path $testDir -Force | Out-Null

$pass = 0
$fail = 0

function Check($name, $condition) {
    if ($condition) {
        $script:pass++
        Log "PASS: $name"
    } else {
        $script:fail++
        Log "FAIL: $name"
    }
}

Log "=== Nova Cache Integration Tests ==="
Log ""

# --- Test 1: IPC Ping ---
Log "--- Test 1: IPC Ping ---"
$resp = Send-IpcRequest '{"type":"ping"}'
Log "Response: $resp"
Check "IPC Ping" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# --- Test 2: IPC GetStats ---
Log "--- Test 2: IPC GetStats ---"
$resp = Send-IpcRequest '{"type":"get_stats"}'
Log "Response: $resp"
Check "IPC GetStats" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# --- Test 3: IPC GetConfig ---
Log "--- Test 3: IPC GetConfig ---"
$resp = Send-IpcRequest '{"type":"get_config"}'
Log "Response: $resp"
Check "IPC GetConfig" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# --- Test 4: IPC GetFlushStatus ---
Log "--- Test 4: IPC GetFlushStatus ---"
$resp = Send-IpcRequest '{"type":"get_flush_status"}'
Log "Response: $resp"
Check "IPC GetFlushStatus" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# --- Test 5: Write 1MB test data ---
Log "--- Test 5: Write 1MB test file ---"
$size = 1024 * 1024
$data = New-Object byte[] $size
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($data)
[System.IO.File]::WriteAllBytes($testFile, $data)
Log "Wrote $size bytes to $testFile"
Check "Write 1MB" ($true)
Log ""

# --- Test 6: Read-back verification ---
Log "--- Test 6: Read-back verification ---"
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$readData = [System.IO.File]::ReadAllBytes($testFile)
$sw.Stop()
if ($readData.Length -eq $size) {
    $match = $true
    for ($i = 0; $i -lt $size; $i++) {
        if ($readData[$i] -ne $data[$i]) {
            $match = $false
            Log "MISMATCH at byte ${i}"
            break
        }
    }
    Check "Read-back integrity 1MB ($($sw.ElapsedMilliseconds)ms)" $match
} else {
    Check "Read-back size 1MB" $false
}
Log ""

# --- Test 7: Write 10MB test data ---
Log "--- Test 7: Write 10MB test file ---"
$size2 = 10 * 1024 * 1024
$data2 = New-Object byte[] $size2
$rng.GetBytes($data2)
[System.IO.File]::WriteAllBytes($testFile2, $data2)
Log "Wrote $size2 bytes to $testFile2"
Check "Write 10MB" ($true)
Log ""

# --- Test 8: Read-back 10MB ---
Log "--- Test 8: Read-back 10MB ---"
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$readData2 = [System.IO.File]::ReadAllBytes($testFile2)
$sw.Stop()
if ($readData2.Length -eq $size2) {
    $match = $true
    for ($i = 0; $i -lt $size2; $i++) {
        if ($readData2[$i] -ne $data2[$i]) {
            $match = $false
            Log "MISMATCH at byte ${i}"
            break
        }
    }
    Check "Read-back integrity 10MB ($($sw.ElapsedMilliseconds)ms)" $match
} else {
    Check "Read-back size 10MB" $false
}
Log ""

# --- Test 9: Repeat reads (should hit cache) ---
Log "--- Test 9: Repeat read (cache hit) ---"
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$null = [System.IO.File]::ReadAllBytes($testFile)
$sw.Stop()
Log "1MB repeat read took: $($sw.ElapsedMilliseconds)ms"
Check "Repeat read" ($sw.ElapsedMilliseconds -lt 5000)
Log ""

# --- Test 10: Flush status after writes ---
Log "--- Test 10: Post-write flush status ---"
Start-Sleep -Seconds 2
$resp = Send-IpcRequest '{"type":"get_flush_status"}'
Log "Response: $resp"
Check "Post-write flush" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# --- Test 11: Performance counters ---
Log "--- Test 11: Performance counters ---"
$resp = Send-IpcRequest '{"type":"get_stats"}'
Log "Response: $resp"
Check "Performance counters" ($resp -match '"status"\s*:\s*"ok"')
Log ""

# Cleanup
Log "--- Cleanup ---"
Remove-Item $testFile -ErrorAction SilentlyContinue
Remove-Item $testFile2 -ErrorAction SilentlyContinue
Log "Cleaned up test files"
Log ""
Log "=== Results: $pass passed, $fail failed ==="
