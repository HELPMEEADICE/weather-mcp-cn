//! 统一的天气数据模型。
//!
//! 设计要点：
//! * 所有可能缺失的字段一律用 `Option`，缺失时 **不序列化**，绝不填 0 或编造数值；
//! * 严格区分「观测时间」「预报发布时间」「本地抓取时间」三者，禁止用抓取时间冒充观测时间；
//! * 每条结果都带 `source`，说明数据来自哪个 URL、什么时候抓的。

use chrono::{DateTime, FixedOffset};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 中国标准时间（UTC+8）。中国天气网的所有时间字段都是北京时间。
pub fn cst_offset() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+8 是合法偏移")
}

/// 数据来源与抓取元信息。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceInfo {
    /// 数据提供方标识，固定为 `weather.com.cn`。
    pub provider: String,
    /// 实际请求的上游 URL。
    pub url: String,
    /// 本次抓取的时间（UTC+8）。这是「什么时候抓的」，不是「什么时候观测的」。
    pub fetched_at: DateTime<FixedOffset>,
    /// 数据源页面/接口的名称说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 关于数据本身的额外说明（例如时间字段的解析方式）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl SourceInfo {
    pub fn new(url: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new_at(
            url,
            description,
            chrono::Utc::now().with_timezone(&cst_offset()),
        )
    }

    /// 用**真实抓取时刻**构造来源信息。
    ///
    /// 缓存命中时必须复用第一次抓取时的 `fetched_at`，否则「抓取时间」会被
    /// 每一次响应刷新，变成一个没有意义的字段。
    pub fn new_at(
        url: impl Into<String>,
        description: impl Into<String>,
        fetched_at: DateTime<FixedOffset>,
    ) -> Self {
        Self {
            provider: "weather.com.cn".to_string(),
            url: url.into(),
            fetched_at,
            description: Some(description.into()),
            note: None,
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// 行政区划层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CityLevel {
    /// 直辖市（北京、上海、天津、重庆）。
    Municipality,
    /// 地级市 / 地区 / 自治州 / 盟。
    City,
    /// 区 / 县 / 县级市。
    District,
}

impl CityLevel {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Municipality => "直辖市",
            Self::City => "地级行政区",
            Self::District => "区县",
        }
    }
}

/// 城市信息（含中国天气网城市代码）。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CityInfo {
    /// 中国天气网 9 位城市代码，例如 `101010100`。
    pub code: String,
    /// 数据源给出的城市简称，例如 `北京`、`海淀`。
    pub name: String,
    /// 所属省级行政区名称，例如 `广东`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub province: Option<String>,
    /// 所属地级行政区名称，例如 `广州`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// 区县名称，例如 `天河`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    /// 完整可读名称，例如 `广东省广州市天河区`。
    pub display_name: String,
    /// 行政区划层级。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<CityLevel>,
    /// 解析说明。例如输入「深圳市南山区」但该区在中国天气网没有独立站点时，
    /// 会退回到所属地级市并在此说明原因。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_note: Option<String>,
}

/// 实况观测数据。
///
/// 缺失字段不会出现在 JSON 中；`missing_fields` 明确列出数据源本次没有提供的字段，
/// 便于调用方区分「没数据」和「数据是 0」。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WeatherObservation {
    /// 城市信息。
    pub city: CityInfo,
    /// 当前气温（摄氏度）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// 天气现象，例如 `晴`、`多云`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather: Option<String>,
    /// 体感温度（摄氏度）。中国天气网实况接口不提供该字段，通常为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feels_like_c: Option<f64>,
    /// 相对湿度（百分比，0-100）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub humidity_percent: Option<f64>,
    /// 风向，例如 `北风`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_direction: Option<String>,
    /// 风力等级，例如 `3级`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_scale: Option<String>,
    /// 风速（km/h）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_speed_kmh: Option<f64>,
    /// 过去 1 小时降水量（毫米）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precipitation_mm: Option<f64>,
    /// 过去 24 小时降水量（毫米）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precipitation_24h_mm: Option<f64>,
    /// 气压（百帕）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure_hpa: Option<f64>,
    /// 能见度（公里）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility_km: Option<f64>,
    /// 空气质量指数 AQI。数据源提供时才有值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aqi: Option<u32>,
    /// 观测时间（UTC+8）。数据源只给出「时:分」时按抓取日期与页面日期字段推断。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始的观测时间文本，例如 `11月05日(星期四) 22:30`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at_raw: Option<String>,
    /// 关于观测时间如何得到的说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at_note: Option<String>,
    /// 数据源本次未提供的字段名列表。
    pub missing_fields: Vec<String>,
    /// 数据来源。
    pub source: SourceInfo,
    /// 是否为「实况观测」而非「预报」。恒为 `true`，用于避免把预报温度当成实况。
    pub is_observation: bool,
}

