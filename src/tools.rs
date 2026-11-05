//! MCP 工具的参数与输出结构。
//!
//! 参数结构同时用于反序列化和生成 JSON Schema（`schemars`），因此字段文档会直接
//! 出现在 `tools/list` 返回的 `inputSchema` 中，方便模型理解。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::models::CityInfo;

/// `get_current_weather` 参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CurrentWeatherQuery {
    /// 城市名称或中国天气网城市代码。
    ///
    /// 支持：`北京`、`上海`、`深圳市南山区`、`湖北省武汉市`、`广东省广州市天河区`，
    /// 或 9 位代码 `101010100`。城市名称有歧义时会返回候选列表。
    pub city: String,
}

/// `get_weather_forecast` 参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ForecastQuery {
    /// 城市名称或中国天气网城市代码，例如 `上海` 或 `101020100`。
    pub city: String,
    /// 预报天数，1-7，默认 3。中国天气网公开页面最多提供 7 天。
    pub days: Option<u32>,
}

/// `get_hourly_weather` 参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HourlyQuery {
    /// 城市名称或中国天气网城市代码。
    pub city: String,
    /// 需要覆盖的小时数，1-24，默认 12。
    ///
    /// 注意：中国天气网公开数据是 **3 小时粒度**，返回的是落在该时间窗内的
    /// 每 3 小时一个时点，不会做插值。
    pub hours: Option<u32>,
}

/// `search_cities` 参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchCitiesQuery {
    /// 城市名称的一部分，例如 `南山`、`天河`、`苏州`；也可以直接填 9 位城市代码。
    pub query: String,
    /// 返回条数上限，1-50，默认 10。
    pub limit: Option<u32>,
}

/// `get_weather_alerts` 参数。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AlertQuery {
    /// 城市名称或中国天气网城市代码。
    pub city: String,
}

/// 本地城市库的元信息。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CityIndexMeta {
    /// 城市条目总数。
    pub total: usize,
    /// 城市库生成时间（UTC+8）。
    pub generated_at: String,
    /// 城市库来源。
    pub source: String,
}

/// `search_cities` 输出。
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchCitiesOutput {
    /// 原始查询串。
    pub query: String,
    /// 命中条数。
    pub count: usize,
    /// 城市列表。
    pub cities: Vec<CityInfo>,
    /// 本地城市库元信息。
    pub city_index: CityIndexMeta,
}
