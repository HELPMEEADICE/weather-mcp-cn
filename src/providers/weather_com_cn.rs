//! 中国天气网（weather.com.cn）数据源实现。
//!
//! # 真实接口调研结论（2026-11 实测）
//!
//! | 用途 | 地址 | 说明 |
//! | --- | --- | --- |
//! | 实况观测 | `https://d1.weather.com.cn/sk_2d/{code}.html` | 返回 `var dataSK={...}`，含气温/湿度/风向/风力/气压/能见度/降水/AQI/观测时间 |
//! | 城市实况+预报+预警 | `https://d1.weather.com.cn/weather_index/{code}.html` | 同上字段集合再叠加 5 日预报与预警数组 |
//! | 7 日预报 | `https://www.weather.com.cn/weather/{code}.shtml` | HTML，`ul.t > li.sky` 七个条目 |
//! | 逐 3 小时预报 | `https://www.weather.com.cn/weather1d/{code}.shtml` | 页面内联 `var hour3data={...}` |
//! | 气象预警 | `https://www.weather.com.cn/dingzhi/{code}.html` | 返回 `var alarmDZ{code}={...}` |
//!
//! `d1.weather.com.cn` 会校验 `Referer`，缺失时返回 403，因此所有请求都带上
//! `https://www.weather.com.cn/`。
//!
//! 站点 **没有** 提供逐 1 小时预报接口（`hourly_2d`、`hour3data` 等路径均 404/302），
//! 公开数据只有 3 小时粒度，本项目如实返回 3 小时粒度并显式标注，不做插值。

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, NaiveDate, TimeZone};
use regex::Regex;
use scraper::{Html, Selector};
use serde_json::Value;

use crate::cache::TtlCache;
use crate::config::Config;
use crate::error::{Result, WeatherError};
use crate::http::HttpClient;
use crate::models::{
    AlertBundle, CityInfo, DailyForecast, ForecastBundle, HourlyBundle, HourlyForecast, SourceInfo,
    WeatherAlert, WeatherObservation, cst_offset,
};
use crate::providers::{BoxFut, Fetched, TextFetcher, WeatherProvider};
use crate::window::RESOLUTION_HOURS;

/// 站点主页。
pub const HOMEPAGE: &str = "https://www.weather.com.cn/";
/// 静态页面域名。
const WWW_HOST: &str = "https://www.weather.com.cn";
/// 数据接口域名。
const DATA_HOST: &str = "https://d1.weather.com.cn";
/// 数据接口要求的 Referer。
const REFERER: &str = "https://www.weather.com.cn/";

/// 中国天气网数据源。
///
/// # 缓存里到底存什么
///
/// 四个缓存的 **值** 都是「可共享的上游数据 + 抓取时刻」（[`Fetched`]），
/// 一律不含 [`CityInfo`]、`resolution_note`、`source.url` 这些与单次请求绑定的东西：
///
/// | 缓存 | Key | Value |
/// | --- | --- | --- |
/// | 实况 | 城市代码 | [`ObservationPayload`]（纯气象字段） |
/// | 逐日预报 | 城市代码 | [`ForecastPayload`] |
/// | 逐 3 小时 | 城市代码 | [`HourlyPayload`] |
/// | 气象预警 | 城市代码 | [`AlertPayload`] |
///
/// 因此「先查深圳市粤海街道（回退到深圳并带 resolution_note）、再查深圳」不会互相
/// 继承说明；`source.fetched_at` 也永远是真正抓取的那一刻，而不是本次响应的时间。
pub struct WeatherComCn {
    fetcher: Arc<dyn TextFetcher>,
    current_cache: TtlCache<String, Fetched<ObservationPayload>>,
    forecast_cache: TtlCache<String, Fetched<ForecastPayload>>,
    hourly_cache: TtlCache<String, Fetched<HourlyPayload>>,
    alert_cache: TtlCache<String, Fetched<AlertPayload>>,
}

/// 判断响应是否是站点的 404 页面。
///
/// 中国天气网对不存在的城市代码并不总是回 404：静态页面会 302 到
/// `other/weather_error_404.html`，数据接口有时直接返回这段 HTML。
fn is_error_page(body: &str) -> bool {
    body.contains("您访问的页面不存在") || body.contains("weather_error_404")
}

/// 上游用「错误页」表达「这个城市代码没有数据」，统一映射成 404。
fn not_found(url: &str) -> WeatherError {
    WeatherError::UpstreamStatus {
        status: 404,
        url: url.to_string(),
    }
}

impl WeatherComCn {
    /// 依据配置构建数据源（含各级缓存）。
    pub fn new(config: &Config) -> Result<Self> {
        let http = HttpClient::new(config)?;
        Ok(Self::with_fetcher(config, Arc::new(http)))
    }

    /// 用自定义的文本上游构建数据源。
    ///
    /// 生产路径用 [`HttpClient`]；测试用它回放落盘的真实样本，从而在没有网络的
    /// 情况下验证缓存命中、singleflight 与请求上下文绑定。
    pub fn with_fetcher(config: &Config, fetcher: Arc<dyn TextFetcher>) -> Self {
        Self {
            fetcher,
            current_cache: TtlCache::new(config.observation_ttl, config.cache_capacity),
            forecast_cache: TtlCache::new(config.forecast_ttl, config.cache_capacity),
            hourly_cache: TtlCache::new(config.hourly_ttl, config.cache_capacity),
            alert_cache: TtlCache::new(config.alert_ttl, config.cache_capacity),
        }
    }

    /// 各接口地址，便于测试与文档引用。
    pub fn endpoints(code: &str) -> Endpoints {
        Endpoints {
            current: format!("{DATA_HOST}/sk_2d/{code}.html"),
            index: format!("{DATA_HOST}/weather_index/{code}.html"),
            forecast: format!("{WWW_HOST}/weather/{code}.shtml"),
            hourly: format!("{WWW_HOST}/weather1d/{code}.shtml"),
            alerts: format!("{DATA_HOST}/dingzhi/{code}.html"),
        }
    }

    /// 取一个上游页面（统一带上站点要求的 Referer）。
    async fn fetch_text(&self, url: &str) -> Result<String> {
        let body = self.fetcher.get_text(url, Some(REFERER)).await?;
        if is_error_page(&body) {
            return Err(not_found(url));
        }
        Ok(body)
    }

    /// 只负责「抓 + 解析」，不含任何城市上下文。
    async fn load_current(&self, code: &str) -> Result<Fetched<ObservationPayload>> {
        let url = Self::endpoints(code).current;
        let body = self.fetch_text(&url).await?;
        let payload = parse_current_js(&body)?;
        Ok(Fetched::new(payload, cst_now()))
    }

    async fn load_forecast(&self, code: &str) -> Result<Fetched<ForecastPayload>> {
        let url = Self::endpoints(code).forecast;
        let body = self.fetch_text(&url).await?;
        let payload = parse_forecast_page(&body)?;
        Ok(Fetched::new(payload, cst_now()))
    }

