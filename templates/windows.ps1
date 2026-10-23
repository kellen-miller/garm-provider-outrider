# Copyright 2022 Cloudbase Solutions SRL
# Modified by Kellen Miller for Outrider.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$CallbackUrl = @@CALLBACK_URL@@
$MetadataUrl = @@METADATA_URL@@
$BearerToken = @@INSTANCE_TOKEN@@
$RunnerName = @@RUNNER_NAME@@
$DownloadUrl = @@DOWNLOAD_URL@@
$DownloadToken = @@DOWNLOAD_TOKEN@@
$Filename = @@FILENAME@@
$ExpectedSha256 = @@SHA256@@
$RepositoryUrl = @@REPOSITORY_URL@@
$RunnerLabels = @@RUNNER_LABELS@@
$JitEnabled = @@JIT_ENABLED@@
$RunnerDirectory = 'C:\actions-runner'

function Send-Status {
    param([string]$Status, [string]$Message, [Nullable[long]]$AgentId = $null)
    $url = $CallbackUrl.TrimEnd('/')
    if ($url -notmatch '/status/?$') { $url = $url.TrimEnd('/') + '/status' }
    $body = @{ status = $Status; message = $Message }
    if ($null -ne $AgentId) { $body.agent_id = $AgentId }
    try {
        Invoke-RestMethod -Method Post -Uri $url -Headers @{ Authorization = "Bearer $BearerToken"; Accept = 'application/json' } -ContentType 'application/json' -Body ($body | ConvertTo-Json -Compress) | Out-Null
    } catch {
        if ($Status -eq 'failed') { Write-Error 'GARM failure callback failed' }
    }
}

function Send-SystemInfo {
    param([Nullable[long]]$AgentId)
    $callback = $CallbackUrl.TrimEnd('/') -replace '/status$', ''
    $os = Get-CimInstance -ClassName Win32_OperatingSystem
    $body = @{ os_name = $os.Caption; os_version = $os.Version; agent_id = $AgentId } | ConvertTo-Json -Compress
    try {
        Invoke-RestMethod -Method Post -Uri "$callback/system-info/" -Headers @{ Authorization = "Bearer $BearerToken"; Accept = 'application/json' } -ContentType 'application/json' -Body $body | Out-Null
    } catch {
        Write-Output 'outrider bootstrap: system information callback failed'
    }
}

