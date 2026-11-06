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
| **健壮的错误处理** | 工具执行失败返回 `isError=true` 的结构化工具错误（稳定错误码 + 是否可重试 + 恢复建议），只有协议层故障才走 JSON-RPC `error`；服务器进程不会崩 |
| **LRU + TTL 缓存** | 手写 O(1) LRU + TTL，容量上限、失败不缓存、同键并发未命中只打一次上游（singleflight），缓存里**不含**任何请求上下文 |
| **限速与重试** | 同主机最小请求间隔（按**真实发送时刻**锚定）、全局并发上限、指数退避重试（超时/连接失败/403 拦截/429/5xx）、成功响应 4 MiB 上限、错误响应 32 KiB 上限 |
| **单文件部署** | `target/release/weather-mcp-cn`（Windows 为 `.exe`），无运行时依赖，无需数据库 |

---

## 技术架构

```
src/
├── main.rs          CLI 入口、日志（严格写 stderr）、stdio transport 启动
├── lib.rs           库入口（便于集成测试引用）
├── server.rs        MCP Server：tool_router + ServerHandler + 5 个工具（返回 CallToolResult）
├── tools.rs         工具参数 / 输出结构（同时用于生成 JSON Schema）
├── models.rs        统一数据模型：WeatherObservation / DailyForecast / HourlyForecast / WeatherAlert / CityInfo
├── city.rs          中文地名 → 城市代码：层级剥离 + 模糊匹配 + 歧义处理
├── citygen.rs       城市库抓取生成器（--refresh-city-index）
├── cache.rs         手写 O(1) LRU + TTL 缓存与 singleflight（无第三方缓存依赖）
├── window.rs        逐 3 小时预报的时间窗口语义（纯函数，可注入固定时钟）
├── http.rs          统一 HTTP 层：连接复用、超时、限速、重试、响应体上限、UTF-8/GBK 解码
├── config.rs        命令行 + 环境变量配置
├── error.rs         错误类型与 MCP 错误映射（工具错误 vs 协议错误）
└── providers/
    ├── mod.rs       WeatherProvider trait（dyn 兼容）+ Fetched<T> + TextFetcher
    └── weather_com_cn.rs   中国天气网实现 + 全部解析器
data/cities.json     编译期嵌入的城市库（3221 条）
tests/               解析器 / 城市解析 / 缓存一致性 / MCP 协议 / 联网集成测试
scripts/mcp-e2e.ps1  真正启动二进制并走完 stdio 协议的端到端验证脚本
```

技术栈：Rust Stable（Edition 2024）· `rmcp` 3.8（官方 Rust MCP SDK）· `tokio` · `reqwest`（TLS 后端可选 rustls / native）· `scraper` · `serde` / `schemars` · `regex` · `chrono` · `tracing` · `thiserror` · `encoding_rs`。

刻意**没有**引入：Chromium / Playwright / Selenium、`clap`、`moka`、任何需要系统库的 TLS 实现。

### 数据流

```
MCP client ──stdio JSON-RPC──▶ WeatherServer (server.rs)
                                   │ 1. CityIndex::resolve("深圳市南山区") → 101280604
                                   │ 2. WeatherProvider::hourly_forecast(city)
                                   ▼
                            WeatherComCn (providers/weather_com_cn.rs)
                                   │ 缓存命中？（key = 城市代码，value = 气象数据 + 抓取时间）
                                   │ 否 → singleflight leader → HttpClient（限速 / 重试 / 体积上限 / 解码）
                                   ▼
                            https://www.weather.com.cn/weather1d/101280604.shtml
                                   ▼
                            解析 hour3data → HourlyPayload（不含城市、不含窗口）
                                   ▼
             绑定本次请求的 CityInfo → window::select(now, hours) → HourlyBundle
                                   ▼
              CallToolResult{structuredContent} 或 {isError:true, code, hint}
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

1. `d1.weather.com.cn` 的 CDN 会校验请求头，不合规时返回 **403** 并带 `X-CCDN-FORBID-CODE`：`020200` = `Referer` 缺失或不是 `https://www.weather.com.cn/` 域下的地址；`020300` = `User-Agent` 命中爬虫黑名单（`curl/`、`Wget/`、`python-requests`、`Go-http-client`、`Java/`、`Scrapy/`、`okhttp/`）或为空。因此本项目所有数据接口请求都带正确 Referer，默认 UA 也是实测可用的自定义值。
2. `https://www.weather.com.cn/robots.txt` 内容为 `User-agent: *` / `Allow: /`，允许抓取。
3. 站点**没有**公开逐 1 小时预报接口：`hourly_2d`、`hour3data` 等路径都 302 到 404 页。公开数据只有 3 小时粒度。
4. 站点不提供体感温度、降水概率，本项目如实返回 `None` 而不是推算。
5. `station/{省市}.html` 的城市列表**不完整**（例如深圳只列了「深圳」一项），而城市选择器数据完整（含南山、福田、罗湖等），因此城市库以选择器数据为准。
6. 有些站点在夜间只给最低气温，此时 `temperature_high_c` 为空——这是数据源的实际情况，不是解析 bug。