    async fn load_hourly(&self, code: &str) -> Result<Fetched<HourlyPayload>> {
        let url = Self::endpoints(code).hourly;
        let body = self.fetch_text(&url).await?;
        let payload = parse_hourly_page(&body)?;
        Ok(Fetched::new(payload, cst_now()))
    }

    async fn load_alerts(&self, code: &str) -> Result<Fetched<AlertPayload>> {
        let url = Self::endpoints(code).alerts;
        let body = self.fetch_text(&url).await?;
        let payload = parse_alerts_payload(&body)?;
        Ok(Fetched::new(payload, cst_now()))
    }
}

impl WeatherProvider for WeatherComCn {
    fn id(&self) -> &'static str {
        "weather.com.cn"
    }

    fn homepage(&self) -> &'static str {
        HOMEPAGE
    }

    fn current_weather<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<WeatherObservation>> {
        Box::pin(async move {
            let code = city.code.clone();
            let fetched = self
                .current_cache
                .get_or_try_insert_with(code.clone(), || self.load_current(&code))
                .await?;
            // 城市上下文在这里、按本次请求绑定，缓存里只有气象数据。
            Ok(fetched.data.into_observation(
                city.clone(),
                Self::endpoints(&code).current,
                fetched.fetched_at,
            ))
        })
    }

    fn daily_forecast<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<ForecastBundle>> {
        Box::pin(async move {
            let code = city.code.clone();
            let fetched = self
                .forecast_cache
                .get_or_try_insert_with(code.clone(), || self.load_forecast(&code))
                .await?;
            Ok(fetched.data.into_bundle(
                city.clone(),
                Self::endpoints(&code).forecast,
                fetched.fetched_at,
            ))
        })
    }

    fn hourly_forecast<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<HourlyBundle>> {
        Box::pin(async move {
            let code = city.code.clone();
            let fetched = self
                .hourly_cache
                .get_or_try_insert_with(code.clone(), || self.load_hourly(&code))
                .await?;
            Ok(fetched.data.into_bundle(
                city.clone(),
                Self::endpoints(&code).hourly,
                fetched.fetched_at,
            ))
        })
    }

    fn alerts<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<AlertBundle>> {
        Box::pin(async move {
            let code = city.code.clone();
            let fetched = self
                .alert_cache
                .get_or_try_insert_with(code.clone(), || self.load_alerts(&code))
                .await?;
            Ok(fetched.data.into_bundle(
                city.clone(),
                Self::endpoints(&code).alerts,
                fetched.fetched_at,
            ))
        })
    }
}

/// 中国天气网各接口地址。
#[derive(Debug, Clone)]
pub struct Endpoints {
    /// 实况观测。
    pub current: String,
    /// 实况 + 预报 + 预警聚合接口。
    pub index: String,
    /// 7 日预报页面。
    pub forecast: String,
    /// 逐 3 小时预报页面。
    pub hourly: String,
    /// 预警接口。
    pub alerts: String,
}

// ---------------------------------------------------------------------------
// 实况解析
// ---------------------------------------------------------------------------

/// 实况接口解析结果（尚未绑定城市与来源）。
#[derive(Debug, Default, Clone)]
pub struct ObservationPayload {
    pub city_name: Option<String>,
    pub temperature_c: Option<f64>,
    pub weather: Option<String>,
    pub humidity_percent: Option<f64>,
    pub wind_direction: Option<String>,
    pub wind_scale: Option<String>,
    pub wind_speed_kmh: Option<f64>,
    pub precipitation_mm: Option<f64>,
    pub precipitation_24h_mm: Option<f64>,
    pub pressure_hpa: Option<f64>,
    pub visibility_km: Option<f64>,
    pub aqi: Option<u32>,
    pub observed_at: Option<DateTime<FixedOffset>>,
    pub observed_at_raw: Option<String>,
    pub observed_at_note: Option<String>,
}

impl ObservationPayload {
    /// 绑定城市、来源与抓取时刻，得到对外的 [`WeatherObservation`]。
    ///
    /// `fetched_at` 必须是真正抓取这份数据的时刻（缓存命中时沿用第一次的值），
    /// 不能用「本次响应的时间」冒充。
    pub fn into_observation(
        self,
        mut city: CityInfo,
        url: String,
        fetched_at: DateTime<FixedOffset>,
    ) -> WeatherObservation {
        if let Some(name) = self.city_name.as_deref()
            && city.level.is_none()
            && !name.is_empty()
        {
            // 用户直接输入城市代码时，用数据源回填更可读的名称。
            city.name = name.to_string();
            city.display_name = name.to_string();
        }

        let mut missing = Vec::new();
        macro_rules! track {
            ($field:expr, $name:literal) => {
                if $field.is_none() {
                    missing.push($name.to_string());
                }
            };
        }
        track!(self.feels_like(), "feels_like_c");
        track!(self.temperature_c, "temperature_c");
        track!(self.weather, "weather");
        track!(self.humidity_percent, "humidity_percent");
        track!(self.wind_direction, "wind_direction");
        track!(self.wind_scale, "wind_scale");
        track!(self.wind_speed_kmh, "wind_speed_kmh");
        track!(self.precipitation_mm, "precipitation_mm");
        track!(self.pressure_hpa, "pressure_hpa");
        track!(self.visibility_km, "visibility_km");
        track!(self.observed_at, "observed_at");

        let source = SourceInfo::new_at(url, "中国天气网实况观测接口（sk_2d）", fetched_at)
            .with_note("该接口不提供体感温度、降水概率等字段，缺失字段不会出现在结果中");

        WeatherObservation {
            city,
            temperature_c: self.temperature_c,
            weather: self.weather,
            feels_like_c: None,
            humidity_percent: self.humidity_percent,
            wind_direction: self.wind_direction,
            wind_scale: self.wind_scale,
            wind_speed_kmh: self.wind_speed_kmh,
            precipitation_mm: self.precipitation_mm,
            precipitation_24h_mm: self.precipitation_24h_mm,
            pressure_hpa: self.pressure_hpa,
            visibility_km: self.visibility_km,
            aqi: self.aqi,
            observed_at: self.observed_at,
            observed_at_raw: self.observed_at_raw,
            observed_at_note: self.observed_at_note,
            missing_fields: missing,
            source,
            is_observation: true,
        }
    }

    fn feels_like(&self) -> Option<f64> {
        // 数据源不提供体感温度，这里恒为 None，仅用于 missing_fields 统计。
        None
    }
}

