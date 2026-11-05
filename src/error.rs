//! 统一的错误类型。
//!
//! 所有对外暴露的错误都映射为 [`rmcp::ErrorData`]，因此单个工具调用失败永远不会让
//! 整个 MCP Server 崩溃；错误里同时带上机器可读的 `code`，方便客户端做分支处理。

use rmcp::ErrorData;
use serde_json::json;
use thiserror::Error;

use crate::models::CityInfo;

/// 天气服务器可能产生的全部错误。
#[derive(Debug, Error)]
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

    /// 上游整体不可用（连接失败、被拒绝、返回了无法识别的页面等）。
    #[error("中国天气网暂时不可用：{0}")]
    Upstream(String),

    /// 上游返回了非预期 HTTP 状态码。
    #[error("中国天气网返回 HTTP {status}（{url}）")]
    UpstreamStatus { status: u16, url: String },

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
    /// 机器可读的错误码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::CityNotFound(_) => "city_not_found",
            Self::InvalidCityCode(_) => "invalid_city_code",
            Self::AmbiguousCity { .. } => "ambiguous_city",
            Self::Unsupported(_) => "unsupported_data",
            Self::Upstream(_) => "upstream_unavailable",
            Self::UpstreamStatus { .. } => "upstream_status",
            Self::Timeout(_) => "timeout",
            Self::Parse(_) => "parse_error",
            Self::Config(_) => "config_error",
        }
    }

    /// 是否属于「调用方输入有问题」，这类错误用 JSON-RPC invalid_params 返回。
    pub fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::CityNotFound(_)
                | Self::InvalidCityCode(_)
                | Self::AmbiguousCity { .. }
                | Self::Unsupported(_)
        )
    }

    /// 是否值得重试（网络抖动、上游 5xx、限流）。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Upstream(_) => true,
            Self::UpstreamStatus { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// 转换为 MCP 协议错误。
    pub fn to_error_data(&self) -> ErrorData {
        let mut data = json!({ "code": self.code(), "provider": "weather.com.cn" });
        if let Self::AmbiguousCity { candidates, .. } = self
            && let Some(obj) = data.as_object_mut()
        {
            obj.insert(
                "candidates".to_string(),
                serde_json::to_value(candidates).unwrap_or(serde_json::Value::Null),
            );
        }
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
