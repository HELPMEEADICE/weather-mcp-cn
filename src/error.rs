//! 统一的错误类型与 MCP 错误映射。
//!
//! MCP 把「工具调用失败」分成两类，本项目严格区分，不会混用：
//!
//! | 情况 | 返回方式 | 本项目里的例子 |
//! | --- | --- | --- |
//! | 协议层故障：请求无法路由或无法解析 | JSON-RPC `error`（[`rmcp::ErrorData`]） | 未知工具名、报文不是合法 JSON-RPC、工具参数反序列化失败 |
//! | 工具层故障：工具确实跑了，但业务上没成功 | `CallToolResult { isError: true }` | 城市不存在/有歧义、上游超时、HTTP 403/429/5xx、页面改版、请求的时间窗没有被覆盖 |
//!
//! 工具层错误一定带 `structuredContent.code`，另外还带 `kind`（给 Agent 分类用）、
//! `retryable`（是否值得重试）、`hint`（怎么恢复）以及必要的 `candidates`，因此
//! 客户端既能展示给用户，也能据此自动重试或改写查询。
//!
//! 两条硬约束：
//!
//! * **错误绝不伪装成成功数据** —— 宁可返回 `isError: true`，也不返回一份空或假的天气；
//! * **单次调用失败绝不让进程退出** —— 所有错误都在工具边界被吞掉并转成结构化结果。

use chrono::{DateTime, FixedOffset};
use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::models::CityInfo;

/// 天气服务器可能产生的全部错误。
#[derive(Debug, Clone, Error)]
pub enum WeatherError {
    /// 城市名称无法在中国天气网城市库中定位。
    #[error(
        "未找到城市「{0}」。可调用 search_cities 搜索，或直接使用 9 位城市代码（如 101010100 表示北京）"
    )]
    CityNotFound(String),

    /// 用户给出的城市代码格式不合法。
    #[error(
        "城市代码格式无效：「{0}」。中国天气网城市代码应为 101 开头的 9 位数字，例如 101010100（北京）"
    )]
    InvalidCityCode(String),

    /// 城市名称命中多个行政区划。
    #[error(
        "城市名称「{query}」存在歧义，命中 {count} 个行政区划，请使用更完整的名称或直接使用城市代码"
    )]
    AmbiguousCity {
        query: String,
        count: usize,
        candidates: Vec<CityInfo>,
    },

    /// 数据源明确不提供该类数据（例如上游没有逐小时预报）。
    #[error("数据源不提供该数据：{0}")]
    Unsupported(String),

    /// 请求的时间窗口没有被任何有效的未来预报时点覆盖。
    #[error("{message}")]
    ForecastWindowUnavailable {
        /// 完整的中文说明，直接展示给调用方。
        message: String,
        /// 调用方请求的小时数。
        requested_hours: u32,
        /// 上游数据里最早可用的预报时点。
        available_from: Option<DateTime<FixedOffset>>,
        /// 上游数据里最后一个时点所在 3 小时区间的结束时刻。
        available_until: Option<DateTime<FixedOffset>>,
    },

    /// 上游整体不可用（连接失败、被拒绝、返回了无法识别的页面等）。
    #[error("中国天气网暂时不可用：{0}")]
    Upstream(String),

    /// 上游返回了非预期 HTTP 状态码。
    #[error("中国天气网返回 HTTP {status}（{url}）")]
    UpstreamStatus { status: u16, url: String },

    /// 被上游 CDN / WAF 拦截。
    ///
    /// 与中国天气网实测确认的两个触发条件：`Referer` 缺失或不是
    /// `www.weather.com.cn`（`Forbid_code=020200`），以及 `User-Agent` 命中爬虫黑名单
    /// （`Forbid_code=020300`）。
    #[error("请求被中国天气网 CDN 拦截（HTTP {status}，Forbid_code={code}）：{url}。{hint}")]
    Blocked {
        status: u16,
        code: String,
        hint: String,
        url: String,
    },

    /// 请求超时。
    #[error("请求超时：{0}")]
    Timeout(String),

    /// 上游页面/脚本结构变化导致解析失败。
    #[error("解析中国天气网响应失败：{0}")]
    Parse(String),

    /// 配置不合法。
    #[error("配置错误：{0}")]
    Config(String),
}