/// 解析 `var dataSK={...}`。
pub fn parse_current_js(body: &str) -> Result<ObservationPayload> {
    let raw = extract_json_object(body, "dataSK").ok_or_else(|| {
        WeatherError::Parse("实况接口未返回 dataSK 数据块（接口结构可能已变化）".into())
    })?;
    let value: Value = serde_json::from_str(raw)?;

    let observed_time = json_text(&value, "time");
    let observed_date = json_text(&value, "date");
    let (observed_at, note) = match (observed_time.as_deref(), observed_date.as_deref()) {
        (Some(time), Some(date)) => (
            observed_datetime(time, date),
            Some(format!(
                "数据源只给出「{date} {time}」，按北京时间（UTC+8）解析，年份由抓取日期推断"
            )),
        ),
        (Some(time), None) => (
            observed_datetime_from_now(time),
            Some(format!(
                "数据源只给出「{time}」，按北京时间（UTC+8）就近推断日期"
            )),
        ),
        _ => (None, Some("数据源未提供观测时间".to_string())),
    };

    Ok(ObservationPayload {
        city_name: json_text(&value, "cityname"),
        temperature_c: json_number(&value, "temp"),
        weather: json_text(&value, "weather"),
        humidity_percent: json_number(&value, "SD"),
        wind_direction: json_text(&value, "WD"),
        wind_scale: json_text(&value, "WS"),
        wind_speed_kmh: json_unit_number(&value, "wse"),
        precipitation_mm: json_number(&value, "rain"),
        precipitation_24h_mm: json_number(&value, "rain24h"),
        pressure_hpa: json_number(&value, "qy"),
        visibility_km: json_unit_number(&value, "njd"),
        aqi: json_u32(&value, "aqi"),
        observed_at,
        observed_at_raw: match (observed_date, observed_time) {
            (Some(date), Some(time)) => Some(format!("{date} {time}")),
            (None, Some(time)) => Some(time),
            (Some(date), None) => Some(date),
            (None, None) => None,
        },
        observed_at_note: note,
    })
}

// ---------------------------------------------------------------------------
// 7 日预报解析
// ---------------------------------------------------------------------------

/// 7 日预报页面的解析结果（不含城市与来源，因此可以安全地共享/缓存）。
#[derive(Debug, Clone)]
pub struct ForecastPayload {
    /// 预报发布时间（UTC+8）。
    pub published_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始发布时间文本。
    pub published_at_raw: Option<String>,
    /// 逐日预报（按日期升序）。
    pub days: Vec<DailyForecast>,
}

impl ForecastPayload {
    /// 绑定本次请求的城市、来源 URL 与抓取时刻。
    pub fn into_bundle(
        self,
        city: CityInfo,
        url: String,
        fetched_at: DateTime<FixedOffset>,
    ) -> ForecastBundle {
        ForecastBundle {
            city,
            published_at: self.published_at,
            published_at_raw: self.published_at_raw,
            available_days: self.days.len(),
            days: self.days,
            weather_split_note: Some(WEATHER_SPLIT_NOTE.to_string()),
            source: SourceInfo::new_at(
                url,
                "中国天气网 7 日预报页面（weather/{code}.shtml）",
                fetched_at,
            ),
        }
    }
}

/// 天气现象拆分口径，与 `weather_day` / `weather_night` 一一对应。
const WEATHER_SPLIT_NOTE: &str = "weather 为数据源原文；weather_day / weather_night 只按「A转B」句式拆分，未出现「转」时 weather_night 为空，此时表示全天天气一致";

/// 解析 7 日预报页面。
pub fn parse_forecast_html(html: &str, city: CityInfo, url: String) -> Result<ForecastBundle> {
    Ok(parse_forecast_page(html)?.into_bundle(city, url, cst_now()))
}

