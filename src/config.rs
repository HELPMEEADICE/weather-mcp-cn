//! 配置：命令行参数 + 环境变量。
//!
//! 刻意不引入 clap —— 参数不多，手写解析即可，能少一个依赖、少几十 KB 体积。

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Result, WeatherError};

/// 传输方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// 标准输入输出，MCP 客户端（Codex / Claude Desktop）默认方式。
    Stdio,
    /// Streamable HTTP（可选模式）。
    Http,
}

impl TransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
        }
    }
}

/// 运行期配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// 实况缓存 TTL。
    pub observation_ttl: Duration,
    /// 天气预报缓存 TTL。
    pub forecast_ttl: Duration,
    /// 逐 3 小时预报缓存 TTL。
    pub hourly_ttl: Duration,
    /// 气象预警缓存 TTL。
    pub alert_ttl: Duration,
    /// 连接超时。
    pub connect_timeout: Duration,
    /// 整体请求超时。
    pub request_timeout: Duration,
    /// 同一上游主机的两次请求之间的最小间隔。
    pub min_request_interval: Duration,
    /// 全局并发上限。
    pub max_concurrency: usize,
    /// 最大重试次数（不含首次尝试）。
    pub max_retries: u32,
    /// 单类缓存的容量上限。
    pub cache_capacity: usize,
    /// User-Agent。
    pub user_agent: String,
    /// 传输方式。
    pub transport: TransportKind,
    /// HTTP 模式监听地址。
    pub listen: SocketAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            observation_ttl: Duration::from_secs(300),
            forecast_ttl: Duration::from_secs(1800),
            hourly_ttl: Duration::from_secs(1800),
            alert_ttl: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(15),
            min_request_interval: Duration::from_millis(350),
            max_concurrency: 4,
            max_retries: 2,
            cache_capacity: 512,
            user_agent: concat!(
                "weather-mcp-cn/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/HELPMEEADICE/weather-mcp-cn)"
            )
            .to_string(),
            transport: TransportKind::Stdio,
            listen: SocketAddr::from(([127, 0, 0, 1], 8787)),
        }
    }
}

/// 解析完命令行后的动作。
#[derive(Debug)]
pub enum Command {
    /// 正常启动 MCP Server。
    Run(Box<Config>),
    /// 重新抓取城市库并写出 JSON，然后退出。
    RefreshCityIndex {
        config: Box<Config>,
        output: PathBuf,
    },
    /// 打印帮助。
    Help,
    /// 打印版本。
    Version,
}