**使用限制与自律**：

- 只读公开页面，不登录、不绕过任何访问控制、不使用验证码识别或代理池。
- 同一主机的请求间隔默认 **350 ms**（`WEATHER_MCP_MIN_INTERVAL_MS`），并发上限默认 **4**；成功响应超过 **4 MiB**、错误响应超过 **32 KiB** 都直接截断失败，避免异常响应把内存吃满。限速保证的是**真实发送时刻**之间的间隔，不是「预约时刻」的间隔（见[限速与并发](#限速与并发信号量与主机限速器的顺序)）。
- 缓存默认值：实况 5 分钟、预报 30 分钟、逐 3 小时 30 分钟、预警 10 分钟；按「最近使用」淘汰，失败结果不缓存。
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

Windows x86_64 实测（Rust 1.98.1，`cargo build --release`，2026-11-06；同一台机器、同一 profile，用 `git stash` 在**同一工作区**里对本轮改动前后的源码各构建一次）：

| 后端 | 改动前 | 改动后 | 变化 | 说明 |
| --- | --- | --- | --- | --- |
| `tls-rustls`（默认） | 9,293,380 B（8.86 MiB） | 9,273,523 B（8.84 MiB） | **−19,857 B（−0.21%）** | 含 rustls + aws-lc-rs，无系统依赖，跨平台一致 |
| `tls-native` | 6,801,026 B（6.49 MiB） | 6,781,525 B（6.47 MiB） | **−19,501 B（−0.29%）** | 系统 TLS（SChannel / Security / OpenSSL） |
| `tls-native-vendored` | — | 未复测（本机等价于 `tls-native`） | — | 静态 OpenSSL 只在 Linux 有意义，由 CI 覆盖 |

> 体积**没有增长**：本轮删掉了非 2xx 路径上的 `response.text()`（连带 hyper 的整块拼接机制）与旧的「主机 → 预约时刻」同步表，新增的区间合并和每主机异步闸门规模都很小，整体净减约 0.2%。没有引入任何新依赖。

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
| `live` | 仅手动触发，真访问中国天气网跑 14 个联网用例 |

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

返回：`resolution_hours`（恒为 **3**）、`requested_hours`、`covered_hours`、`contiguous_hours`、`reference_time`、`coverage_end`、`hours[]`（`time`、`weather`、`temperature_c`、`wind_direction`、`wind_scale`、`icon_code`）、`note`、`source`。

> ⚠️ 中国天气网公开数据只有 3 小时粒度。本工具返回时间窗内每 3 小时一个时点，**不做任何插值**。若需要逐 1 小时数据，本数据源确实不提供。

**时间窗口语义**（`src/window.rs`，用注入的固定时钟做确定性测试）：

一个 `11:00` 的时点代表区间 `[11:00, 14:00)`。窗口以本次调用的「现在」（`reference_time`）为基准：

| 分类 | 条件 | 处理 |
| --- | --- | --- |
| 过去 | `t + 3h <= now` | 丢弃，绝不返回已经作废的预报 |
| 当前附近 | `t <= now < t + 3h` | 保留（正在生效的那份预报） |
| 未来 | `now < t <= now + hours` | 保留 |
| 超出窗口 | `t > now + hours` | 丢弃 |

请求窗口按半开区间 `[reference_time, reference_time + hours)` 处理。每个保留时点 `t` 映射成 `[t, t + 3h)`，窗口内的**真实覆盖**是两者的交集并集：

```text
segments         = merge( { [max(t, now), min(t + 3h, deadline)) | t ∈ 保留时点, min > max } )
covered_hours    = floor( Σ |segment| / 1h )        # 累计覆盖：允许中间有缺口
contiguous_hours = floor( 从 now 起连续的那一段 / 1h )  # 连续覆盖：遇到第一个缺口即止
coverage_end     = 最后一段 segment 的结束时刻（不会超过 deadline）
```

区间先按起点排序再合并重叠/首尾相接的区间，因此**重叠时点不会被重复计数，缺口不会被算成覆盖**。两个数字都被「请求窗口」和「上游实际数据」同时夹住，**永远不会被夸大**：

| 场景（`now = 10:00`，请求 12 小时） | `covered_hours` | `contiguous_hours` |
| --- | --- | --- |
| 上游给出 10:00、13:00、…、22:00 连续时点 | 12 | 12 |
| 上游只给了 19:00 一个时点 | 3 | 0 |
| 上游给了 10:00 与 17:00（14:00–17:00 无数据） | 6 | 3 |
| 上游最早的时点恰好是 22:00（= 截止时刻） | 工具错误（该时点不贡献覆盖） | — |

`covered_hours` 与 `contiguous_hours` 不相等，就是窗口里存在数据缺口的信号：后面的数据不能用来回答「接下来连续 N 小时怎么样」。旧实现只看「最后一个时点 + 3h」，上面第 2、3 行会分别被误报成 12 小时与 10 小时。

窗口内没有任何有效覆盖时，工具返回 `isError=true` 的错误（`code="forecast_window_unavailable"`，附 `available_from` / `available_until`），绝不返回空数组冒充「查询成功但没有数据」。

时间窗按绝对时刻比较，与服务器所在时区无关；跨日、跨月、跨年、闰日都有专门用例。

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

> `has_alerts=false` + `status="no_active_alerts"` 表示**查询成功且确实没有预警**（上游明确返回 `{"w":[]}`）；查询失败会返回 `isError=true` 的工具错误，两者绝不混淆。
>
> 预警解析是严格的：`w` 缺失、`w` 是 `null`/字符串/对象、JSON 截断、返回 HTML 错误页，一律报 `parse_error`，不会被当成「没有预警」。单条预警内部字段残缺时不会丢弃整条（缺 `w13` 标题会按「区域 + 类型 + 级别」合成标题），避免静默吞掉真实预警。

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
  "city": { "code": "101010100", "display_name": "北京市", "level": "municipality" },
  "resolution_hours": 3,
  "requested_hours": 12,
  "covered_hours": 12,
  "contiguous_hours": 12,
  "reference_time": "2026-11-05T23:30:00+08:00",
  "coverage_end": "2026-11-06T11:30:00+08:00",
  "hours": [
    { "time": "2026-11-05T23:00:00+08:00", "weather": "晴", "temperature_c": 5, "wind_direction": "东北风", "wind_scale": "<3级", "icon_code": "n00" },
    { "time": "2026-11-06T02:00:00+08:00", "weather": "晴", "temperature_c": 2, "wind_direction": "东南风", "wind_scale": "<3级" },
    { "time": "2026-11-06T05:00:00+08:00", "weather": "晴", "temperature_c": 1, "wind_direction": "西风", "wind_scale": "<3级" },
    { "time": "2026-11-06T08:00:00+08:00", "weather": "晴", "temperature_c": 2, "wind_direction": "西风", "wind_scale": "<3级" },
    { "time": "2026-11-06T11:00:00+08:00", "weather": "晴", "temperature_c": 10, "wind_direction": "西南风", "wind_scale": "<3级" }
  ],
  "note": "中国天气网公开数据为 3 小时粒度，本项目未做任何插值或推算；时间均为北京时间（UTC+8）",
  "source": { "url": "https://www.weather.com.cn/weather1d/101010100.shtml" }
}
```

### 预警（长春，命中 1 条）

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

### 策略：真正的 LRU + TTL

`TtlCache`（`src/cache.rs`）是手写实现，不引第三方缓存库（避免定时器与后台线程）：

- **数据结构**：槽位数组 + 双向链表 + `HashMap` 索引。命中时把节点移到表头，写满容量时从表尾淘汰 —— 这是**按最近使用顺序（LRU）淘汰**，不是「按过期时间远近挑一个删掉」。
- **复杂度**：链表操作全部 O(1)，不复制键。只有「写满容量后仍要插入」时才做一次 O(n) 的过期清理：先回收死条目，再退化为淘汰 LRU 尾节点，所以死数据不会挤掉活数据。
- **过期**：读取时按键判断并顺手删除；`len()` 与容量压力时批量清理。
- **容量**：`WEATHER_MCP_CACHE_CAPACITY`（默认 512，最小 8），四类缓存各自独立计数。
- **只缓存成功结果**：失败（超时 / 5xx / 解析失败）绝不入缓存，避免一次网络抖动被固化成一整段时间的错误。
- **不在 async 任务里长期持锁**：`std::sync::Mutex` 的临界区里没有任何 `.await`。
- **单一抓取时刻**：命中缓存时复用第一次抓取的 `fetched_at`，不会把「响应时间」冒充成「抓取时间」。
- 每条结果都带 `source.fetched_at`（抓取时间）与 `observed_at` / `published_at`（气象时间），调用方可自行判断新鲜度。

### 缓存里到底存什么（请求上下文隔离）

四个缓存的值都是 `Fetched<T>`：**只有可共享的气象数据 + 真实抓取时刻**，键是 9 位城市代码。`CityInfo`、`resolution_note`、`source.url` 这些与单次请求绑定的东西一律不进缓存，而是在每次调用返回时现场绑定：

| 缓存 | Key | Value | 绑定位置 |
| --- | --- | --- | --- |
| 实况 | 城市代码 | `ObservationPayload` | `into_observation(city, url, fetched_at)` |
| 逐日预报 | 城市代码 | `ForecastPayload` | `into_bundle(city, url, fetched_at)` |
| 逐 3 小时 | 城市代码 | `HourlyPayload` | `into_bundle(city, url, fetched_at)` |
| 气象预警 | 城市代码 | `AlertPayload` | `into_bundle(city, url, fetched_at)` |

这修掉了 v0.1.0 的一个真实缺陷：以前缓存的是「完整返回对象」，于是「先查深圳市粤海街道（回退到深圳并带 `resolution_note`）」之后，「再查深圳」会错误继承那条回退说明；反过来的顺序又会丢掉应有的说明。现在类型系统直接禁止这种污染（payload 里根本没有 `CityInfo` 字段），并且有交叉查询回归测试守着。

### singleflight（同键并发未命中只打一次上游）

`get_or_try_insert_with` 的语义：

1. 命中直接返回；
2. 未命中时第一个调用者成为 **leader**，其余调用者在 `tokio::sync::oneshot` 上等待；
3. **leader 拿到领导权后、真正加载前一定会再回查一次缓存**：从「检查缓存」到「注册 flight」之间有一个窗口，另一个 leader 可能刚好写完缓存并注销 flight，此时新 leader 若不回查就会把同一份数据再打一次上游（TOCTOU）。这个回查是充分的——leader 一定先 `insert` 再注销 flight，所以「拿到领导权」就意味着此前所有 leader 都已写完；
4. leader 成功后写缓存并唤醒等待者，等待者回查缓存直接命中；
5. leader 失败或被取消（future 被 drop）时，`FlightGuard` 在 `Drop` 里注销 inflight 表并唤醒等待者；等待者会自己接手重试（最多 3 轮），**不会死锁**；失败结果依然不入缓存。

协作轮次用尽后的兜底是「自己直接加载」，同样先回查缓存、成功后写缓存。它每个调用者最多走一次，因此 N 个并发调用者的总加载次数不超过 `N + 3`——与调用数同阶，不会指数放大（见 `repeated_failure_does_not_amplify_loads`）。

所以 4 个并发请求同一城市只会产生 1 次上游请求（见 `concurrent_misses_are_coalesced_into_one_request`），而「A 检查未命中 → 暂停 → B 完成加载 → A 恢复」这种交错也只会产生 1 次（见 `becoming_leader_rechecks_the_cache_before_loading`，用 `Barrier` 式的可控同步复现，不靠 `sleep` 碰运气）。

### 限速与并发（信号量与主机限速器的顺序）

旧实现先 `wait_for_slot()` 预约一个时间片、再去抢全局信号量。问题在于**预约时刻不等于真实发送时刻**：信号量被占满时，排队请求会在预约时刻上把等待耗完，等许可集中放行时一起把请求打出去，同主机的真实发送间隔被压缩到微秒级（实测 2.6 µs）。

现在的顺序（`src/http.rs`）：

1. 先进入**主机独占区**：同一主机同一时刻只有一个请求在这里排队等自己的时间片，**限速等待期间不占用任何并发许可**；
2. 再申请全局并发许可；
3. 拿到许可、真正发送之前调用 `HostLease::anchor()`，把「下一次最早发送时刻」锚定到 `Instant::now()`——记录的是真实发送时刻；
4. 锚定后立刻释放主机独占区，让同主机的下一个请求开始等它自己的时刻，本次请求继续带着并发许可读响应体。

不同主机的独占区互不相关，可并行推进；全局并发上限、请求超时、重试与指数退避完全不受影响。`tests/http_behavior_tests.rs` 用本地回放服务器记录**真实到达时刻**，断言「4 个请求长期占满许可 → 其余请求排队 → 许可集中释放」之后间隔依然 ≥ 最小间隔（容差 25 ms），并断言服务端观测到的并发不超过配置上限。

---

## 错误处理

MCP 把失败分成两层，本项目严格区分：

| 层 | 返回方式 | 例子 |
| --- | --- | --- |
| **协议层故障** | JSON-RPC `error`（`ErrorData`） | 未知工具名、报文不是合法 JSON-RPC、工具参数反序列化失败 |
| **工具执行失败** | `CallToolResult { isError: true }` + `structuredContent` | 城市不存在 / 有歧义、上游超时、HTTP 403/429/5xx、页面改版、时间窗未覆盖 |

工具错误的结构（以城市歧义为例）：

```json
{
  "code": "ambiguous_city",
  "kind": "ambiguous",
  "message": "城市名称「鼓楼」存在歧义，命中 12 个行政区划，请使用更完整的名称或直接使用城市代码",
  "provider": "weather.com.cn",
  "retryable": false,
  "hint": "从 candidates 里选一个 code，或改用「省+市+区」的完整名称重试",
  "query": "鼓楼",
  "candidates": [
    { "code": "101190111", "name": "鼓楼", "display_name": "江苏省南京市鼓楼区", "level": "district" }
  ]
}
```

同样的 JSON 也会写进 `content[0].text`，因此不读 `structuredContent` 的客户端一样能看到错误码与说明。

| `code` | `kind` | `retryable` | 含义 | 处理建议 |
| --- | --- | --- | --- | --- |
| `city_not_found` | not_found | ❌ | 城市名 / 代码无法定位 | 调 `search_cities` 或改用 9 位代码 |
| `ambiguous_city` | ambiguous | ❌ | 名称命中多个行政区（`candidates` 给候选） | 用更完整的名称，如 `深圳市南山区` |
| `invalid_city_code` | invalid_input | ❌ | 代码格式不合法 | 101 开头 9 位数字 |
| `unsupported_data` | unavailable | ❌ | 数据源明确不提供该数据 / 上游没返回预报 | 换查询口径 |
| `forecast_window_unavailable` | unavailable | ❌ | 请求时间窗没有任何有效未来时点（附 `available_from` / `available_until`） | 缩小 `hours`，或改用逐日预报 |
| `upstream_status` | upstream | 5xx / 429 时 ✅ | 上游返回非预期状态码（含 404）；若错误页读不完整，`message` 会说明「错误响应体未能完整读取」并附 `reason`（同一 `code`，客户端分支逻辑不变） | 检查城市代码；稍后重试 |
| `upstream_blocked` | upstream | ✅ | 被 CDN 拦截（`Forbid_code=020200` 表示 Referer 不合规，`020300` 表示 User-Agent 不合规） | 见[常见问题](#常见问题)里的 403 排查 |
| `upstream_unavailable` | upstream | ✅ | 连接失败 / 响应无法识别 | 稍后重试 |
| `timeout` | upstream | ✅ | 请求超时 | 稍后重试或调大 `WEATHER_MCP_REQUEST_TIMEOUT_SECS` |
| `parse_error` | upstream_format | ❌ | 页面 / 接口结构变化导致解析失败 | 提 issue，附 `source.url` |

**任何工具执行错误都不会让进程退出**：`server_keeps_serving_after_errors` 会连续触发 6 次失败调用（工具错误 + 未知工具），然后确认服务器仍能正常列工具、正常查询。

HTTP 层重试策略：只对**可恢复的上游故障**重试——超时、连接失败、CDN 拦截、429、5xx；指数退避 `300ms × 2^n`（含抖动，上限 4s），默认最多 2 次。403/404（非拦截）、解析失败、响应体超限都不重试。

**所有响应路径都有读取上限**（`src/http.rs` 的 `read_body_capped`）：

| 路径 | 上限 | 超限后的行为 |
| --- | --- | --- |
| 2xx 成功响应 | 4 MiB（`MAX_BODY_BYTES`） | `parse_error`，不重试（重试只会再读一遍） |
| 非 2xx 错误响应 | 32 KiB（`MAX_ERROR_BODY_BYTES`） | 保留已读前缀继续提取 `Forbid_code`；分类后附「未能完整读取」说明 |

错误页只需要区分「CDN 拦截」与「普通错误状态」，`Forbid_code` 实测出现在前几百字节，因此 32 KiB 足够；同时一个异常的 403/429/5xx 页面再也不会把内存拖垮。`Content-Length` 缺失、声明值偏小、分块传输、提前断开都由同一个分块读取循环兜住；`X-CCDN-FORBID-CODE` 响应头与 UTF-8 / GBK / GB18030 解码能力保持不变。

日志全部走 stderr（`RUST_LOG` 控制），**stdout 只输出 MCP 协议消息**。

---

## 测试与验证

### 离线测试（`cargo test`，不依赖网络）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo clippy --all-targets --locked --no-default-features --features tls-native -- -D warnings
cargo test --locked --no-default-features --features tls-native
cargo clippy --all-targets --locked --no-default-features --features tls-native-vendored -- -D warnings
```