/// 只解析页面内容，不绑定任何请求上下文。
pub fn parse_forecast_page(html: &str) -> Result<ForecastPayload> {
    let document = Html::parse_document(html);
    let item_selector = Selector::parse("ul.t > li").expect("静态选择器合法");
    let wea_selector = Selector::parse("p.wea").expect("静态选择器合法");
    let tem_selector = Selector::parse("p.tem").expect("静态选择器合法");
    let high_selector = Selector::parse("span").expect("静态选择器合法");
    let low_selector = Selector::parse("i").expect("静态选择器合法");
    let win_selector = Selector::parse("p.win").expect("静态选择器合法");
    let dir_selector = Selector::parse("span").expect("静态选择器合法");
    let scale_selector = Selector::parse("i").expect("静态选择器合法");
    let heading_selector = Selector::parse("h1").expect("静态选择器合法");

    let fc_24h = hidden_input(&document, "fc_24h_internal_update_time");
    let update_time = hidden_input(&document, "update_time");
    let published_at = publish_datetime(fc_24h.as_deref(), update_time.as_deref());
    let base_date = published_at
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| cst_now().date_naive());
    let published_raw = describe_publish(fc_24h.as_deref(), update_time.as_deref());

    let mut days: Vec<DailyForecast> = Vec::new();
    let mut cursor = base_date;
    for item in document.select(&item_selector) {
        let heading = item
            .select(&heading_selector)
            .next()
            .map(|node| node.text().collect::<String>())
            .unwrap_or_default();
        if heading.is_empty() {
            continue;
        }
        let (day_of_month, label) = parse_day_heading(&heading);
        let Some(day_of_month) = day_of_month else {
            continue;
        };
        let date = if label.as_deref() == Some("今天") {
            base_date
        } else {
            next_date_with_day(cursor, day_of_month).unwrap_or(base_date)
        };
        cursor = date;

        let weather = item
            .select(&wea_selector)
            .next()
            .map(|node| node.text().collect::<String>().trim().to_string())
            .filter(|text| !text.is_empty());
        let (weather_day, weather_night) = split_weather(weather.as_deref());

        let temperature = item.select(&tem_selector).next();
        let temperature_high_c = temperature
            .and_then(|node| node.select(&high_selector).next())
            .and_then(|node| parse_int(&node.text().collect::<String>()));
        let temperature_low_c = temperature
            .and_then(|node| node.select(&low_selector).next())
            .and_then(|node| parse_int(&node.text().collect::<String>()));

        let win = item.select(&win_selector).next();
        let mut directions: Vec<String> = Vec::new();
        if let Some(win) = win {
            for node in win.select(&dir_selector) {
                if let Some(title) = node.value().attr("title")
                    && !title.trim().is_empty()
                {
                    directions.push(title.trim().to_string());
                }
            }
        }
        let wind_scale = win
            .map(|node| {
                node.select(&scale_selector)
                    .map(|n| n.text().collect::<String>().trim().to_string())
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .filter(|text| !text.is_empty());

        days.push(DailyForecast {
            date: date.format("%Y-%m-%d").to_string(),
            label: label.clone(),
            weekday: weekday_zh(date),
            weather,
            weather_day,
            weather_night,
            temperature_high_c,
            temperature_low_c,
            wind_direction_day: directions.first().cloned(),
            wind_direction_night: directions.get(1).cloned(),
            wind_scale,
        });
    }

    if days.is_empty() {
        return Err(WeatherError::Parse(
            "7 日预报页面未解析到任何 `ul.t > li` 条目（页面结构可能已变化）".into(),
        ));
    }

    Ok(ForecastPayload {
        published_at,
        published_at_raw: published_raw,
        days,
    })
}

// ---------------------------------------------------------------------------
// 逐 3 小时预报解析
// ---------------------------------------------------------------------------

/// 逐 3 小时预报页面的解析结果（不含城市与来源，因此可以安全地共享/缓存）。
#[derive(Debug, Clone)]
pub struct HourlyPayload {
    /// 预报发布时间（UTC+8）。
    pub published_at: Option<DateTime<FixedOffset>>,
    /// 数据源原始发布时间文本。
    pub published_at_raw: Option<String>,
    /// 时点列表（按时间升序、去重、保留上游原始的 3 小时粒度）。
    pub points: Vec<HourlyForecast>,
}

impl HourlyPayload {
    /// 绑定本次请求的城市、来源 URL 与抓取时刻。
    ///
    /// 这里**不裁剪时间窗**，也不编造 `requested_hours` / `covered_hours`：
    /// 窗口与覆盖时长是「请求」的属性，由 MCP 工具用真实当前时间计算
    /// （见 [`crate::window`]）。本函数给出的 `covered_hours` 只是「上游数据本身
    /// 的总跨度」，方便调用方了解数据量。
    pub fn into_bundle(
        self,
        city: CityInfo,
        url: String,
        fetched_at: DateTime<FixedOffset>,
    ) -> HourlyBundle {
        let covered_hours = self
            .points
            .first()
            .zip(self.points.last())
            .map(|(first, last)| (last.time - first.time).num_hours() + RESOLUTION_HOURS)
            .unwrap_or(0)
            .clamp(0, i64::from(u32::MAX)) as u32;
        HourlyBundle {
            city,
            published_at: self.published_at,
            published_at_raw: self.published_at_raw,
            resolution_hours: RESOLUTION_HOURS as u32,
            requested_hours: 0,
            covered_hours,
            reference_time: None,
            coverage_end: None,
            hours: self.points,
            note: HOURLY_GRANULARITY_NOTE.to_string(),
            source: SourceInfo::new_at(url, "中国天气网 1 日页面内联数据（hour3data）", fetched_at),
        }
    }
}

/// 粒度说明，写入 `HourlyBundle.note`。
pub const HOURLY_GRANULARITY_NOTE: &str =
    "中国天气网公开数据为 3 小时粒度，本项目未做任何插值或推算；时间均为北京时间（UTC+8）";

/// 解析 1 日页面内联的 `hour3data`。
pub fn parse_hourly_html(html: &str, city: CityInfo, url: String) -> Result<HourlyBundle> {
    Ok(parse_hourly_page(html)?.into_bundle(city, url, cst_now()))
}

/// 只解析页面内容，不绑定任何请求上下文。
pub fn parse_hourly_page(html: &str) -> Result<HourlyPayload> {
    let document = Html::parse_document(html);
    let publish_raw = hidden_input(&document, "fc_3h_internal_update_time")
        .or_else(|| hidden_input(&document, "fc_24h_internal_update_time"));
    let published_at = publish_raw.as_deref().and_then(parse_datetime_flexible);

    let raw = extract_json_object(html, "hour3data").ok_or_else(|| {
        WeatherError::Parse("1 日页面未包含 hour3data 数据块（数据源可能已调整）".into())
    })?;
    let value: Value = serde_json::from_str(raw)?;

    let mut points: Vec<HourlyForecast> = Vec::new();
    let base = published_at.unwrap_or_else(cst_now);
    if let Some(list) = value.get("1d").and_then(Value::as_array) {
        for entry in list {
            if let Some(forecast) = parse_hour_point(entry.as_str(), base) {
                points.push(forecast);
            }
        }
    }
    // `1d` 只有 8 个时点（24 小时）。不足 8 个时回落到多日字段：
    // 实测线上字段名为 `23d`（数组的数组），老版本/部分页面用过 `7d`。
    if points.len() < 8 {
        for key in ["23d", "7d"] {
            let Some(days) = value.get(key).and_then(Value::as_array) else {
                continue;
            };
            for day in days {
                let Some(list) = day.as_array() else {
                    continue;
                };
                for entry in list {
                    if let Some(forecast) = parse_hour_point(entry.as_str(), base)
                        && !points.iter().any(|p| p.time == forecast.time)
                    {
                        points.push(forecast);
                    }
                }
            }
            if points.len() >= 8 {
                break;
            }
        }
    }

    if points.is_empty() {
        return Err(WeatherError::Parse(
            "hour3data 中没有可解析的时点（数据源可能已调整结构）".into(),
        ));
    }
    points.sort_by_key(|point| point.time);
    points.dedup_by_key(|point| point.time);
    // 最多保留 32 个时点（约 96 小时），避免长期占用内存。
    points.truncate(32);

    Ok(HourlyPayload {
        published_at,
        published_at_raw: publish_raw,
        points,
    })
}

fn parse_hour_point(
    entry: Option<&str>,
    base: DateTime<chrono::FixedOffset>,
) -> Option<HourlyForecast> {
    let entry = entry?;
    let fields: Vec<&str> = entry.split(',').collect();
    if fields.len() < 6 {
        return None;
    }
    let time_text = fields[0];
    let re = regex_time_bucket();
    let captures = re.captures(time_text)?;
    let day: u32 = captures.get(1)?.as_str().parse().ok()?;
    let hour: u32 = captures.get(2)?.as_str().parse().ok()?;

    // 数据源只给出「日 + 时」，月份与年份由发布时间推断：日号小于基准日号即进入下个月。
    let mut year = base.year_of();
    let mut month = base.month_of();
    if day < base.day_of() {
        if month == 12 {
            month = 1;
            year += 1;
        } else {
            month += 1;
        }
    }
    let time = build_cst(year, month, day, hour, 0)?;

    Some(HourlyForecast {
        time,
        icon_code: Some(fields[1].trim().to_string()).filter(|v| !v.is_empty()),
        weather: Some(fields[2].trim().to_string()).filter(|v| !v.is_empty()),
        temperature_c: parse_int(fields[3]),
        wind_direction: Some(fields[4].trim().to_string()).filter(|v| !v.is_empty()),
        wind_scale: Some(fields[5].trim().to_string()).filter(|v| !v.is_empty()),
    })
}

// ---------------------------------------------------------------------------
// 预警解析
// ---------------------------------------------------------------------------

/// 预警接口的解析结果（不含城市与来源，因此可以安全地共享/缓存）。
#[derive(Debug, Clone, Default)]
pub struct AlertPayload {
    /// 当前生效的预警列表。空列表意味着**上游明确给出空数组**。
    pub alerts: Vec<WeatherAlert>,
}

impl AlertPayload {
    /// 绑定本次请求的城市、来源 URL 与抓取时刻。
    pub fn into_bundle(
        self,
        city: CityInfo,
        url: String,
        fetched_at: DateTime<FixedOffset>,
    ) -> AlertBundle {
        let has_alerts = !self.alerts.is_empty();
        AlertBundle {
            city,
            has_alerts,
            status: if has_alerts {
                "active_alerts_found".to_string()
            } else {
                "no_active_alerts".to_string()
            },
            alerts: self.alerts,
            source: SourceInfo::new_at(url, "中国天气网预警接口（dingzhi）", fetched_at),
        }
    }
}

/// 解析 `var alarmDZ{code}={...}`，返回预警列表。
pub fn parse_alerts_js(body: &str) -> Result<Vec<WeatherAlert>> {
    Ok(parse_alerts_payload(body)?.alerts)
}

/// 严格解析预警接口。
///
/// 只有**上游明确返回 `{"w":[]}`** 才算「查询成功且当前无预警」。以下情况一律报
/// [`WeatherError::Parse`]，绝不再被误判成「没有预警」：
///
/// * `w` 字段缺失；
/// * `w` 是 `null`、字符串、对象或其它非数组值；
/// * 顶层不是 JSON 对象；
/// * JSON 被截断/损坏，或返回的根本是 HTML 错误页。
///
/// 单条预警内部的字段缺失**不会**丢弃整条预警（那等于静默吞掉真实预警）：
/// 缺 `w13` 标题时按「区域 + 类型 + 级别」合成一个可读标题，缺正文时保留空串，
/// 只有「数组元素根本不是对象」这种结构性损坏才报错。
pub fn parse_alerts_payload(body: &str) -> Result<AlertPayload> {
    let Some(raw) = extract_json_object(body, "alarmDZ") else {
        return Err(WeatherError::Parse(
            "预警接口未返回 alarmDZ 数据块（接口结构可能已变化，或返回的是 HTML 错误页）".into(),
        ));
    };
    let value: Value = serde_json::from_str(raw)?;
    if !value.is_object() {
        return Err(WeatherError::Parse(format!(
            "预警数据结构异常：顶层应为对象，实际是 {}",
            json_type_name(&value)
        )));
    }

    let Some(field) = value.get("w") else {
        return Err(WeatherError::Parse(
            "预警数据结构异常：缺少 w 字段（无法区分「没有预警」与「查询失败」，按失败处理）"
                .into(),
        ));
    };
    let Some(list) = field.as_array() else {
        return Err(WeatherError::Parse(format!(
            "预警数据结构异常：w 字段应为数组，实际是 {}（不能当成空预警处理）",
            json_type_name(field)
        )));
    };

    let mut alerts = Vec::with_capacity(list.len());
    for (index, item) in list.iter().enumerate() {
        if !item.is_object() {
            return Err(WeatherError::Parse(format!(
                "预警数据结构异常：w[{index}] 应为对象，实际是 {}",
                json_type_name(item)
            )));
        }

        let text = |key: &str| {
            item.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        let content = text("w9").unwrap_or_default();
        let alert_type = text("w5");
        let level = text("w7");
        let region: String = ["w1", "w2", "w3"]
            .iter()
            .filter_map(|key| text(key))
            .collect();
        let title = text("w13").unwrap_or_else(|| {
            synthesize_alert_title(&region, alert_type.as_deref(), level.as_deref(), &content)
        });
        let published_raw = text("w8");

        alerts.push(WeatherAlert {
            title,
            alert_type,
            level,
            level_code: text("w6"),
            type_code: text("w4"),
            publisher: extract_publisher(&content),
            published_at: published_raw.as_deref().and_then(parse_datetime_flexible),
            published_at_raw: published_raw,
            expires_at: text("w15").as_deref().and_then(parse_datetime_flexible),
            content,
            detail_url: text("w11").map(|path| format!("{WWW_HOST}/alarm/{path}")),
            region,
        });
    }
    Ok(AlertPayload { alerts })
}

/// 缺标题时的兜底：用区域 + 类型 + 级别拼一个可读标题。
///
/// 宁可给一个描述性标题，也不要丢掉这条预警。
fn synthesize_alert_title(
    region: &str,
    alert_type: Option<&str>,
    level: Option<&str>,
    content: &str,
) -> String {
    if let Some(alert_type) = alert_type {
        let level = level.unwrap_or("");
        return if region.is_empty() {
            format!("{alert_type}{level}预警")
        } else {
            format!("{region}发布{alert_type}{level}预警")
        };
    }
    let excerpt: String = content.chars().take(40).collect();
    if excerpt.trim().is_empty() {
        "未命名气象预警（数据源未给出标题与正文）".to_string()
    } else {
        excerpt
    }
}

/// JSON 值的中文类型名，用于把「结构不对」说得具体一点。
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "布尔值",
        Value::Number(_) => "数字",
        Value::String(_) => "字符串",
        Value::Array(_) => "数组",
        Value::Object(_) => "对象",
    }
}