impl Config {
    /// 从环境变量与命令行参数构建配置。
    pub fn from_env_and_args() -> Result<Command> {
        let mut config = Config::default();
        config.load_env()?;

        let args: Vec<String> = env::args().skip(1).collect();
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "-h" | "--help" => return Ok(Command::Help),
                "-V" | "--version" => return Ok(Command::Version),
                "--transport" => {
                    index += 1;
                    let value = args.get(index).ok_or_else(|| {
                        WeatherError::Config("--transport 需要取值 stdio|http".into())
                    })?;
                    config.transport = parse_transport(value)?;
                }
                "--listen" => {
                    index += 1;
                    let value = args.get(index).ok_or_else(|| {
                        WeatherError::Config("--listen 需要取值 例如 127.0.0.1:8787".into())
                    })?;
                    config.listen = value
                        .parse()
                        .map_err(|_| WeatherError::Config(format!("无法解析监听地址：{value}")))?;
                }
                "--refresh-city-index" => {
                    let next = args.get(index + 1);
                    let output = match next {
                        Some(value) if !value.starts_with('-') => PathBuf::from(value),
                        _ => PathBuf::from("data/cities.json"),
                    };
                    return Ok(Command::RefreshCityIndex {
                        config: Box::new(config),
                        output,
                    });
                }
                "--user-agent" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| WeatherError::Config("--user-agent 需要取值".into()))?;
                    config.user_agent = value.clone();
                }
                other => {
                    return Err(WeatherError::Config(format!(
                        "未知参数：{other}。使用 --help 查看支持的参数"
                    )));
                }
            }
            index += 1;
        }

        if config.transport == TransportKind::Http {
            return Err(WeatherError::Config(
                "本版本仅提供 stdio 传输（Streamable HTTP 尚未启用），请使用 --transport stdio"
                    .into(),
            ));
        }

        Ok(Command::Run(Box::new(config)))
    }

    fn load_env(&mut self) -> Result<()> {
        if let Some(value) = env_secs("WEATHER_MCP_OBSERVATION_TTL_SECS")? {
            self.observation_ttl = value;
        }
        if let Some(value) = env_secs("WEATHER_MCP_FORECAST_TTL_SECS")? {
            self.forecast_ttl = value;
        }
        if let Some(value) = env_secs("WEATHER_MCP_HOURLY_TTL_SECS")? {
            self.hourly_ttl = value;
        }
        if let Some(value) = env_secs("WEATHER_MCP_ALERT_TTL_SECS")? {
            self.alert_ttl = value;
        }
        if let Some(value) = env_secs("WEATHER_MCP_CONNECT_TIMEOUT_SECS")? {
            self.connect_timeout = value;
        }
        if let Some(value) = env_secs("WEATHER_MCP_REQUEST_TIMEOUT_SECS")? {
            self.request_timeout = value;
        }
        if let Some(value) = env_u64("WEATHER_MCP_MIN_INTERVAL_MS")? {
            self.min_request_interval = Duration::from_millis(value);
        }
        if let Some(value) = env_u64("WEATHER_MCP_MAX_CONCURRENCY")? {
            self.max_concurrency = value.max(1) as usize;
        }
        if let Some(value) = env_u64("WEATHER_MCP_MAX_RETRIES")? {
            self.max_retries = value.min(5) as u32;
        }
        if let Some(value) = env_u64("WEATHER_MCP_CACHE_CAPACITY")? {
            self.cache_capacity = value.max(8) as usize;
        }
        if let Ok(value) = env::var("WEATHER_MCP_USER_AGENT")
            && !value.trim().is_empty()
        {
            self.user_agent = value;
        }
        if let Ok(value) = env::var("WEATHER_MCP_TRANSPORT")
            && !value.trim().is_empty()
        {
            self.transport = parse_transport(&value)?;
        }
        if let Ok(value) = env::var("WEATHER_MCP_LISTEN")
            && !value.trim().is_empty()
        {
            self.listen = value.parse().map_err(|_| {
                WeatherError::Config(format!("无法解析 WEATHER_MCP_LISTEN：{value}"))
            })?;
        }
        Ok(())
    }
}

fn parse_transport(value: &str) -> Result<TransportKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" => Ok(TransportKind::Stdio),
        "http" => Ok(TransportKind::Http),
        other => Err(WeatherError::Config(format!(
            "不支持的传输方式：{other}（可选：stdio、http）"
        ))),
    }
}

fn env_secs(key: &str) -> Result<Option<Duration>> {
    Ok(env_u64(key)?.map(Duration::from_secs))
}

fn env_u64(key: &str) -> Result<Option<u64>> {
    match env::var(key) {
        Ok(value) if !value.trim().is_empty() => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| WeatherError::Config(format!("{key} 必须是正整数，当前为「{value}」"))),
        _ => Ok(None),
    }
}

/// 帮助文本。
pub const HELP: &str = r#"weather-mcp-cn —— 中国天气网 MCP 服务器（纯 Rust）

用法：
  weather-mcp-cn [选项]

选项：
  --transport <stdio|http>        传输方式，默认 stdio
  --listen <地址>                 HTTP 模式监听地址，例如 127.0.0.1:8787
  --user-agent <UA>               覆盖默认 User-Agent
  --refresh-city-index [路径]     重新抓取中国天气网城市库并写入 JSON 后退出
  -h, --help                      显示帮助
  -V, --version                   显示版本

环境变量：
  WEATHER_MCP_OBSERVATION_TTL_SECS   实况缓存 TTL，默认 300
  WEATHER_MCP_FORECAST_TTL_SECS      预报缓存 TTL，默认 1800
  WEATHER_MCP_HOURLY_TTL_SECS        逐 3 小时预报缓存 TTL，默认 1800
  WEATHER_MCP_ALERT_TTL_SECS         预警缓存 TTL，默认 600
  WEATHER_MCP_CONNECT_TIMEOUT_SECS   连接超时，默认 5
  WEATHER_MCP_REQUEST_TIMEOUT_SECS   请求超时，默认 15
  WEATHER_MCP_MIN_INTERVAL_MS        同一主机最小请求间隔，默认 350
  WEATHER_MCP_MAX_CONCURRENCY         并发上限，默认 4
  WEATHER_MCP_MAX_RETRIES            重试次数，默认 2
  WEATHER_MCP_CACHE_CAPACITY         单类缓存容量，默认 512
  WEATHER_MCP_USER_AGENT             User-Agent
  RUST_LOG                           日志级别（输出到 stderr），默认 info
"#;