impl WeatherError {
    /// 机器可读的错误码（稳定契约，客户端可以据此分支）。
    pub fn code(&self) -> &'static str {
        match self {
            Self::CityNotFound(_) => "city_not_found",
            Self::InvalidCityCode(_) => "invalid_city_code",
            Self::AmbiguousCity { .. } => "ambiguous_city",
            Self::Unsupported(_) => "unsupported_data",
            Self::ForecastWindowUnavailable { .. } => "forecast_window_unavailable",
            Self::Upstream(_) => "upstream_unavailable",
            Self::UpstreamStatus { .. } => "upstream_status",
            Self::Blocked { .. } => "upstream_blocked",
            Self::Timeout(_) => "timeout",
            Self::Parse(_) => "parse_error",
            Self::Config(_) => "config_error",
        }
    }

    /// 错误大类，便于 Agent 一次性判断「该改查询还是该重试」。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CityNotFound(_) => "not_found",
            Self::InvalidCityCode(_) => "invalid_input",
            Self::AmbiguousCity { .. } => "ambiguous",
            Self::Unsupported(_) | Self::ForecastWindowUnavailable { .. } => "unavailable",
            Self::Upstream(_)
            | Self::UpstreamStatus { .. }
            | Self::Blocked { .. }
            | Self::Timeout(_) => "upstream",
            Self::Parse(_) => "upstream_format",
            Self::Config(_) => "internal",
        }
    }

    /// 恢复建议（写给 AI Agent 看的一句话）。
    pub fn hint(&self) -> &'static str {
        match self {
            Self::CityNotFound(_) => "先调用 search_cities 搜索地名，或改用 9 位城市代码重试",
            Self::InvalidCityCode(_) => "城市代码应为 101 开头的 9 位数字，例如 101010100",
            Self::AmbiguousCity { .. } => {
                "从 candidates 里选一个 code，或改用「省+市+区」的完整名称重试"
            }
            Self::Unsupported(_) => "换一种查询口径，例如改用 get_weather_forecast 查询逐日预报",
            Self::ForecastWindowUnavailable { .. } => {
                "缩小 hours 后重试，或改用 get_weather_forecast 查询逐日预报"
            }
            Self::UpstreamStatus { .. } => "稍后重试；若持续 404 请确认城市代码是否真的存在",
            Self::Blocked { .. } => "检查出口代理是否改写了请求头；本项目默认请求头实测可用",
            Self::Timeout(_) | Self::Upstream(_) => "稍后重试，上游抖动通常几秒内自行恢复",
            Self::Parse(_) => "上游结构可能已变化，属服务端问题，请附 source.url 反馈，不建议重试",
            Self::Config(_) => "检查环境变量与命令行参数",
        }
    }

    /// 是否属于「调用方输入有问题」。
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::CityNotFound(_)
                | Self::InvalidCityCode(_)
                | Self::AmbiguousCity { .. }
                | Self::Unsupported(_)
                | Self::ForecastWindowUnavailable { .. }
        )
    }

    /// 是否值得重试（网络抖动、上游 5xx、限流）。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Upstream(_) => true,
            // CDN 拦截既可能是请求头永久不合规，也可能是短时风控，
            // 重试一两次成本很低，能救回后者。
            Self::Blocked { .. } => true,
            Self::UpstreamStatus { status, .. } => *status == 429 || *status >= 500,
            // 页面改版、城市不存在、窗口无数据都不该重试。
            _ => false,
        }
    }

    /// 结构化错误负载：既进 `structuredContent`，也作为 `content[0].text`。
    pub fn payload(&self) -> Value {
        let mut data = Map::new();
        data.insert("code".into(), json!(self.code()));
        data.insert("kind".into(), json!(self.kind()));
        data.insert("message".into(), json!(self.to_string()));
        data.insert("provider".into(), json!("weather.com.cn"));
        data.insert("retryable".into(), json!(self.is_retryable()));
        data.insert("hint".into(), json!(self.hint()));

        match self {
            Self::AmbiguousCity {
                query, candidates, ..
            } => {
                data.insert("query".into(), json!(query));
                data.insert(
                    "candidates".into(),
                    serde_json::to_value(candidates).unwrap_or(Value::Null),
                );
            }
            Self::ForecastWindowUnavailable {
                requested_hours,
                available_from,
                available_until,
                ..
            } => {
                data.insert("requested_hours".into(), json!(requested_hours));
                data.insert("available_from".into(), json!(available_from));
                data.insert("available_until".into(), json!(available_until));
            }
            Self::UpstreamStatus { status, url } => {
                data.insert("status".into(), json!(status));
                data.insert("url".into(), json!(url));
            }
            Self::Blocked {
                status, code, url, ..
            } => {
                data.insert("status".into(), json!(status));
                data.insert("forbid_code".into(), json!(code));
                data.insert("url".into(), json!(url));
            }
            _ => {}
        }

        Value::Object(data)
    }

    /// 转换成 MCP **工具错误结果**（`isError: true`）。
    ///
    /// 这是工具执行失败的默认出口：调用方能看到完整信息，服务器继续服务。
    pub fn to_tool_result(&self) -> CallToolResult {
        CallToolResult::structured_error(self.payload())
    }

    /// 转换成 JSON-RPC **协议错误**。
    ///
    /// 只在「请求本身不成立、客户端根本没机会看到工具结果」时使用；
    /// 常规的工具执行失败请用 [`WeatherError::to_tool_result`]。
    pub fn to_error_data(&self) -> ErrorData {
        let data = self.payload();
        if self.is_client_error() {
            ErrorData::invalid_params(self.to_string(), Some(data))
        } else {
            ErrorData::internal_error(self.to_string(), Some(data))
        }
    }
}

impl From<reqwest::Error> for WeatherError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::Timeout(err.to_string())
        } else if err.is_decode() {
            Self::Parse(format!("响应体解码失败：{err}"))
        } else {
            Self::Upstream(err.to_string())
        }
    }
}

impl From<serde_json::Error> for WeatherError {
    fn from(err: serde_json::Error) -> Self {
        Self::Parse(format!("JSON 解析失败：{err}"))
    }
}

impl From<WeatherError> for ErrorData {
    fn from(value: WeatherError) -> Self {
        value.to_error_data()
    }
}

/// 便捷别名。
pub type Result<T> = std::result::Result<T, WeatherError>;