fn extract_publisher(content: &str) -> Option<String> {
    let re = regex_publisher();
    re.captures(content)
        .and_then(|caps| caps.get(1))
        .map(|matched| matched.as_str().trim().to_string())
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// 通用工具
// ---------------------------------------------------------------------------

/// 从 `marker` 之后的第一个 `{` 开始做花括号配平，返回完整的 JSON 文本。
///
/// 这些接口把 JSON 内联在 `<script>` 里，直接正则截取会在嵌套 `{}` 或字符串里的
/// 花括号上出错，因此按字符扫描并跳过字符串内容。
pub fn extract_json_object<'a>(source: &'a str, marker: &str) -> Option<&'a str> {
    let marker_at = source.find(marker)?;
    let bytes = source.as_bytes();
    let mut index = marker_at + marker.len();
    while index < bytes.len() && bytes[index] != b'{' {
        index += 1;
    }
    if index >= bytes.len() {
        return None;
    }
    let start = index;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&source[start..=index]);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    None
}

fn json_text(value: &Value, key: &str) -> Option<String> {
    let raw = value.get(key)?;
    let text = match raw {
        Value::String(text) => text.trim().to_string(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    if text.is_empty() || is_sentinel(&text) {
        None
    } else {
        Some(text)
    }
}

fn json_number(value: &Value, key: &str) -> Option<f64> {
    let raw = value.get(key)?;
    let number = match raw {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            let cleaned = text.trim().trim_end_matches('%').trim();
            if cleaned.is_empty() || is_sentinel(cleaned) {
                return None;
            }
            cleaned.parse::<f64>().ok()?
        }
        _ => return None,
    };
    if is_sentinel_number(number) {
        None
    } else {
        Some(number)
    }
}

fn json_u32(value: &Value, key: &str) -> Option<u32> {
    let number = json_number(value, key)?;
    if number.is_sign_negative() {
        return None;
    }
    Some(number.round().max(0.0) as u32)
}

/// 形如 `12km/h`、`30km`、`<3级` 的带单位字段，取其中第一个数字。
fn json_unit_number(value: &Value, key: &str) -> Option<f64> {
    let text = json_text(value, key)?;
    let re = regex_first_number();
    re.find(&text)?.as_str().parse::<f64>().ok()
}

fn is_sentinel(text: &str) -> bool {
    matches!(
        text,
        "暂无" | "无" | "N/A" | "n/a" | "null" | "-" | "--" | "999" | "-999" | "9999"
    )
}

fn is_sentinel_number(number: f64) -> bool {
    number == 999.0 || number == -999.0 || number == 9999.0 || !number.is_finite()
}

fn parse_int(text: &str) -> Option<i32> {
    let re = regex_first_int();
    re.find(text)?.as_str().parse::<i32>().ok()
}

fn split_weather(text: Option<&str>) -> (Option<String>, Option<String>) {
    match text {
        Some(text) => match text.split_once('转') {
            Some((day, night)) => (
                Some(day.trim().to_string()).filter(|v| !v.is_empty()),
                Some(night.trim().to_string()).filter(|v| !v.is_empty()),
            ),
            None => (Some(text.trim().to_string()), None),
        },
        None => (None, None),
    }
}

fn hidden_input(document: &Html, id: &str) -> Option<String> {
    let selector = Selector::parse("input").ok()?;
    for node in document.select(&selector) {
        if node.value().attr("id") == Some(id) {
            return node
                .value()
                .attr("value")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
        }
    }
    None
}

fn parse_day_heading(heading: &str) -> (Option<u32>, Option<String>) {
    let re = regex_day_heading();
    let captures = re.captures(heading);
    let day = captures
        .as_ref()
        .and_then(|caps| caps.get(1))
        .and_then(|m| m.as_str().parse::<u32>().ok());
    let label = captures
        .as_ref()
        .and_then(|caps| caps.get(2))
        .map(|m| m.as_str().to_string());
    (day, label)
}

fn next_date_with_day(cursor: NaiveDate, day_of_month: u32) -> Option<NaiveDate> {
    let mut year = cursor.year_of();
    let mut month = cursor.month_of();
    for _ in 0..14 {
        if let Some(candidate) = NaiveDate::from_ymd_opt(year, month, day_of_month)
            && candidate >= cursor
        {
            return Some(candidate);
        }
        if month == 12 {
            month = 1;
            year += 1;
        } else {
            month += 1;
        }
    }
    None
}

fn weekday_zh(date: NaiveDate) -> Option<String> {
    use chrono::Datelike;
    let label = match date.weekday().number_from_monday() {
        1 => "周一",
        2 => "周二",
        3 => "周三",
        4 => "周四",
        5 => "周五",
        6 => "周六",
        7 => "周日",
        _ => return None,
    };
    Some(label.to_string())
}

fn describe_publish(fc_24h: Option<&str>, update_time: Option<&str>) -> Option<String> {
    match (fc_24h, update_time) {
        (Some(fc), Some(update)) => Some(format!(
            "update_time={update}; fc_24h_internal_update_time={fc}"
        )),
        (Some(fc), None) => Some(format!("fc_24h_internal_update_time={fc}")),
        (None, Some(update)) => Some(format!("update_time={update}")),
        (None, None) => None,
    }
}

/// 组合「页面日期 + update_time」得到预报发布时间。
fn publish_datetime(
    fc_24h: Option<&str>,
    update_time: Option<&str>,
) -> Option<DateTime<chrono::FixedOffset>> {
    let base = fc_24h.and_then(parse_datetime_flexible);
    match (base, update_time.and_then(parse_hhmm)) {
        (Some(base), Some((hour, minute))) => {
            build_cst(base.year_of(), base.month_of(), base.day_of(), hour, minute)
        }
        (Some(base), None) => Some(base),
        (None, Some((hour, minute))) => {
            let today = cst_now().date_naive();
            build_cst(
                today.year_of(),
                today.month_of(),
                today.day_of(),
                hour,
                minute,
            )
        }
        (None, None) => None,
    }
}

fn parse_hhmm(text: &str) -> Option<(u32, u32)> {
    let re = regex_hhmm();
    let caps = re.captures(text.trim())?;
    Some((
        caps.get(1)?.as_str().parse().ok()?,
        caps.get(2)?.as_str().parse().ok()?,
    ))
}

/// 解析多种时间格式：`2026-11-05 18:00`、`2026110518`、`2026-11-05 20:00:00.0`。
pub fn parse_datetime_flexible(text: &str) -> Option<DateTime<chrono::FixedOffset>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f") {
        return cst_offset().from_local_datetime(&parsed).single();
    }
    if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M") {
        return cst_offset().from_local_datetime(&parsed).single();
    }
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return build_cst(date.year_of(), date.month_of(), date.day_of(), 0, 0);
    }
    if text.len() == 10 && text.bytes().all(|b| b.is_ascii_digit()) {
        let year: i32 = text[0..4].parse().ok()?;
        let month: u32 = text[4..6].parse().ok()?;
        let day: u32 = text[6..8].parse().ok()?;
        let hour: u32 = text[8..10].parse().ok()?;
        return build_cst(year, month, day, hour, 0);
    }
    if text.len() == 12 && text.bytes().all(|b| b.is_ascii_digit()) {
        let year: i32 = text[0..4].parse().ok()?;
        let month: u32 = text[4..6].parse().ok()?;
        let day: u32 = text[6..8].parse().ok()?;
        let hour: u32 = text[8..10].parse().ok()?;
        let minute: u32 = text[10..12].parse().ok()?;
        return build_cst(year, month, day, hour, minute);
    }
    None
}

