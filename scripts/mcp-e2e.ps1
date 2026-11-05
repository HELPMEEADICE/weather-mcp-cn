# MCP 端到端验证脚本：真正启动编译好的 weather-mcp-cn，走 stdio JSON-RPC 协议。
# 用法：pwsh -File scripts\mcp-e2e.ps1 -Binary target\release\weather-mcp-cn.exe

param(
    [string]$Binary = "target\release\weather-mcp-cn.exe"
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not (Test-Path $Binary)) { throw "找不到可执行文件：$Binary" }
$Binary = (Resolve-Path $Binary).Path

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Binary
$psi.Arguments = "--transport stdio"
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
$psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
$psi.EnvironmentVariables["RUST_LOG"] = "warn"

$proc = [System.Diagnostics.Process]::Start($psi)

function Send-Message([hashtable]$message) {
    $json = $message | ConvertTo-Json -Depth 20 -Compress
    $proc.StandardInput.WriteLine($json)
    $proc.StandardInput.Flush()
}

function Read-Message {
    $line = $proc.StandardOutput.ReadLine()
    if ($null -eq $line) { throw "服务端关闭了 stdout" }
    return ($line | ConvertFrom-Json)
}

$id = 0
function Invoke-Request([string]$method, $params) {
    $script:id++
    $current = $script:id
    Send-Message @{ jsonrpc = "2.0"; id = $current; method = $method; params = $params }
    while ($true) {
        $msg = Read-Message
        if ($msg.PSObject.Properties.Name -contains 'id' -and $msg.id -eq $current) { return $msg }
    }
}

try {
    # 1) initialize
    $init = Invoke-Request "initialize" @{
        protocolVersion = "2025-06-18"
        capabilities = @{}
        clientInfo = @{ name = "e2e-verify"; version = "0.0.0" }
    }
    Write-Host "== initialize =="
    Write-Host ("  serverInfo : " + $init.result.serverInfo.name + " " + $init.result.serverInfo.version)
    Write-Host ("  protocol   : " + $init.result.protocolVersion)
    Write-Host ("  tools cap  : " + ($null -ne $init.result.capabilities.tools))
    Send-Message @{ jsonrpc = "2.0"; method = "notifications/initialized" }

    # 2) tools/list
    $list = Invoke-Request "tools/list" @{}
    Write-Host "== tools/list =="
    foreach ($t in $list.result.tools) {
        $required = ($t.inputSchema.required -join ",")
        Write-Host ("  - {0} (required: {1})" -f $t.name, $required)
    }

    function Show-Tool([string]$name, $arguments, [string]$title) {
        Write-Host "== tools/call $name ($title) =="
        $resp = Invoke-Request "tools/call" @{ name = $name; arguments = $arguments }
        if ($null -ne $resp.error) {
            Write-Host ("  ERROR: " + ($resp.error | ConvertTo-Json -Depth 8 -Compress))
            return $null
        }
        $payload = $resp.result.structuredContent
        if ($null -eq $payload) { $payload = $resp.result.content[0].text | ConvertFrom-Json }
        return $payload
    }

    # 3) 实况
    $cur = Show-Tool "get_current_weather" @{ city = "北京" } "北京实况"
    if ($cur) {
        Write-Host ("  城市: " + $cur.city.display_name + " 代码 " + $cur.city.code)
        Write-Host ("  气温: " + $cur.temperature_c + "°C  天气: " + $cur.weather)
        Write-Host ("  湿度: " + $cur.humidity_percent + "%  风: " + $cur.wind_direction + $cur.wind_scale)
        Write-Host ("  观测时间: " + $cur.observed_at + "  (原始: " + $cur.observed_at_raw + ")")
        Write-Host ("  来源: " + $cur.source.url)
        Write-Host ("  缺失字段: " + ($cur.missing_fields -join ","))
    }

    # 4) 深圳南山（区县级）
    $ns = Show-Tool "get_current_weather" @{ city = "深圳市南山区" } "深圳市南山区实况"
    if ($ns) {
        Write-Host ("  解析到: " + $ns.city.display_name + " (" + $ns.city.code + ")")
        Write-Host ("  气温: " + $ns.temperature_c + "°C 天气: " + $ns.weather + " 观测: " + $ns.observed_at)
    }

    # 5) 上海 3 天预报
    $fc = Show-Tool "get_weather_forecast" @{ city = "上海"; days = 3 } "上海未来3天"
    if ($fc) {
        Write-Host ("  发布时间: " + $fc.published_at)
        foreach ($d in $fc.days) {
            Write-Host ("  {0} {1} 高{2} 低{3} 风{4} {5}" -f $d.date, $d.weather, $d.temperature_high_c, $d.temperature_low_c, $d.wind_direction_day, $d.wind_scale)
        }
    }

    # 6) 广州 7 天预报
    $fc7 = Show-Tool "get_weather_forecast" @{ city = "广州"; days = 7 } "广州未来7天"
    if ($fc7) { Write-Host ("  返回天数: " + $fc7.days.Count + " / 数据源可用天数 " + $fc7.available_days) }

    # 7) 逐 3 小时
    $hr = Show-Tool "get_hourly_weather" @{ city = "北京"; hours = 12 } "北京逐3小时"
    if ($hr) {
        Write-Host ("  粒度: " + $hr.resolution_hours + " 小时; 时点数: " + $hr.hours.Count)
        foreach ($h in $hr.hours) { Write-Host ("  " + $h.time + "  " + $h.weather + " " + $h.temperature_c + "°C " + $h.wind_direction + $h.wind_scale) }
    }

    # 8) 预警
    $al = Show-Tool "get_weather_alerts" @{ city = "长春" } "长春预警"
    if ($al) {
        Write-Host ("  has_alerts=" + $al.has_alerts + " status=" + $al.status + " count=" + $al.alerts.Count)
        foreach ($a in $al.alerts) {
            Write-Host ("  [" + $a.level + " " + $a.alert_type + "] " + $a.title + " 发布:" + $a.published_at + " 机构:" + $a.publisher)
        }
    }

    # 9) 城市搜索
    $sc = Show-Tool "search_cities" @{ query = "天河"; limit = 3 } "搜索天河"
    if ($sc) {
        Write-Host ("  命中 " + $sc.count + " 条；城市库 " + $sc.city_index.total + " 条")
        foreach ($c in $sc.cities) { Write-Host ("  " + $c.code + "  " + $c.display_name + "  [" + $c.level + "]") }
    }

    # 10) 错误处理：不存在的城市
    Write-Host "== 错误处理 =="
    $bad = Invoke-Request "tools/call" @{ name = "get_current_weather"; arguments = @{ city = "不存在的城市" } }
    Write-Host ("  " + ($bad.error | ConvertTo-Json -Depth 6 -Compress))

    # 11) 出错后服务器仍可用
    $again = Invoke-Request "tools/call" @{ name = "search_cities"; arguments = @{ query = "武汉" } }
    Write-Host ("  出错后仍可用: " + ($null -ne $again.result))

    Write-Host "== E2E OK =="
}
finally {
    try { $proc.StandardInput.Close() } catch {}
    if (-not $proc.HasExited) { $proc.Kill() }
    $stderr = $proc.StandardError.ReadToEnd()
    if ($stderr) { Write-Host "--- server stderr ---"; Write-Host $stderr }
}