当前结果（Windows x86_64，Rust 1.98.1，三套 TLS feature 结果一致）：

| 目标 | 数量 | 内容 |
| --- | --- | --- |
| `src/lib.rs` 单元测试 | 93 | LRU/TTL 淘汰（含链表不变量校验与中毒恢复）与 singleflight、窗口语义（跨日/月/年/闰日/时区/1-3-12-24 小时、缺口、重叠、截止时刻边界）、限速器主机隔离、预警异常结构、HTTP 体积上限、解析器、城市后缀 |
| `tests/parser_tests.rs` | 15 | **真实抓取样本**离线验证四个解析器，含空预警/`w:null`/损坏 JSON 与 67 KB 完整页面 |
| `tests/city_tests.rs` | 12 | 内置 3221 条城市库的名称/代码/层级/歧义解析 |
| `tests/provider_cache_tests.rs` | 6 | **真实缓存实现** + 回放样本：交叉查询上下文隔离、命中不打上游、并发 singleflight、失败不缓存 |
| `tests/window_coverage_tests.rs` | 10 | 逐 3 小时覆盖时长的真实交集：只有最后 3 小时有数据、缺口、重叠/重复、不规则时点、部分小时、跨年/闰日 |
| `tests/http_behavior_tests.rs` | 5 | 本地回放服务器：大体积 403/500（含分块、无 `Content-Length`）有界读取、错误页提前断开、CDN 拦截页分类、**许可集中释放时的真实发送间隔** |
| `tests/cache_singleflight_tests.rs` | 2 | 可控同步复现「检查缓存 ↔ 注册 flight」竞态；连续失败时不放大加载次数 |
| `tests/mcp_tests.rs` | 7 | **真正启动二进制**走 stdio：`initialize` → `tools/list` → `tools/call` → 工具错误语义 → 出错后继续服务 |
| `tests/live_tests.rs` | 14 | 联网集成测试（含 403/CDN 拦截回归），默认 `#[ignore]` |