/// 用「页面日期 + 时:分」推算观测时间。
fn observed_datetime(time: &str, date: &str) -> Option<DateTime<chrono::FixedOffset>> {
    let (hour, minute) = parse_hhmm(time)?;
    let (month, day) = parse_month_day(date)?;
    let now = cst_now();
    let mut year = now.year_of();
    if let Some(candidate) = build_cst(year, month, day, hour, minute)
        && candidate > now + ChronoDuration::hours(24)
    {
        year -= 1;
    }
    build_cst(year, month, day, hour, minute)
}

/// 只有「时:分」时，按最近的时刻推断日期（用于拿不到日期字段的兜底）。
fn observed_datetime_from_now(time: &str) -> Option<DateTime<chrono::FixedOffset>> {
    let (hour, minute) = parse_hhmm(time)?;
    let now = cst_now();
    let today = now.date_naive();
    let candidate = build_cst(
        today.year_of(),
        today.month_of(),
        today.day_of(),
        hour,
        minute,
    )?;
    if candidate > now + ChronoDuration::minutes(30) {
        Some(candidate - ChronoDuration::days(1))
    } else {
        Some(candidate)
    }
}

fn parse_month_day(text: &str) -> Option<(u32, u32)> {
    let re = regex_month_day();
    let caps = re.captures(text)?;
    Some((
        caps.get(1)?.as_str().parse().ok()?,
        caps.get(2)?.as_str().parse().ok()?,
    ))
}

/// 以北京时间（UTC+8）构造时间。
pub fn build_cst(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> Option<DateTime<chrono::FixedOffset>> {
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let naive = date.and_hms_opt(hour, minute, 0)?;
    cst_offset().from_local_datetime(&naive).single()
}

/// 当前北京时间。
pub fn cst_now() -> DateTime<chrono::FixedOffset> {
    chrono::Utc::now().with_timezone(&cst_offset())
}

fn regex_first_number() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"-?\d+(?:\.\d+)?").expect("静态正则合法"))
}

fn regex_first_int() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"-?\d+").expect("静态正则合法"))
}

fn regex_hhmm() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{1,2}):(\d{2})$").expect("静态正则合法"))
}

fn regex_month_day() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2})月(\d{1,2})日").expect("静态正则合法"))
}

fn regex_day_heading() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{1,2})日(?:[（(]([^）)]*)[）)])?").expect("静态正则合法"))
}

fn regex_time_bucket() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2})日(\d{1,2})时").expect("静态正则合法"))
}

fn regex_publisher() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^([^，。：:；\s]{2,30}?(?:气象台|气象局|预警中心|应急管理局))")
            .expect("静态正则合法")
    })
}

