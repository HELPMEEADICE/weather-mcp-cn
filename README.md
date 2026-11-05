# weather-mcp-cn

[![CI](https://github.com/HELPMEEADICE/weather-mcp-cn/actions/workflows/ci.yml/badge.svg)](https://github.com/HELPMEEADICE/weather-mcp-cn/actions/workflows/ci.yml)
[![Build](https://github.com/HELPMEEADICE/weather-mcp-cn/actions/workflows/build.yml/badge.svg)](https://github.com/HELPMEEADICE/weather-mcp-cn/actions/workflows/build.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**纯 Rust 实现的 Model Context Protocol（MCP）中国天气查询服务器。**

数据全部来自[中国天气网](https://www.weather.com.cn/)的公开页面与公开数据接口——不需要 API Key、不需要注册账号、不依赖 Python / Node.js / 浏览器运行时。编译产物是**单个原生可执行文件**，可以被 Codex、Claude Desktop 或任何兼容 MCP 的客户端直接拉起。

```
用户: 深圳南山现在多少度？
  ↓ MCP tools/call get_current_weather { "city": "深圳市南山区" }
weather-mcp-cn → https://d1.weather.com.cn/sk_2d/101280604.html
  ↑ 25.4°C 多云 湿度 68% 东风2级  观测时间 2026-11-05T23:10:00+08:00
```

---

## 目录

- [核心功能](#核心功能)
- [技术架构](#技术架构)
- [数据来源与使用限制](#数据来源与使用限制)
- [编译方式](#编译方式)
- [CI 与发布](#ci-与发布)
- [安装与 Codex 集成](#安装与-codex-集成)
- [MCP 工具说明](#mcp-工具说明)
- [查询结果示例](#查询结果示例)
- [缓存说明](#缓存说明)
- [错误处理](#错误处理)
- [测试与验证](#测试与验证)
- [常见问题](#常见问题)
- [已知限制](#已知限制)

---

## 核心功能

| 能力 | 说明 |
| --- | --- |
| **中文城市名解析** | `北京` / `东莞` / `苏州市` / `深圳市南山区` / `湖北省武汉市` / `广东省广州市天河区` 都能解析成中国天气网 9 位城市代码；也接受直接输入代码 |
| **3221 条内置城市库** | 覆盖全国 34 个省级行政区、地级市与区县，编译期嵌入二进制，启动即可用，无需外部数据文件 |
| **5 个 MCP 工具** | `get_current_weather`、`get_weather_forecast`、`get_hourly_weather`、`search_cities`、`get_weather_alerts` |
| **真实联网取数** | 所有气象数值都来自实时抓取，项目里没有任何模拟数据、硬编码天气或示例 JSON |
| **缺失即留空** | 数据源没提供的字段（例如体感温度）用 `None` 表达并在 `missing_fields` 中列出，绝不填 0 或编造 |
| **时间语义严格** | 明确区分「观测时间」「预报发布时间」「本地抓取时间」，绝不用抓取时间冒充观测时间 |
| **健壮的错误处理** | 上游超时 / 403 / 429 / 5xx / 页面改版都会变成结构化 MCP 错误，服务器进程不会崩 |
| **限速与缓存** | 同主机最小请求间隔、并发上限、指数退避重试、分级 TTL 内存缓存 |
| **单文件部署** | `target/release/weather-mcp-cn`（Windows 为 `.exe`），无运行时依赖，无需数据库 |

---

## 技术架构

```
src/
├── main.rs          CLI 入口、日志（严格写 stderr）、stdio transport 启动
├── lib.rs           库入口（便于集成测试引用）
├── server.rs        MCP Server：tool_router + ServerHandler + 5 个工具
├── tools.rs         工具参数 / 输出结构（同时用于生成 JSON Schema）
├── models.rs        统一数据模型：WeatherObservation / DailyForecast / HourlyForecast / WeatherAlert / CityInfo
├── city.rs          中文地名 → 城市代码：层级剥离 + 模糊匹配 + 歧义处理
├── citygen.rs       城市库抓取生成器（--refresh-city-index）
├── cache.rs         手写 TTL 缓存（无第三方缓存依赖）
├── http.rs          统一 HTTP 层：连接复用、超时、限速、重试、UTF-8/GBK 解码
├── config.rs        命令行 + 环境变量配置
├── error.rs         错误类型与 MCP 错误映射
└── providers/
    ├── mod.rs       WeatherProvider trait（dyn 兼容，便于扩展其它数据源）
    └── weather_com_cn.rs   中国天气网实现 + 全部解析器
data/cities.json     编译期嵌入的城市库（3221 条）
tests/               解析器 / 城市解析 / MCP 协议 / 联网集成测试
scripts/mcp-e2e.ps1  真正启动二进制并走完 stdio 协议的端到端验证脚本
```

技术栈：Rust Stable（Edition 2024）· `rmcp` 3.8（官方 Rust MCP SDK）· `tokio` · `reqwest`（TLS 后端可选 rustls / native）· `scraper` · `serde` / `schemars` · `regex` · `chrono` · `tracing` · `thiserror` · `encoding_rs`。

刻意**没有**引入：Chromium / Playwright / Selenium、`clap`、`moka`、任何需要系统库的 TLS 实现。

### 数据流

```
MCP client ──stdio JSON-RPC──▶ WeatherServer (server.rs)
                                   │ 1. CityIndex::resolve("深圳市南山区") → 101280604
                                   │ 2. WeatherProvider::current_weather(city)
                                   ▼
                            WeatherComCn (providers/weather_com_cn.rs)
                                   │ TTL 缓存命中？
                                   │ 否 → HttpClient（限速 / 重试 / 解码）
                                   ▼
                            https://d1.weather.com.cn/sk_2d/101280604.html
                                   ▼
                            解析 var dataSK={...} → WeatherObservation
                                   ▼
                            Json<T> → content[].text + structuredContent
```

---

## 数据来源与使用限制

| 用途 | 地址 | 实测返回 |
| --- | --- | --- |
| 实况观测 | `https://d1.weather.com.cn/sk_2d/{code}.html` | `var dataSK={...}`：气温、天气现象、湿度、风向、风力、风速、气压、能见度、1h/24h 降水、AQI、观测时间 |
| 7 日预报 | `https://www.weather.com.cn/weather/{code}.shtml` | HTML，`ul.t > li` 七个条目：日期、天气、最高/最低温、风向、风力、发布时间 |
| 逐 3 小时预报 | `https://www.weather.com.cn/weather1d/{code}.shtml` | 页面内联 `var hour3data={...}`，粒度 **3 小时** |
| 气象预警 | `https://d1.weather.com.cn/dingzhi/{code}.html` | `var alarmDZ{code}={...}`：类型、级别、发布时间、失效时间、正文 |
| 城市编码库 | `https://j.i8tq.com/weather2020/search/city.js` | 站点城市选择器的官方数据：`{省:{市:{区:{AREAID,NAMECN}}}}` |

实测结论（2026-11，`curl` 直接请求核对）：

1. `d1.weather.com.cn` 会校验 `Referer`，缺失时返回 **403**，因此所有数据接口请求都带 `Referer: https://www.weather.com.cn/`。
2. `https://www.weather.com.cn/robots.txt` 内容为 `User-agent: *` / `Allow: /`，允许抓取。
3. 站点**没有**公开逐 1 小时预报接口：`hourly_2d`、`hour3data` 等路径都 302 到 404 页。公开数据只有 3 小时粒度。
4. 站点不提供体感温度、降水概率，本项目如实返回 `None` 而不是推算。
5. `station/{省市}.html` 的城市列表**不完整**（例如深圳只列了「深圳」一项），而城市选择器数据完整（含南山、福田、罗湖等），因此城市库以选择器数据为准。
6. 有些站点在夜间只给最低气温，此时 `temperature_high_c` 为空——这是数据源的实际情况，不是解析 bug。

**使用限制与自律**：

- 只读公开页面，不登录、不绕过任何访问控制、不使用验证码识别或代理池。
- 同一主机的请求间隔默认 **350 ms**（`WEATHER_MCP_MIN_INTERVAL_MS`），并发上限默认 **4**。
- 缓存默认值：实况 5 分钟、预报 30 分钟、逐 3 小时 30 分钟、预警 10 分钟，避免重复打上游。
- 数据版权归中国天气网及其数据提供方所有。本项目只做个人/内部查询用途的转发，**不得用于商业再分发**；请勿移除结果中的 `source` 字段。

---

## 编译方式

需要 Rust Stable ≥ 1.88（Edition 2024 + `rmcp` 要求）。

```bash
git clone https://github.com/HELPMEEADICE/weather-mcp-cn.git
cd weather-mcp-cn

cargo build --release
```

产物：

- Windows：`target\release\weather-mcp-cn.exe`
- Linux：`target/release/weather-mcp-cn`

### Release Profile

```toml
[profile.release]
opt-level = "z"        # 优先体积
lto = "fat"            # 跨 crate 全量 LTO
codegen-units = 1      # 更强的优化，构建慢一点
strip = "debuginfo"    # 去掉调试信息，保留符号
debug = false
incremental = false
overflow-checks = false
panic = "abort"        # 去掉 unwinding 表
```

Windows x86_64 实测（同样源码，两种 TLS 后端）：

| 后端 | 二进制 | 说明 |
| --- | --- | --- |
| `tls-rustls`（默认） | **8.80 MB** | 含 rustls + aws-lc-rs，无系统依赖，跨平台一致 |
| `tls-native` | **6.43 MB** | 用系统 TLS（SChannel / Security / OpenSSL） |
| `tls-native-vendored` | **6.74 MB** | Linux 上静态链接 OpenSSL |

> `strip = "debuginfo"` 保留符号表，比 `strip = true` 大约多 2.4 MB。若要把体积压到极限，把 `strip` 改成 `"symbols"` 或 `true` 即可，功能完全不受影响。
>
> 之所以用 `opt-level = "z"` 而不是 `3`：本工具的耗时几乎全在网络 I/O，解析成本远大于计算，体积收益更实在。改成 `opt-level = 3` 大约增大 10–15%。

### TLS 后端选择

TLS 后端是 Cargo feature，不是硬编码：

| Feature | reqwest 后端 | 平台实现 |
| --- | --- | --- |
| `tls-rustls`（**默认**） | `rustls` | 纯 Rust + aws-lc-rs，无系统依赖 |
| `tls-native` | `native-tls` | Windows SChannel / macOS Security.framework / Linux 系统 OpenSSL |
| `tls-native-vendored` | `native-tls` + vendored | 同上，但 Linux 上把 OpenSSL 静态编进二进制（发布用） |

```bash
cargo build --release                                          # rustls
cargo build --release --no-default-features --features tls-native
cargo build --release --no-default-features --features tls-native-vendored
```

必须提供这个开关的原因：rustls 的加密后端（aws-lc-rs / ring）**不支持 riscv64、mips 和 32 位 ARM**，这些架构只能用 OpenSSL。CI 里所有 Linux 目标统一走 `tls-native-vendored` —— 交叉编译时不需要目标机预装 libssl，产物也不依赖运行时 `libssl.so`（musl 目标即完全静态）。

### 交叉编译

```bash
# Linux x86_64
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

# Linux aarch64
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-gnu
```

项目只依赖 rustls（纯 Rust TLS），因此 musl 静态链接不需要额外系统库。

### 刷新城市库

城市库随二进制发布，正常使用**不需要**这一步。需要更新（例如行政区划调整）时：

```bash
weather-mcp-cn --refresh-city-index data/cities.json
cargo build --release    # 重新编译以内嵌新数据
```

也可以在运行期用外部文件覆盖，不必重新编译：

```bash
WEATHER_MCP_CITIES_FILE=/etc/weather-mcp-cn/cities.json weather-mcp-cn --transport stdio
```

---

## CI 与发布

两个 workflow，都在 `.github/workflows/`：

### `ci.yml` —— 代码质量门禁

| Job | 内容 |
| --- | --- |
| `fmt` | `cargo fmt --all -- --check` |
| `clippy` | `cargo clippy --all-targets --locked -- -D warnings`，两套 TLS 后端各跑一遍 |
| `test` | `ubuntu` / `windows` / `macos` 三平台 × 两套 TLS 后端（`cargo test --locked` + `--no-default-features --features tls-native`） |
| `live` | 仅手动触发，真访问中国天气网跑 10 个联网用例 |

触发：`push` 到 `main`、`pull_request`、手动。

> `test` 之所以把 `tls-native` 也真跑一遍：Release 里 Linux 二进制用的就是这个后端，不能让它成为"没测过的路径"。

### `build.yml` —— 交叉编译与产物分发

触发方式：

- **Release 发布时自动**（`release: published`）→ 构建全部目标并 `gh release upload` 自动挂到该 Release（`--clobber` 可重复上传）
- **手动触发**（`workflow_dispatch`）→ 构建全部目标，产物作为 **Actions artifact** 供下载；若在输入框填了 `tag`，同时上传到该 Release

目标矩阵（9 个）：

| 目标 | 运行器 | 构建方式 | TLS |
| --- | --- | --- | --- |
| `x86_64-pc-windows-msvc` | `windows-latest` | 原生 | rustls |
| `aarch64-pc-windows-msvc` | `windows-latest` | MSVC ARM64 交叉（`ilammy/msvc-dev-cmd`） | native（SChannel） |
| `x86_64-apple-darwin` | `macos-latest` | 原生（Apple clang 交叉） | rustls |
| `aarch64-apple-darwin` | `macos-latest` | 原生 | rustls |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` | `cross` | vendored OpenSSL（产物完全静态） |
| `aarch64-unknown-linux-musl` | `ubuntu-latest` | `cross` | vendored OpenSSL |
| `armv7-unknown-linux-gnueabihf` | `ubuntu-latest` | `cross` | vendored OpenSSL |
| `riscv64gc-unknown-linux-gnu` | `ubuntu-latest` | `cross` | vendored OpenSSL |
| `mips64-unknown-linux-gnuabi64` | `ubuntu-latest` | `cross` + `-Z build-std`（nightly，Tier-3 无预编译 std） | vendored OpenSSL |

设计要点：

- `fail-fast: false`：一个目标挂了不影响其它目标出产物。
- 缓存用 `Swatinem/rust-cache`，key 带 target + feature，避免不同目标互相污染。
- 产物命名 `weather-mcp-cn-<版本>-<target>.zip|tar.gz`，内含可执行文件 + `README.md` + `LICENSE`。
- 手动触发时版本号取 `tag` 输入，留空则用 `0.0.0-dev.<short-sha>`。
- 下载方式：Actions → 选一次运行 → 底部 **Artifacts** → 选对应 `weather-mcp-cn-<target>`。

> 关于 `mips`：这里构建的是 **mips64**。32 位 `mips-unknown-linux-gnu` 在依赖层就无法编译 —— `rmcp` / `tokio` 需要 `std::sync::atomic::AtomicU64`，而 MIPS32 不提供 64 位原子操作（实测报 `E0432: unresolved import std::sync::atomic::AtomicU64`）。mips64 是 Rust Tier 3，rustup 不发布预编译 std，因此该 job 使用 nightly + `-Z build-std=std,panic_abort`。
>
> 实测（2026-11-05）：9 个目标全部构建成功，单目标耗时 1–3 分钟；Windows 产物解压后已验证 `--version` 与完整 MCP 协议流程可用。

## 安装与 Codex 集成

### 1. 放置可执行文件

**Windows**

```powershell
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\weather-mcp-cn" | Out-Null
Copy-Item target\release\weather-mcp-cn.exe "$env:LOCALAPPDATA\weather-mcp-cn\weather-mcp-cn.exe"
```

**Linux**

```bash
install -Dm755 target/release/weather-mcp-cn ~/.local/bin/weather-mcp-cn
# 或
sudo install -Dm755 target/release/weather-mcp-cn /usr/local/bin/weather-mcp-cn
```

### 2. 注册到 Codex

Codex 的 MCP 配置在 `~/.codex/config.toml`（也可放在受信任项目的 `.codex/config.toml`），**格式是 TOML，不是 JSON**。

**方式 A：CLI 一条命令（推荐）**

```powershell
# Windows
codex mcp add weather-cn -- "$env:LOCALAPPDATA\weather-mcp-cn\weather-mcp-cn.exe" --transport stdio
```

```bash
# Linux
codex mcp add weather-cn -- /usr/local/bin/weather-mcp-cn --transport stdio
```

**方式 B：手写配置**

`~/.codex/config.toml`：

```toml
[mcp_servers.weather-cn]
command = "C:\\Users\\<你>\\AppData\\Local\\weather-mcp-cn\\weather-mcp-cn.exe"
args = ["--transport", "stdio"]
startup_timeout_sec = 20
tool_timeout_sec = 30
```

```toml
# Linux / macOS
[mcp_servers.weather-cn]
command = "/usr/local/bin/weather-mcp-cn"
args = ["--transport", "stdio"]
startup_timeout_sec = 20
tool_timeout_sec = 30
```

验证：

```bash
codex mcp list          # 应看到 weather-cn
codex                   # 进入 TUI 后输入 /mcp，应看到 5 个 weather 工具
```

Codex 会把工具名命名空间化成 `mcp__weather-cn__get_current_weather` 之类。它也会读取服务器 `initialize` 返回的 `instructions` 作为全局使用指引（本项目已提供）。

### 3. 注册到 Claude Desktop（可选）

`claude_desktop_config.json`（Windows：`%APPDATA%\Claude\claude_desktop_config.json`）：

```json
{
  "mcpServers": {
    "weather-cn": {
      "command": "C:\\Users\\<你>\\AppData\\Local\\weather-mcp-cn\\weather-mcp-cn.exe",
      "args": ["--transport", "stdio"]
    }
  }
}
```

```json
{
  "mcpServers": {
    "weather-cn": {
      "command": "/usr/local/bin/weather-mcp-cn",
      "args": ["--transport", "stdio"]
    }
  }
}
```

### 4. 命令行参数与环境变量

```
weather-mcp-cn [选项]

  --transport <stdio|http>        传输方式，当前版本仅实现 stdio
  --listen <地址>                 HTTP 模式监听地址（预留）
  --user-agent <UA>               覆盖默认 User-Agent
  --refresh-city-index [路径]     重新抓取城市库后退出
  -h, --help                      帮助
  -V, --version                   版本
```

| 环境变量 | 默认 | 说明 |
| --- | --- | --- |
| `WEATHER_MCP_OBSERVATION_TTL_SECS` | `300` | 实况缓存 TTL |
| `WEATHER_MCP_FORECAST_TTL_SECS` | `1800` | 预报缓存 TTL |
| `WEATHER_MCP_HOURLY_TTL_SECS` | `1800` | 逐 3 小时缓存 TTL |
| `WEATHER_MCP_ALERT_TTL_SECS` | `600` | 预警缓存 TTL |
| `WEATHER_MCP_CONNECT_TIMEOUT_SECS` | `5` | 连接超时 |
| `WEATHER_MCP_REQUEST_TIMEOUT_SECS` | `15` | 整体请求超时 |
| `WEATHER_MCP_MIN_INTERVAL_MS` | `350` | 同主机最小请求间隔 |
| `WEATHER_MCP_MAX_CONCURRENCY` | `4` | 全局并发上限 |
| `WEATHER_MCP_MAX_RETRIES` | `2` | 重试次数（不含首次） |
| `WEATHER_MCP_CACHE_CAPACITY` | `512` | 单类缓存容量 |
| `WEATHER_MCP_CITIES_FILE` | 空 | 覆盖内置城市库文件 |
| `RUST_LOG` | `info` | 日志级别，全部输出到 stderr |

---

## MCP 工具说明

所有工具都返回 `structuredContent`（同时把同样的 JSON 作为 `content[0].text` 返回），并带 `outputSchema`。

### `get_current_weather`

查询实况观测。

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `city` | string | ✅ | 城市名称或 9 位代码 |

返回：`city`、`temperature_c`、`weather`、`humidity_percent`、`wind_direction`、`wind_scale`、`wind_speed_kmh`、`pressure_hpa`、`visibility_km`、`precipitation_mm`、`precipitation_24h_mm`、`aqi`、`observed_at`、`observed_at_raw`、`observed_at_note`、`missing_fields`、`source`、`is_observation`。

### `get_weather_forecast`

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `city` | string | ✅ | 城市名称或代码 |
| `days` | integer | ❌ | 1–7，默认 3 |

返回：`published_at`、`published_at_raw`、`available_days`、`days[]`（`date`、`weekday`、`label`、`weather`、`weather_day`、`weather_night`、`temperature_high_c`、`temperature_low_c`、`wind_direction_day`、`wind_direction_night`、`wind_scale`）、`weather_split_note`、`source`。

### `get_hourly_weather`

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `city` | string | ✅ | 城市名称或代码 |
| `hours` | integer | ❌ | 1–24，默认 12 |

返回：`resolution_hours`（恒为 **3**）、`requested_hours`、`covered_hours`、`hours[]`（`time`、`weather`、`temperature_c`、`wind_direction`、`wind_scale`、`icon_code`）、`note`、`source`。

> ⚠️ 中国天气网公开数据只有 3 小时粒度。本工具返回时间窗内每 3 小时一个时点，**不做任何插值**。若需要逐 1 小时数据，本数据源确实不提供。

### `search_cities`

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `query` | string | ✅ | 名称片段（`南山`、`天河`、`苏州`）或城市代码 |
| `limit` | integer | ❌ | 1–50，默认 10 |

返回：`query`、`count`、`cities[]`（`code`、`name`、`province`、`city`、`district`、`display_name`、`level`）、`city_index`（`total`、`generated_at`、`source`）。

### `get_weather_alerts`

| 参数 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `city` | string | ✅ | 城市名称或代码 |

返回：`has_alerts`、`alerts[]`（`title`、`alert_type`、`level`、`level_code`、`type_code`、`publisher`、`published_at`、`expires_at`、`content`、`detail_url`、`region`）、`status`、`source`。

> `has_alerts=false` + `status="no_active_alerts"` 表示**查询成功且确实没有预警**；数据源不可用会返回 MCP 错误。两者绝不混淆。

---

## 查询结果示例

### 实况（北京，2026-11-05 23:15 真实返回）

```json
{
  "city": {
    "code": "101010100",
    "name": "北京",
    "province": "北京市",
    "city": "北京市",
    "display_name": "北京市",
    "level": "municipality"
  },
  "temperature_c": 8.8,
  "weather": "晴",
  "humidity_percent": 26.0,
  "wind_direction": "东北风",
  "wind_scale": "2级",
  "wind_speed_kmh": 9.0,
  "precipitation_mm": 0.0,
  "precipitation_24h_mm": 0.0,
  "pressure_hpa": 1021.0,
  "visibility_km": 30.0,
  "aqi": 16,
  "observed_at": "2026-11-05T23:15:00+08:00",
  "observed_at_raw": "11月05日(星期四) 23:15",
  "observed_at_note": "数据源只给出「11月05日(星期四) 23:15」，按北京时间（UTC+8）解析，年份由抓取日期推断",
  "missing_fields": ["feels_like_c"],
  "is_observation": true,
  "source": {
    "provider": "weather.com.cn",
    "url": "https://d1.weather.com.cn/sk_2d/101010100.html",
    "description": "中国天气网实况观测接口（sk_2d）",
    "fetched_at": "2026-11-05T23:31:14.155325800+08:00",
    "note": "该接口不提供体感温度、降水概率等字段，缺失字段不会出现在结果中"
  }
}
```

### 预报（上海，3 天）

```json
{
  "city": { "code": "101020100", "display_name": "上海市", "level": "municipality" },
  "published_at": "2026-11-05T18:00:00+08:00",
  "available_days": 7,
  "days": [
    { "date": "2026-11-05", "label": "今天", "weather": "阴", "temperature_low_c": 14, "wind_scale": "<3级" },
    { "date": "2026-11-06", "label": "明天", "weather": "阴转多云", "weather_day": "阴", "weather_night": "多云", "temperature_high_c": 20, "temperature_low_c": 15 },
    { "date": "2026-11-07", "label": "后天", "weather": "阴转小雨", "temperature_high_c": 18, "temperature_low_c": 15 }
  ],
  "weather_split_note": "weather 为数据源原文；weather_day / weather_night 只按「A转B」句式拆分，未出现「转」时两项为空，此时表示全天天气一致",
  "source": { "url": "https://www.weather.com.cn/weather/101020100.shtml" }
}
```

### 逐 3 小时（北京，12 小时窗口）

```json
{
  "resolution_hours": 3,
  "requested_hours": 12,
  "covered_hours": 12,
  "hours": [
    { "time": "2026-11-05T23:00:00+08:00", "weather": "晴", "temperature_c": 5, "wind_direction": "东北风", "wind_scale": "<3级", "icon_code": "n00" },
    { "time": "2026-11-06T02:00:00+08:00", "weather": "晴", "temperature_c": 2, "wind_direction": "东南风", "wind_scale": "<3级" },
    { "time": "2026-11-06T05:00:00+08:00", "weather": "晴", "temperature_c": 1, "wind_direction": "西风", "wind_scale": "<3级" }
  ],
  "note": "中国天气网公开数据为 3 小时粒度，本项目未做任何插值或推算；时间均为北京时间（UTC+8）",
  "source": { "url": "https://www.weather.com.cn/weather1d/101010100.shtml" }
}
```

### 预警（长春，命中 2 条）

```json
{
  "has_alerts": true,
  "status": "active_alerts_found",
  "alerts": [
    {
      "title": "吉林省发布大风蓝色预警",
      "alert_type": "大风",
      "level": "蓝色",
      "publisher": "吉林省气象台",
      "published_at": "2026-11-05T15:01:00+08:00",
      "expires_at": "2026-11-06T14:55:00+08:00",
      "region": "吉林省",
      "content": "吉林省气象台2026年11月05日14时55分发布大风蓝色预警：……（预警信息来源：国家预警信息发布中心）",
      "detail_url": "https://www.weather.com.cn/alarm/10106-20261105150120-0501.html"
    }
  ]
}
```

### 城市解析

```
深圳市南山区  → 101280604  广东省深圳市南山区  [district]
湖北省武汉市  → 101200101  湖北省武汉市        [city]
广东省广州市天河区 → 101280109  广东省广州市天河区 [district]
南山          → 歧义：101280604（深圳） / 101051206（黑龙江鹤岗）
深圳市粤海街道 → 退回 101280601（深圳），resolution_note 说明该街道无独立站点
```

---

## 缓存说明

| 数据 | 默认 TTL | 理由 |
| --- | --- | --- |
| 实况观测 | 5 分钟 | 上游约 5–10 分钟更新一次 |
| 7 日预报 | 30 分钟 | 上游每日更新数次 |
| 逐 3 小时预报 | 30 分钟 | 同上 |
| 气象预警 | 10 分钟 | 预警时效性强 |
| 城市库 | 进程生命周期 | 编译期嵌入 / 启动时加载，刷新需重启或 `--refresh-city-index` |

实现要点：

- 手写 `TtlCache`（`Mutex<HashMap>` + `Instant` 过期时间 + 容量上限 LRU 淘汰），没有引入缓存库。
- **只缓存成功结果**，失败不缓存——避免一次网络抖动被固化成一整段时间的错误。
- 每条结果都带 `source.fetched_at`（抓取时间）与 `observed_at` / `published_at`（气象时间），调用方可自行判断新鲜度。

---

## 错误处理

所有错误都映射为 MCP `ErrorData`，进程不退出：

| `data.code` | JSON-RPC | 含义 | 处理建议 |
| --- | --- | --- | --- |
| `city_not_found` | -32602 | 城市名/代码无法定位 | 调用 `search_cities` 或改用 9 位代码 |
| `ambiguous_city` | -32602 | 名称命中多个行政区（`data.candidates` 给出候选） | 用更完整的名称，如 `深圳市南山区` |
| `invalid_city_code` | -32602 | 代码格式不合法 | 应为 101 开头的 9 位数字 |
| `unsupported_data` | -32602 | 数据源明确不提供该数据 | 换查询口径 |
| `upstream_status` | -32603 | 上游返回 403 / 404 / 429 / 5xx | 检查城市代码；稍后重试 |
| `upstream_unavailable` | -32603 | 连接失败 / 数据源结构异常 | 稍后重试 |
| `timeout` | -32603 | 请求超时 | 稍后重试或调大 `WEATHER_MCP_REQUEST_TIMEOUT_SECS` |
| `parse_error` | -32603 | 页面结构变化导致解析失败 | 提 issue，附 `source.url` |

重试策略：仅对**超时 / 429 / 5xx** 重试，指数退避 `300ms × 2^n`（含抖动，上限 4s），默认最多 2 次；403/404 不重试。

日志全部走 stderr（`RUST_LOG` 控制），**stdout 只输出 MCP 协议消息**。

---

## 测试与验证

### 离线测试（`cargo test`，不依赖网络）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

当前结果：

| 目标 | 数量 | 内容 |
| --- | --- | --- |
| `src/lib.rs` 单元测试 | 25 | 解析器、缓存、编码解码、城市后缀、时间解析 |
| `tests/parser_tests.rs` | 11 | 用**真实抓取样本**离线验证四个解析器 |
| `tests/city_tests.rs` | 12 | 用内置 3221 条城市库验证名称/代码/层级/歧义解析 |
| `tests/mcp_tests.rs` | 5 | **真正启动二进制**，走 stdio 协议 `initialize` → `tools/list` → `tools/call` |
| `tests/live_tests.rs` | 10 | 联网集成测试，默认 `#[ignore]` |

`cargo test` 汇总：**53 passed, 0 failed, 10 ignored**。

`tests/fixtures/` 里保存了 2026-11-05 抓取的真实响应样本（实况 / 7 日预报 / 逐 3 小时 / 预警），因此这些解析测试既离线又真实。

`tests/mcp_tests.rs` 不是直接调用 Rust 函数：它用 `CARGO_BIN_EXE_weather-mcp-cn` 启动真实进程，按行收发 JSON-RPC，并断言 stdout 的每一行都是合法 JSON-RPC 2.0 消息（协议流未被日志污染）。

### 联网集成测试

```bash
cargo test --test live_tests -- --ignored --test-threads=1 --nocapture
```

覆盖：北京实况（含观测时间新鲜度校验）、上海 3 天预报、广州 7 天预报、武汉/深圳南山城市解析、逐 3 小时粒度校验、预警三态（有/无/不可用）、不存在城市、无效城市代码、超时上报、城市库源可达。

### 端到端脚本

```powershell
pwsh -File scripts/mcp-e2e.ps1 -Binary target\release\weather-mcp-cn.exe
```

真实拉起 release 二进制，依次执行 `initialize`、`tools/list`、7 次 `tools/call`（实况 / 区县解析 / 3 天预报 / 7 天预报 / 逐 3 小时 / 预警 / 城市搜索）以及 2 个错误用例，并验证出错后服务器仍可用。

---

## 常见问题

**Q：为什么 `get_hourly_weather` 返回的是 3 小时间隔？**
中国天气网公开数据只有 3 小时粒度（`hour3data`）。逐 1 小时接口不存在，本项目坚持「不推算」，因此如实标注 `resolution_hours: 3`。

**Q：为什么某些字段不见了？**
结果里只出现有值的字段。缺失字段名会列在 `missing_fields` 中。中国天气网实况接口本来就不提供体感温度、降水概率。

**Q：预报里 `temperature_high_c` 为空正常吗？**
正常。当天已入夜时数据源只给最低气温，此时最高气温留空而不是重复最低值。

**Q：为什么 `广州` 有时只有最低温、没有最高温？**
同上，是数据源当天的实际内容。

**Q：城市库怎么更新？**
`weather-mcp-cn --refresh-city-index data/cities.json` 重新抓取，然后重新编译；或用 `WEATHER_MCP_CITIES_FILE` 指向新文件免编译覆盖。

**Q：能加别的天气数据源吗？**
实现 `providers::WeatherProvider`（4 个方法 + 2 个元信息）即可，`server.rs` 只依赖 `Arc<dyn WeatherProvider>`。

**Q：Windows 上被杀软报毒 / 启动慢？**
二进制未做签名。首次启动可能被杀软扫描；必要时加白名单。服务器启动不做任何网络请求（城市库已内嵌），`initialize` 应在毫秒级完成。

**Q：能在 CI 里跑吗？**
可以。`cargo test` 完全离线；只有 `--ignored` 的联网用例需要出网。

---

## 已知限制

1. **逐小时精度**：上游只有 3 小时粒度，`get_hourly_weather` 的输出就是 3 小时一个点。这不是实现缺陷，是数据源边界。
2. **体感温度 / 降水概率 / 分钟级降水**：上游公开接口没有，一律留空。
3. **区县覆盖**：内置 3221 条，覆盖到区县；但个别街道/乡镇级地名没有独立站点，此时如实退回到上级行政区并在 `resolution_note` 说明，不编造代码。
4. **区县显示名后缀**：中国天气网只给简称（如「和林」「托县」），`display_name` 会按常见后缀补全，个别会与实际行政区名略有出入；`province` / `city` / `district` / `code` 均为原始数据，未加工。
5. **Streamable HTTP 模式**：`--transport http` 已预留但尚未实现，当前只提供 stdio。
6. **站点改版的脆弱性**：预报/逐 3 小时依赖 HTML 结构，上游改版会导致 `parse_error`（明确报错，不会返回假数据）。数据接口（`dataSK` / `alarmDZ` / `hour3data`）是 JSON，稳定得多。
7. **时区**：所有时间统一按北京时间（UTC+8）输出，与服务器所在时区无关。

---

## License

MIT，见 [LICENSE](LICENSE)。

天气数据版权归[中国天气网](https://www.weather.com.cn/)及数据提供方所有，本项目仅做个人/内部查询用途的转发。