`cargo test` 汇总：**150 passed, 0 failed, 14 ignored**（三套 TLS feature 组合同样全绿）。

关键回归用例：

| 用例 | 守护的行为 |
| --- | --- |
| `only_the_last_three_hours_does_not_claim_twelve` | 只有最后一个时点落在窗口内时，`covered_hours` 报 3 而不是 12（旧实现报 12） |
| `gap_between_buckets_is_not_counted_as_coverage` / `contiguous_hours_stop_at_the_first_gap` | 缺口不计入累计覆盖；连续覆盖在第一个缺口处停止 |
| `bucket_exactly_at_the_deadline_covers_nothing` | 时点恰好等于截止时刻时返回结构化工具错误 |
| `same_host_sends_keep_the_min_interval_when_permits_are_released_together` | 许可集中释放后同主机真实发送间隔仍 ≥ 配置值（旧实现实测 2.6 µs） |
| `oversized_403_body_is_read_with_a_cap` / `oversized_chunked_500_body_is_read_with_a_cap` | 32 MB 错误页不会整体读进内存 |
| `truncated_error_body_is_reported_not_swallowed` | 错误页读到一半断开时如实写进诊断 |
| `becoming_leader_rechecks_the_cache_before_loading` | 成为 leader 后回查缓存，不重复加载（旧实现加载 2 次） |
| `poisoned_inner_state_is_reset_instead_of_reused` | 持锁期间 panic 后丢弃可能残缺的链表，而不是继续使用 |
| `city_context_is_bound_per_request_in_every_cache` | 同一城市代码的两次查询各自绑定自己的 `resolution_note`（四类缓存全覆盖） |
| `fallback_note_appears_when_a_later_request_needs_it` | 反序查询时回退说明必须出现，且第二次命中缓存 |
| `concurrent_misses_are_coalesced_into_one_request` | 4 个并发同键请求只打 1 次上游 |
| `cancelled_leader_lets_a_waiter_take_over` | leader 被取消后等待者不死锁并接手 |
| `evicts_least_recently_used_key` / `lru_order_survives_many_evictions` / `expired_entries_are_reclaimed_before_live_ones` | LRU 淘汰顺序、连续淘汰后链表不变量自洽、「死条目不挤掉活条目」 |
| `hours_one_three_twelve_twenty_four` / `crosses_month_and_year_boundary` / `reports_miss_*` | 窗口裁剪与覆盖时长的精确语义 |
| `alert_null_w_is_an_error_not_empty` / `corrupt_alert_response_is_a_parse_error` | 「没有预警」与「查询失败」不混淆 |
| `invalid_city_returns_tool_error_with_stable_code` | 工具错误走 `isError: true` + 稳定错误码，而不是 JSON-RPC 错误 |
| `server_keeps_serving_after_errors` | 连续失败后服务器仍然可用 |