trait DateParts {
    fn year_of(&self) -> i32;
    fn month_of(&self) -> u32;
    fn day_of(&self) -> u32;
}

impl DateParts for NaiveDate {
    fn year_of(&self) -> i32 {
        use chrono::Datelike;
        self.year()
    }
    fn month_of(&self) -> u32 {
        use chrono::Datelike;
        self.month()
    }
    fn day_of(&self) -> u32 {
        use chrono::Datelike;
        self.day()
    }
}

impl DateParts for DateTime<chrono::FixedOffset> {
    fn year_of(&self) -> i32 {
        use chrono::Datelike;
        self.year()
    }
    fn month_of(&self) -> u32 {
        use chrono::Datelike;
        self.month()
    }
    fn day_of(&self) -> u32 {
        use chrono::Datelike;
        self.day()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_with_nested_objects() {
        let src = r#"var dataSK={"a":{"b":1},"c":"}{","d":[1,2]};var x=1;"#;
        let extracted = extract_json_object(src, "dataSK").unwrap();
        assert_eq!(extracted, r#"{"a":{"b":1},"c":"}{","d":[1,2]}"#);
        let value: Value = serde_json::from_str(extracted).unwrap();
        assert_eq!(value["c"], "}{");
    }

    #[test]
    fn parses_current_payload() {
        let body = r#"var dataSK={"cityname":"北京","city":"101010100","temp":"9.5","WD":"北风","WS":"2级","wse":"8km/h","SD":"24%","qy":"1021","njd":"30km","time":"22:40","rain":"0","rain24h":"0","aqi":"16","weather":"晴","date":"11月05日(星期四)"}"#;
        let payload = parse_current_js(body).unwrap();
        assert_eq!(payload.city_name.as_deref(), Some("北京"));
        assert_eq!(payload.temperature_c, Some(9.5));
        assert_eq!(payload.humidity_percent, Some(24.0));
        assert_eq!(payload.wind_speed_kmh, Some(8.0));
        assert_eq!(payload.visibility_km, Some(30.0));
        assert_eq!(payload.aqi, Some(16));
        assert_eq!(payload.weather.as_deref(), Some("晴"));
        assert!(payload.observed_at.is_some());
    }

    #[test]
    fn treats_sentinel_temperatures_as_missing() {
        let body = r#"var dataSK={"temp":"999","SD":"","weather":"晴"}"#;
        let payload = parse_current_js(body).unwrap();
        assert_eq!(payload.temperature_c, None);
        assert_eq!(payload.humidity_percent, None);
    }

    #[test]
    fn splits_weather_text() {
        assert_eq!(
            split_weather(Some("晴转多云")),
            (Some("晴".into()), Some("多云".into()))
        );
        assert_eq!(split_weather(Some("晴")), (Some("晴".into()), None));
        assert_eq!(
            split_weather(Some("小雨转雨夹雪")),
            (Some("小雨".into()), Some("雨夹雪".into()))
        );
    }

    #[test]
    fn parses_empty_alarm_list() {
        let body = r#"var alarmDZ101010100 ={"w":[]}"#;
        assert!(parse_alerts_js(body).unwrap().is_empty());
    }

    #[test]
    fn parses_alarm_entries() {
        let body = r#"var alarmDZ101060101={"w":[{"w1":"吉林省","w2":"","w3":"","w4":"05","w5":"大风","w6":"01","w7":"蓝色","w8":"2026-11-05 15:01","w9":"吉林省气象台2026年11月05日14时55分发布大风蓝色预警：请注意防范。（预警信息来源：国家预警信息发布中心）","w11":"10106-20261105150120-0501.html","w13":"吉林省发布大风蓝色预警","w15":"2026-11-06 14:55:00"}]}"#;
        let alerts = parse_alerts_js(body).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].alert_type.as_deref(), Some("大风"));
        assert_eq!(alerts[0].level.as_deref(), Some("蓝色"));
        assert_eq!(alerts[0].publisher.as_deref(), Some("吉林省气象台"));
        assert!(alerts[0].published_at.is_some());
        assert_eq!(alerts[0].region, "吉林省");
    }

    #[test]
    fn parses_flexible_datetimes() {
        assert!(parse_datetime_flexible("2026-11-05 18:00").is_some());
        assert!(parse_datetime_flexible("2026110520").is_some());
        assert!(parse_datetime_flexible("2026-11-05 20:00:00.0").is_some());
        assert!(parse_datetime_flexible("").is_none());
    }

    #[test]
    fn finds_next_date_with_day() {
        let start = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        assert_eq!(
            next_date_with_day(start, 6),
            Some(NaiveDate::from_ymd_opt(2026, 11, 6).unwrap())
        );
        assert_eq!(
            next_date_with_day(NaiveDate::from_ymd_opt(2026, 12, 30).unwrap(), 2),
            Some(NaiveDate::from_ymd_opt(2027, 1, 2).unwrap())
        );
    }

    // -----------------------------------------------------------------------
    // 预警结构异常：只有明确返回空数组才算「没有预警」
    // -----------------------------------------------------------------------

    fn assert_alert_parse_error(body: &str, expected_fragment: &str) {
        let err = parse_alerts_js(body).expect_err("必须报解析错误，而不是静默当成无预警");
        assert_eq!(err.code(), "parse_error", "{err}");
        assert!(
            err.to_string().contains(expected_fragment),
            "错误信息应说明原因（期望包含「{expected_fragment}」）：{err}"
        );
    }