/// 单日天气预报。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DailyForecast {
    /// 日期，格式 `YYYY-MM-DD`。
    pub date: String,
    /// 数据源给出的日期标签，例如 `今天`、`明天`、`周六`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 星期几（中文），例如 `周四`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekday: Option<String>,
    /// 天气现象原文，例如 `晴转多云`、`小雨转雨夹雪`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather: Option<String>,
    /// 白天天气（按 `A转B` 句式拆分得到）。未出现「转」时为空，请读取 `weather`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather_day: Option<String>,
    /// 夜间天气（按 `A转B` 句式拆分得到）。未出现「转」时为空。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather_night: Option<String>,
    /// 最高气温（摄氏度）。数据源未给出时为空，例如当天已入夜时只给最低温。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_high_c: Option<i32>,
    /// 最低气温（摄氏度）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_low_c: Option<i32>,
    /// 白天风向。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_direction_day: Option<String>,
    /// 夜间风向。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_direction_night: Option<String>,
    /// 风力等级，例如 `<3级`、`3-4级`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_scale: Option<String>,
}

/// 未来逐 3 小时预报中的一个时点。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HourlyForecast {
    /// 预报时点（UTC+8，RFC3339 格式）。
    pub time: DateTime<FixedOffset>,
    /// 天气现象。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather: Option<String>,
    /// 气温（摄氏度）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<i32>,
    /// 风向。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_direction: Option<String>,
    /// 风力等级。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wind_scale: Option<String>,
    /// 数据源使用的天气图标代码，例如 `d00`（白天晴）/`n01`（夜间多云）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_code: Option<String>,
}

/// 气象预警信息。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WeatherAlert {
    /// 预警标题，例如 `吉林省发布大风蓝色预警`。
    pub title: String,
    /// 预警类型，例如 `大风`、`寒潮`、`暴雨`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_type: Option<String>,
    /// 预警级别，例如 `蓝色`、`黄色`、`橙色`、`红色`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// 数据源给出的级别代码。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level_code: Option<String>,
    /// 数据源给出的类型代码。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_code: Option<String>,
    /// 发布机构（尽可能从正文中提取，例如 `吉林省气象台`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// 发布时间（UTC+8）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始发布时间文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at_raw: Option<String>,
    /// 预警失效时间（UTC+8），数据源提供时才有值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<FixedOffset>>,
    /// 预警正文。
    pub content: String,
    /// 预警详情页地址。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail_url: Option<String>,
    /// 影响区域描述（省/市/县拼接）。
    pub region: String,
}

/// 天气预报结果集合。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ForecastBundle {
    /// 城市信息。
    pub city: CityInfo,
    /// 预报发布时间（UTC+8），来自页面 `fc_24h_internal_update_time` 等字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始发布时间文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at_raw: Option<String>,
    /// 数据源可提供的最长预报天数。
    pub available_days: usize,
    /// 按 `days` 参数截取的预报。
    pub days: Vec<DailyForecast>,
    /// 关于天气现象拆分方式的说明。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weather_split_note: Option<String>,
    /// 数据来源。
    pub source: SourceInfo,
}

/// 逐 3 小时预报结果集合。
///
/// # `covered_hours` 的确切语义
///
/// MCP 工具返回前会按 `reference_time`（本次调用的「现在」）裁剪时间窗，
/// `covered_hours` 表示**从现在起、被返回的时点真正覆盖到的小时数**：
///
/// ```text
/// coverage_end  = min(最后一个时点 + 3 小时, reference_time + requested_hours)
/// covered_hours = floor((coverage_end - reference_time) / 1 小时)
/// ```
///
/// 因此它永远不会大于 `requested_hours`，也不会大于上游实际给出的数据跨度；
/// 上游只覆盖了一部分窗口时，这里如实给出较小的数字，不做任何「至少 3 小时」的兜底。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HourlyBundle {
    /// 城市信息。
    pub city: CityInfo,
    /// 预报发布时间（UTC+8）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始发布时间文本。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at_raw: Option<String>,
    /// 数据粒度（小时）。中国天气网公开数据为 3 小时粒度。
    pub resolution_hours: u32,
    /// 请求的小时数。
    pub requested_hours: u32,
    /// 从现在起实际被覆盖的小时数（见上方语义说明）。
    pub covered_hours: u32,
    /// 裁剪时间窗所用的「现在」（UTC+8）。由 MCP 工具写入，便于调用方复核窗口。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_time: Option<DateTime<FixedOffset>>,
    /// 最后一个返回时点所在 3 小时区间的结束时刻，且不超过请求的截止时刻。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_end: Option<DateTime<FixedOffset>>,
    /// 逐时点预报。
    pub hours: Vec<HourlyForecast>,
    /// 说明：数据源只有 3 小时粒度，未做任何插值或推算。
    pub note: String,
    /// 数据来源。
    pub source: SourceInfo,
}

/// 气象预警结果集合。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AlertBundle {
    /// 城市信息。
    pub city: CityInfo,
    /// 是否有生效中的预警。`false` 表示已成功查询且确实没有预警。
    pub has_alerts: bool,
    /// 预警列表。
    pub alerts: Vec<WeatherAlert>,
    /// 结论说明，便于调用方区分「没有预警」与「查询失败」。
    pub status: String,
    /// 数据来源。
    pub source: SourceInfo,
}