`tests/fixtures/` 里保存了从中国天气网抓取的真实响应样本（实况 / 7 日预报 / 逐 3 小时 / 完整逐 3 小时页面 / 预警 / 空预警），因此这些解析测试既离线又真实。

`tests/mcp_tests.rs` 不是直接调用 Rust 函数：它用 `CARGO_BIN_EXE_weather-mcp-cn` 启动真实进程，按行收发 JSON-RPC，并断言 stdout 的每一行都是合法 JSON-RPC 2.0 消息（协议流未被日志污染）。

### 与 TLS feature 相关的说明

- `tls-rustls`（默认）与 `tls-native` 在本机都完整跑过 fmt/clippy/test。
- `tls-native-vendored` 在本机通过 clippy；但 Windows 上 `native-tls` 走的是 SChannel，`vendored` 只影响 Linux/BSD 的 OpenSSL 构建，因此**静态 OpenSSL 这条路径只能由 CI 的 Linux 任务验证**（`.github/workflows/ci.yml` 与 `build.yml` 里就是这样做的），本机不做「vendored 一定静态链接成功」的宣称。
- 交叉编译目标（aarch64 / armv7 / riscv64 / mips64 / macOS）在本机无法运行，只能由 GitHub Actions 覆盖，不在此处宣称本地验证通过。