    #[test]
    fn alert_null_w_is_an_error_not_empty() {
        assert_alert_parse_error(r#"var alarmDZ101010100 ={"w":null}"#, "null");
    }

    #[test]
    fn alert_object_w_is_an_error() {
        assert_alert_parse_error(r#"var alarmDZ101010100 ={"w":{}}"#, "对象");
    }

    #[test]
    fn alert_string_w_is_an_error() {
        assert_alert_parse_error(r#"var alarmDZ101010100 ={"w":""}"#, "字符串");
    }

    #[test]
    fn alert_missing_w_is_an_error() {
        assert_alert_parse_error(
            r#"var alarmDZ101010100 ={"city":"101010100"}"#,
            "缺少 w 字段",
        );
    }

    #[test]
    fn alert_top_level_array_is_an_error() {
        // `alarmDZ` 后面直接跟数组：结构已经不对，不能当成「没有预警」。
        let err = parse_alerts_js(r#"var alarmDZ101010100 =[1,2,3]"#).expect_err("应报错");
        assert_eq!(err.code(), "parse_error");
    }

    #[test]
    fn alert_broken_item_is_an_error() {
        assert_alert_parse_error(r#"var alarmDZ101010100 ={"w":["坏了"]}"#, "w[0]");
    }

    #[test]
    fn alert_truncated_json_is_an_error() {
        let err =
            parse_alerts_js(r#"var alarmDZ101010100 ={"w":[{"w1":"吉"#).expect_err("截断应报错");
        assert_eq!(err.code(), "parse_error");
    }

    #[test]
    fn alert_html_response_is_an_error() {
        assert_alert_parse_error("<html><body>403 Forbidden</body></html>", "alarmDZ");
    }

    /// 单条预警字段残缺时不能丢，标题要能合成出来。
    #[test]
    fn alert_without_title_is_kept_with_synthesized_title() {
        let body = r#"var alarmDZ101060101={"w":[{"w1":"吉林省","w2":"长春市","w5":"大风","w7":"蓝色","w9":""}]}"#;
        let alerts = parse_alerts_js(body).expect("字段残缺但结构合法的预警必须保留");
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "吉林省长春市发布大风蓝色预警");
        assert_eq!(alerts[0].region, "吉林省长春市");
        assert_eq!(alerts[0].content, "");
    }

    #[test]
    fn alert_without_any_identifying_field_is_still_kept() {
        let alerts =
            parse_alerts_js(r#"var alarmDZ101010100 ={"w":[{}]}"#).expect("空对象也算一条");
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].title.contains("未命名"), "{}", alerts[0].title);
    }

    // -----------------------------------------------------------------------
    // 缓存值不含城市上下文：绑定发生在每次调用
    // -----------------------------------------------------------------------

    fn city(note: Option<&str>) -> CityInfo {
        CityInfo {
            code: "101280601".into(),
            name: "深圳".into(),
            province: Some("广东省".into()),
            city: Some("深圳市".into()),
            district: None,
            display_name: "广东省深圳市".into(),
            level: Some(crate::models::CityLevel::City),
            resolution_note: note.map(str::to_string),
        }
    }

    fn fetched_at() -> DateTime<FixedOffset> {
        build_cst(2026, 11, 6, 9, 0).expect("合法时间")
    }

    const HOURLY_HTML: &str = r#"<html><body>
<input type="hidden" id="fc_3h_internal_update_time" value="2026-11-06 08:00"/>
<script>var hour3data={"1d":["06日11时,d00,晴,12℃,西风,<3级,1","06日14时,d00,晴,13℃,东南风,<3级,1","06日17时,d00,晴,13℃,东南风,<3级,1","06日20时,n00,晴,6℃,东南风,<3级,0","06日23时,n01,多云,4℃,东风,<3级,0","07日02时,n01,多云,4℃,东北风,<3级,0","07日05时,n01,多云,2℃,东北风,<3级,0","07日08时,d01,多云,3℃,东北风,<3级,2"]};</script>
</body></html>"#;

    #[test]
    fn observation_payload_has_no_city_context() {
        let body = r#"var dataSK={"cityname":"深圳","temp":"22","weather":"多云","time":"09:00","date":"11月06日(星期五)"}"#;
        let payload = parse_current_js(body).expect("解析成功");

        let fallback = payload.clone().into_observation(
            city(Some("「粤海街道」没有独立站点")),
            "u".to_string(),
            fetched_at(),
        );
        let plain = payload.into_observation(city(None), "u".to_string(), fetched_at());

        assert_eq!(
            fallback.city.resolution_note.as_deref(),
            Some("「粤海街道」没有独立站点")
        );
        assert!(
            plain.city.resolution_note.is_none(),
            "同一份数据绑定不同请求时必须各自独立"
        );
        assert_eq!(
            fallback.source.fetched_at,
            fetched_at(),
            "抓取时间必须来自真实抓取"
        );
    }

    #[test]
    fn forecast_payload_binds_city_per_call() {
        let html = r#"<html><body>
<input type="hidden" id="fc_24h_internal_update_time" value="2026-11-06 07:30"/>
<ul class="t">
<li><h1>6日（今天）</h1><p class="wea">晴</p><p class="tem"><span>13</span><i>5</i></p></li>
</ul></body></html>"#;
        let payload = parse_forecast_page(html).expect("解析成功");
        let a = payload
            .clone()
            .into_bundle(city(Some("回退说明")), "u".into(), fetched_at());
        let b = payload.into_bundle(city(None), "u".into(), fetched_at());
        assert!(a.city.resolution_note.is_some());
        assert!(b.city.resolution_note.is_none());
        assert_eq!(a.available_days, 1);
        assert_eq!(a.published_at, build_cst(2026, 11, 6, 7, 30));
    }

    #[test]
    fn hourly_payload_binds_city_and_reports_full_span() {
        let payload = parse_hourly_page(HOURLY_HTML).expect("解析成功");
        assert_eq!(payload.points.len(), 8);
        let bundle = payload.into_bundle(city(Some("回退说明")), "u".into(), fetched_at());
        assert_eq!(bundle.city.resolution_note.as_deref(), Some("回退说明"));
        assert_eq!(bundle.resolution_hours, 3);
        // 未按请求窗口裁剪：requested_hours 为 0，covered_hours 是上游总跨度。
        assert_eq!(bundle.requested_hours, 0);
        assert_eq!(bundle.covered_hours, 24, "06日11时 → 07日08时 + 3 小时");
        assert_eq!(bundle.reference_time, None);
    }

    /// 线上多日字段叫 `23d`，`1d` 不足 8 个点时必须能回落到它。
    #[test]
    fn hourly_falls_back_to_23d_when_1d_is_short() {
        let html = r#"<html><body>
<input type="hidden" id="fc_3h_internal_update_time" value="2026-11-06 08:00"/>
<script>var hour3data={"1d":["06日11时,d00,晴,12℃,西风,<3级,1"],"23d":[["06日14时,d00,晴,13℃,东南风,<3级,1","06日17时,d00,晴,13℃,东南风,<3级,1"]]};</script>
</body></html>"#;
        let payload = parse_hourly_page(html).expect("解析成功");
        assert_eq!(payload.points.len(), 3, "1d 的 1 个点 + 23d 的 2 个点");
        let times: Vec<String> = payload
            .points
            .iter()
            .map(|point| point.time.format("%d %H:%M").to_string())
            .collect();
        assert_eq!(times, vec!["06 11:00", "06 14:00", "06 17:00"]);
    }

    #[test]
    fn alert_payload_binds_city_per_call() {
        let body = r#"var alarmDZ101060101={"w":[{"w1":"吉林省","w5":"大风","w7":"蓝色","w8":"2026-11-05 15:01","w9":"吉林省气象台发布大风蓝色预警","w13":"吉林省发布大风蓝色预警"}]}"#;
        let payload = parse_alerts_payload(body).expect("解析成功");
        let a = payload
            .clone()
            .into_bundle(city(Some("回退说明")), "u".into(), fetched_at());
        let b = payload.into_bundle(city(None), "u".into(), fetched_at());
        assert!(a.city.resolution_note.is_some());
        assert!(b.city.resolution_note.is_none());
        assert!(a.has_alerts && b.has_alerts);
        assert_eq!(a.status, "active_alerts_found");
    }

    #[test]
    fn empty_alert_payload_reports_no_alerts_explicitly() {
        let payload = parse_alerts_payload(r#"var alarmDZ101010100 ={"w":[]}"#).expect("解析成功");
        let bundle = payload.into_bundle(city(None), "u".into(), fetched_at());
        assert!(!bundle.has_alerts);
        assert!(bundle.alerts.is_empty());
        assert_eq!(bundle.status, "no_active_alerts");
    }
}
