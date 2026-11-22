$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

$isolatedTestRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("gpu-swarm-test-" + [guid]::NewGuid().ToString("N"))
$isolatedAppData = Join-Path $isolatedTestRoot "AppData"
$isolatedConfigDir = Join-Path $isolatedTestRoot "config"
$originalAppData = $env:APPDATA
$originalConfigDir = $env:GPUSWARM_CONFIG_DIR
New-Item -ItemType Directory -Path $isolatedAppData -Force | Out-Null
New-Item -ItemType Directory -Path $isolatedConfigDir -Force | Out-Null
$env:APPDATA = $isolatedAppData
$env:GPUSWARM_CONFIG_DIR = $isolatedConfigDir
Write-Output "Using isolated provider config: $isolatedConfigDir"

if (-not $env:GPUSWARM_FEE_ADDRESS) {
    $env:GPUSWARM_FEE_ADDRESS = "0x20759eE366409B7dFE73a5325bfe0Dd57B6d7850"
}
if (-not $env:OLLAMA_MODEL) {
    $env:OLLAMA_MODEL = "llama3.2:1b"
}

$coordinatorLog = Join-Path $PSScriptRoot "coordinator-test.log"
$coordinatorErrorLog = Join-Path $PSScriptRoot "coordinator-test.err.log"
$providerLog = Join-Path $PSScriptRoot "provider-test.log"
$providerErrorLog = Join-Path $PSScriptRoot "provider-test.err.log"
Set-Content -Path $coordinatorLog -Value ""
Set-Content -Path $coordinatorErrorLog -Value ""
Set-Content -Path $providerLog -Value ""
Set-Content -Path $providerErrorLog -Value ""

$coordinator = Start-Process -FilePath "cargo" `
    -ArgumentList "run", "-p", "coordinator" `
    -WorkingDirectory $PSScriptRoot `
    -RedirectStandardOutput $coordinatorLog `
    -RedirectStandardError $coordinatorErrorLog `
    -PassThru

try {
    $httpReady = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        try {
            $fees = Invoke-RestMethod -Uri "http://127.0.0.1:8080/fees" -TimeoutSec 1
            $httpReady = $true
            break
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    if (-not $httpReady) {
        throw "Coordinator HTTP server did not become ready"
    }
    $fees | ConvertTo-Json -Compress

    $configDirectory = $env:GPUSWARM_CONFIG_DIR
    $configPath = Join-Path $configDirectory "config.json"
    New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
    if (Test-Path $configPath) {
        $config = Get-Content $configPath -Raw | ConvertFrom-Json
    } else {
        $config = [ordered]@{
            payout_address = $null
            provider_id = "test-provider"
            coordinator_ws = "ws://127.0.0.1:8081/ws"
            ollama_model = "llama3.2:1b"
            resource_settings = [ordered]@{
                mode = "only_when_idle"
                idle_threshold_minutes = 5
                schedule = $null
                preset = "balanced"
                do_not_run_on_battery = $true
                not_recommended_confirmed = $true
                performance_percent = 50
            }
            desktop_preferences = [ordered]@{}
        }
        $config = $config | ConvertTo-Json -Depth 8 | ConvertFrom-Json
    }
    $config.ollama_model = "llama3.2:1b"
    if ($null -eq $config.resource_settings) {
        $config | Add-Member -NotePropertyName resource_settings -NotePropertyValue ([ordered]@{})
    }
    if ($null -eq $config.resource_settings.not_recommended_confirmed) {
        $config.resource_settings | Add-Member -NotePropertyName not_recommended_confirmed -NotePropertyValue $true
    } else {
        $config.resource_settings.not_recommended_confirmed = $true
    }
    $configJson = $config | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText(
        $configPath,
        $configJson,
        [System.Text.UTF8Encoding]::new($false)
    )

    $provider = Start-Process -FilePath "cargo" `
        -ArgumentList "run", "-p", "provider-daemon", "--", "start" `
        -WorkingDirectory $PSScriptRoot `
        -RedirectStandardOutput $providerLog `
        -RedirectStandardError $providerErrorLog `
        -PassThru

    Write-Output "Waiting for provider registration..."
    $registered = $false
    for ($attempt = 0; $attempt -lt 600; $attempt++) {
        try {
            $providers = Invoke-RestMethod -Uri "http://127.0.0.1:8080/providers" -TimeoutSec 1
            if ($providers.Count -gt 0) {
                $providers | ConvertTo-Json -Compress
                $registered = $true
                break
            }
        } catch {}
        Start-Sleep -Milliseconds 100
    }
    if (-not $registered) {
        Write-Output "FAIL: no provider registered"
        Get-Content $providerLog -Tail 80
        exit 1
    }

    Write-Output "Testing missing X-Payment header:"
    try {
        Invoke-WebRequest -Method Post -Uri "http://127.0.0.1:8080/request-job" `
            -ContentType "application/json" `
            -Body '{"id":"job-missing-payment","prompt":"Respond with exactly: ok"}'
    } catch {
        $_.Exception.Response.StatusCode.value__ 
        $_.ErrorDetails.Message
    }

    $jobId = "job-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())"
    Write-Output "Testing accepted job: $jobId"
    $accepted = Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:8080/request-job" `
        -Headers @{ "X-Payment" = "mock-payment-ok" } `
        -ContentType "application/json" `
        -Body (@{ id = $jobId; prompt = "Respond with exactly: ok" } | ConvertTo-Json -Compress)
    $accepted | ConvertTo-Json -Compress

    Write-Output "Polling $jobId..."
    for ($attempt = 0; $attempt -lt 600; $attempt++) {
        try {
            $job = Invoke-RestMethod -Uri "http://127.0.0.1:8080/jobs/$jobId" -TimeoutSec 1
            if ($job.status -eq "completed") {
                $job | ConvertTo-Json -Compress -Depth 8
                exit 0
            }
            if ($job.status -eq "failed") {
                $job | ConvertTo-Json -Compress -Depth 8
                exit 1
            }
        } catch {}
        Start-Sleep -Milliseconds 100
    }
    Write-Output "FAIL: job did not finish"
    exit 1
} finally {
    if ($provider -and -not $provider.HasExited) {
        Stop-Process -Id $provider.Id -Force
    }
    if ($coordinator -and -not $coordinator.HasExited) {
        Stop-Process -Id $coordinator.Id -Force
    }
    $managedRoot = Join-Path $env:LOCALAPPDATA "GpuSwarm\runtime"
    Get-Process -Name ollama -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and $_.Path.StartsWith($managedRoot, [System.StringComparison]::OrdinalIgnoreCase) } |
        ForEach-Object { Stop-Process -Id $_.Id -Force }
    $env:APPDATA = $originalAppData
    if ($null -eq $originalConfigDir) {
        Remove-Item Env:GPUSWARM_CONFIG_DIR -ErrorAction SilentlyContinue
    } else {
        $env:GPUSWARM_CONFIG_DIR = $originalConfigDir
    }
    if (Test-Path $isolatedTestRoot) {
        Remove-Item -Recurse -Force $isolatedTestRoot -ErrorAction SilentlyContinue
    }
}