### 联网集成测试

```bash
cargo test --test live_tests -- --ignored --test-threads=1 --nocapture
```

覆盖：北京实况（含观测时间新鲜度校验）、上海 3 天预报、广州 7 天预报、武汉/深圳南山城市解析、逐 3 小时粒度校验、预警三态（有/无/不可用）、不存在城市、无效城市代码、超时上报、城市库源可达。请求间隔由 `HttpClient` 的限速器保证，只做串行小样本验证，不做压力测试。

### 端到端脚本

```powershell
pwsh -File scripts/mcp-e2e.ps1 -Binary target\release\weather-mcp-cn.exe
```

真实拉起 release 二进制，依次执行 `initialize`、`tools/list`、7 次 `tools/call`（实况 / 区县解析 / 3 天预报 / 7 天预报 / 逐 3 小时 / 预警 / 城市搜索）以及 2 个错误用例，并验证出错后服务器仍可用。

---

## 常见问题

**Q：报 `upstream_blocked` / HTTP 403 怎么办？**
中国天气网前面挂着 CDN，它对请求头有硬性要求。实测（2026-11-05 直接抓取核对）只有两个触发条件：

| `Forbid_code` | 触发条件 | 处理 |
| --- | --- | --- |
| `020200` | `Referer` 缺失，或来源不是 `https://www.weather.com.cn/` 域下的地址 | 本项目**所有**请求都带正确 Referer；出现这个码几乎只有一种可能：中间代理改写了请求头，请直连或换代理 |
| `020300` | `User-Agent` 命中爬虫黑名单（`curl/`、`Wget/`、`python-requests`、`Go-http-client`、`Java/`、`Scrapy/`、`okhttp/`）或为空 | 检查 `WEATHER_MCP_USER_AGENT`；默认值 `weather-mcp-cn/<版本> (+仓库地址)` 可用，浏览器 UA 也可用 |