try {
    Add-Type -AssemblyName System.Security
    @@CA_BUNDLE@@
    @@DISABLE_UPDATES@@
    @@SSH_KEYS@@
    Send-Status -Status 'installing' -Message 'starting runner bootstrap'
    Write-Output 'outrider bootstrap: downloading runner'
    New-Item -ItemType Directory -Force -Path 'C:\ProgramData\outrider' | Out-Null
    $archive = Join-Path 'C:\ProgramData\outrider' $Filename
    $headers = @{}
    if ($DownloadToken.Length -gt 0) { $headers.Authorization = "Bearer $DownloadToken" }
    Invoke-WebRequest -UseBasicParsing -Uri $DownloadUrl -Headers $headers -OutFile $archive
    $actual = (Get-FileHash -Algorithm SHA256 -Path $archive).Hash.ToLowerInvariant()
    if ($actual -cne $ExpectedSha256) { throw 'runner archive checksum verification failed' }
    if (-not (Test-Path $RunnerDirectory)) { New-Item -ItemType Directory -Path $RunnerDirectory | Out-Null }
    Expand-Archive -Path $archive -DestinationPath $RunnerDirectory -Force
    Set-Location $RunnerDirectory

    if ($JitEnabled) {
        Write-Output 'outrider bootstrap: retrieving runner credentials'
        Invoke-WebRequest -UseBasicParsing -Uri "$($MetadataUrl.TrimEnd('/'))/credentials/runner" -Headers @{ Authorization = "Bearer $BearerToken" } -OutFile (Join-Path $RunnerDirectory '.runner')
        Invoke-WebRequest -UseBasicParsing -Uri "$($MetadataUrl.TrimEnd('/'))/credentials/credentials" -Headers @{ Authorization = "Bearer $BearerToken" } -OutFile (Join-Path $RunnerDirectory '.credentials')
        $rsa = (Invoke-WebRequest -UseBasicParsing -Uri "$($MetadataUrl.TrimEnd('/'))/credentials/credentials_rsaparams" -Headers @{ Authorization = "Bearer $BearerToken" }).Content
        $bytes = [Text.Encoding]::UTF8.GetBytes($rsa)
        $protected = [Security.Cryptography.ProtectedData]::Protect($bytes, $null, [Security.Cryptography.DataProtectionScope]::LocalMachine)
        [IO.File]::WriteAllBytes((Join-Path $RunnerDirectory '.credentials_rsaparams'), $protected)
        & icacls.exe $RunnerDirectory /inheritance:r /grant 'SYSTEM:(OI)(CI)F' /grant 'Administrators:(OI)(CI)F' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'runner credential ACL setup failed' }
        Invoke-WebRequest -UseBasicParsing -Uri "$($MetadataUrl.TrimEnd('/'))/system/service-name" -Headers @{ Authorization = "Bearer $BearerToken" } -OutFile (Join-Path $RunnerDirectory '.service')
        $serviceName = (Get-Content -Raw (Join-Path $RunnerDirectory '.service')).Trim()
        if ($serviceName -notmatch '^[A-Za-z0-9_.-]+$') { throw 'GARM returned an invalid service name' }
        New-Service -Name $serviceName -BinaryPathName "`"$RunnerDirectory\bin\RunnerService.exe`"" -DisplayName $serviceName -StartupType Automatic | Out-Null
        Start-Service -Name $serviceName
    } else {
        Write-Output 'outrider bootstrap: retrieving repository registration token'
        $registrationToken = (Invoke-WebRequest -UseBasicParsing -Uri "$($MetadataUrl.TrimEnd('/'))/runner-registration-token/" -Headers @{ Authorization = "Bearer $BearerToken" }).Content
        $configArguments = @('--unattended', '--url', $RepositoryUrl, '--token', $registrationToken, '--name', $RunnerName, '--labels', $RunnerLabels, '--no-default-labels', '--ephemeral', '--runasservice', '--windowslogonaccount', 'NT AUTHORITY\SYSTEM')
        Get-ChildItem -Path $RunnerDirectory -Recurse -File | Unblock-File
        & (Join-Path $RunnerDirectory 'bin\Runner.Listener.exe') configure @configArguments
        if ($LASTEXITCODE -ne 0) { throw 'runner repository configuration failed' }
        $agent = Get-Content -Raw (Join-Path $RunnerDirectory '.runner') | ConvertFrom-Json
        $agentId = $agent.agent_id
        if ($null -eq $agentId) { $agentId = $agent.agentId }
        $numericAgentId = [long]0
        if ($null -eq $agentId -or -not [long]::TryParse([string]$agentId, [ref]$numericAgentId) -or $numericAgentId -le 0) { throw 'runner .runner file has no positive agent ID' }
        $agentId = $numericAgentId
        Send-SystemInfo -AgentId $agentId
        Send-Status -Status 'idle' -Message 'runner successfully installed' -AgentId $agentId
    }
    if ($JitEnabled) {
        $agent = Get-Content -Raw (Join-Path $RunnerDirectory '.runner') | ConvertFrom-Json
        $agentId = $agent.agent_id
        if ($null -eq $agentId) { $agentId = $agent.agentId }
        $numericAgentId = [long]0
        if ($null -eq $agentId -or -not [long]::TryParse([string]$agentId, [ref]$numericAgentId) -or $numericAgentId -le 0) { throw 'runner .runner file has no positive agent ID' }
        $agentId = $numericAgentId
        Send-SystemInfo -AgentId $agentId
        Send-Status -Status 'idle' -Message 'runner successfully installed' -AgentId $agentId
    }
} catch {
    Send-Status -Status 'failed' -Message 'runner bootstrap failed'
    throw 'runner bootstrap failed'
}