不会触发 403 的情况也实测过：同一主机 4 路并发连续 100 次请求（无间隔）全部 200，不存在速率型 403；`www.weather.com.cn` 与 `j.i8tq.com` 不校验 Referer。

拦截是**可重试**的（可能是短时风控），客户端会按退避重试后仍失败才报错，错误体里带 `Forbid_code` 和对应排查建议。

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

**Q：为什么工具调用返回的是 `isError: true` 而不是 JSON-RPC 错误？**
这是 MCP 规范的推荐做法：请求本身合法、工具也确实执行了，只是业务上失败（城市不存在、上游超时、时间窗没数据），这类信息属于「工具结果」，客户端会把 `content` 展示给模型，模型据此改写查询或重试。只有请求无法路由/解析（未知工具名、报文不是合法 JSON-RPC、参数反序列化失败）才是 JSON-RPC `error`——那种情况下客户端往往只显示一句「内部错误」，信息全丢。`structuredContent.code` 是稳定错误码，`retryable` 与 `hint` 直接告诉 Agent 该不该重试。

**Q：为什么 `covered_hours` 小于我请求的 `hours`？**
三种可能：上游只提供了有限时点（例如 `hour1d` 只给 24 小时）；数据的最后一个时点所在区间在截止时刻前就结束了；**窗口中间有缺口**。`covered_hours` 统计的是「请求窗口与数据区间的真实交集总长度」，缺口不计入；`contiguous_hours` 统计从此刻起连续能答多久，两者不相等就说明中间有缺口（例如只有 19:00 一个时点时，`covered_hours=3` 而 `contiguous_hours=0`）。项目不会用 `max(3)` 之类的手段把数字凑大，也不对空档做插值。

**Q：为什么 `contiguous_hours` 比 `covered_hours` 小？**
说明窗口里有数据缺口。`covered_hours` 是累计值，`contiguous_hours` 在第一个缺口处就停止计数——如果你的 Agent 要回答「接下来连续 N 小时会不会下雨」，应该用 `contiguous_hours` 判断可信区间。

**Q：能在 CI 里跑吗？**
可以。`cargo test` 完全离线；只有 `--ignored` 的联网用例需要出网。

---

## 已知限制

1. **逐小时精度**：上游只有 3 小时粒度，`get_hourly_weather` 的输出就是 3 小时一个点。窗口只保留仍然覆盖未来的时点，`covered_hours` / `contiguous_hours` 只统计真正能回答的小时数——这不是实现缺陷，是数据源边界。
2. **体感温度 / 降水概率 / 分钟级降水**：上游公开接口没有，一律留空。
3. **区县覆盖**：内置 3221 条，覆盖到区县；但个别街道/乡镇级地名没有独立站点，此时如实退回到上级行政区并在 `resolution_note` 说明，不编造代码。
4. **区县显示名后缀**：中国天气网只给简称（如「和林」「托县」），`display_name` 会按常见后缀补全，个别会与实际行政区名略有出入；`province` / `city` / `district` / `code` 均为原始数据，未加工。
5. **Streamable HTTP 模式**：`--transport http` 已预留但尚未实现，当前只提供 stdio。
6. **站点改版的脆弱性**：预报/逐 3 小时依赖 HTML 结构，上游改版会导致 `parse_error`（明确报错，不会返回假数据）。数据接口（`dataSK` / `alarmDZ` / `hour3data`）是 JSON，稳定得多。
7. **请求头由 CDN 把关**：改了 `WEATHER_MCP_USER_AGENT` 或走了会改写请求头的代理，可能触发 403（`upstream_blocked`）。默认配置实测不受影响。
8. **时区**：所有时间统一按北京时间（UTC+8）输出，与服务器所在时区无关。
9. **城市模糊匹配**：仅用于把地名映射成城市代码，命中不确定时宁可报 `ambiguous_city` 也不猜；`resolve` 会对全部 3221 条条目做一次扫描（每进程为数不多的调用，尚未做索引化，属于已知的性能上限）。

---

## License

MIT，见 [LICENSE](LICENSE)。

天气数据版权归[中国天气网](https://www.weather.com.cn/)及数据提供方所有，本项目仅做个人/内部查询用途的转发。
