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
        // 24 小时降水与 AQI 同样受物理量程约束（见 [`RANGE_PRECIPITATION_MM`] /
        // [`RANGE_AQI`]）。被量程拒绝的值会变成 `None`，既不出现在 JSON 里，也必须
        // 在这里显式列出 —— 否则调用方无法区分「上游没给」与「给了但被量程拒绝」。
        track!(self.precipitation_24h_mm, "precipitation_24h_mm");
        track!(self.pressure_hpa, "pressure_hpa");
        track!(self.visibility_km, "visibility_km");
        track!(self.aqi, "aqi");
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

    /// 是否至少命中一个核心气象字段。
    ///
    /// 这是**实况解析的最低有效性门槛**：上游返回 `var dataSK={};`、
    /// `{"cityname":"北京"}` 这类空壳时，字段全部为 `None`，若照样返回成功，
    /// 调用方就会以为「查到了天气」。观测时间不参与判定 —— 时间缺失只影响
    /// `observed_at_note`，不影响「有没有气象数据」这个更基本的事实。
    fn has_any_measurement(&self) -> bool {
        self.temperature_c.is_some()
            || self.weather.is_some()
            || self.humidity_percent.is_some()
            || self.wind_direction.is_some()
            || self.wind_scale.is_some()
            || self.wind_speed_kmh.is_some()
            || self.precipitation_mm.is_some()
            || self.precipitation_24h_mm.is_some()
            || self.pressure_hpa.is_some()
            || self.visibility_km.is_some()
            || self.aqi.is_some()
    }
}

/// 解析 `var dataSK={...}`。
///
/// 解析失败（结构不对、或一条气象字段都没有）一律返回 [`WeatherError::Parse`]，
/// 绝不返回一份「字段全空但调用成功」的观测：门槛必须设在解析边界，因为那里
/// 的错误才会被缓存层识别为抓取失败，而不是被当成成功值缓存下来。
pub fn parse_current_js(body: &str) -> Result<ObservationPayload> {
    let raw = match extract_json_object(body, "dataSK") {
        Some(raw) => raw,
        None => {
            // 区分「根本没有 dataSK」与「dataSK 是 null / 数组 / 数字」：
            // 后者能截到标记但没有对象文本，属于结构异常，不是接口改版。
            let message = if body.contains("dataSK") {
                "实况接口的 dataSK 不是 JSON 对象（上游返回了 null、数组或其它类型，无法读出任何气象字段）"
            } else {
                "实况接口未返回 dataSK 数据块（接口结构可能已变化）"
            };
            return Err(WeatherError::Parse(message.into()));
        }
    };
    let value: Value = serde_json::from_str(raw)?;
    // 解析结果不是对象时立刻报错，不再按字段读取 —— 否则会得到一份「字段全空」的
    // 载荷，看起来和「成功但没有数据」完全一样。
    if !value.is_object() {
        return Err(WeatherError::Parse(format!(
            "实况数据结构异常：dataSK 应为 JSON 对象，实际是 {}",
            json_type_name(&value)
        )));
    }

    let observed_time = json_text(&value, "time");
    let observed_date = json_text(&value, "date");
    let (observed_at, note) = match (observed_time.as_deref(), observed_date.as_deref()) {
        (Some(time), Some(date)) => {
            let observed_at = observed_datetime(time, date);
            // D10：`observed_at` 为 `None` 时 note **不能**继续宣称「已按北京时间解析」。
            // 日期与时间文本都规整、只是该日期在可推断的年份里不存在（非闰年的
            // 02月29日、任何年份都没有的 02月30日 / 04月31日）时，必须如实说明
            // 「时间无法确定」，而不是留下一句把人骗过去的说明。
            let note = if observed_at.is_some() {
                format!(
                    "数据源只给出「{date} {time}」，按北京时间（UTC+8）解析，年份由抓取日期推断"
                )
            } else if parse_hhmm(time).is_some() && parse_month_day(date).is_some() {
                format!(
                    "数据源给出了「{date} {time}」，但{date}在可推断的年份里不存在（例如非闰年的 02月29日），观测时间无法确定"
                )
            } else {
                format!("数据源给出的「{date} {time}」无法解析成具体时刻，观测时间无法确定")
            };
            (observed_at, Some(note))
        }
        (Some(time), None) => (
            observed_datetime_from_now(time),
            Some(format!(
                "数据源只给出「{time}」，按北京时间（UTC+8）就近推断日期"
            )),
        ),
        _ => (None, Some("数据源未提供观测时间".to_string())),
    };

    let payload = ObservationPayload {
        city_name: json_text(&value, "cityname"),
        temperature_c: json_number_in(&value, "temp", RANGE_TEMPERATURE_C),
        weather: json_text(&value, "weather"),
        humidity_percent: json_number_in(&value, "SD", RANGE_HUMIDITY_PERCENT),
        wind_direction: json_text(&value, "WD"),
        wind_scale: json_text(&value, "WS"),
        wind_speed_kmh: json_unit_number_in(&value, "wse", RANGE_WIND_SPEED_KMH),
        precipitation_mm: json_number_in(&value, "rain", RANGE_PRECIPITATION_MM),
        precipitation_24h_mm: json_number_in(&value, "rain24h", RANGE_PRECIPITATION_MM),
        pressure_hpa: json_number_in(&value, "qy", RANGE_PRESSURE_HPA),
        visibility_km: json_unit_number_in(&value, "njd", RANGE_VISIBILITY_KM),
        aqi: json_u32(&value, "aqi"),
        observed_at,
        observed_at_raw: match (observed_date, observed_time) {
            (Some(date), Some(time)) => Some(format!("{date} {time}")),
            (None, Some(time)) => Some(time),
            (Some(date), None) => Some(date),
            (None, None) => None,
        },
        observed_at_note: note,
    };

    // 最低有效性门槛：至少命中一个核心气象字段才算「查到了实况」。`var dataSK={};`、
    // `{"cityname":"北京"}`、`{"temp":"N/A"}` 都走这条分支，不会被伪装成成功。
    if !payload.has_any_measurement() {
        return Err(WeatherError::Parse(
            "实况接口返回的 dataSK 不含任何气象字段（上游返回了空数据，不能当成查到了天气）".into(),
        ));
    }

    Ok(payload)
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
    // 只取一次「现在」：发布时间回滚与「没有发布时间时的基准日期」必须基于同一个
    // 时刻，否则春节零点前后这两处会互相矛盾。
    let now = cst_now();
    let published_at = publish_datetime(fc_24h.as_deref(), update_time.as_deref(), now);
    let base_date = published_at
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| now.date_naive());
    let published_raw = describe_publish(fc_24h.as_deref(), update_time.as_deref());

    let mut days: Vec<DailyForecast> = Vec::new();
    // 游标只由**被接受**的日期推进。被跳过的条目绝不能污染它，否则后面的条目会
    // 跟着一起错位。
    let mut last_date: Option<NaiveDate> = None;
    let mut saw_item = false;
    for item in document.select(&item_selector) {
        saw_item = true;
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

        // `今天` / `明天` / `后天` 只是**期望日期**：标题里的日号必须与它一致。
        // 不一致说明标题本身已经坏了，此时不能把期望日期硬塞进去（那等于替上游
        // 编造一个日期），只能整条跳过。
        let labeled = match label.as_deref() {
            Some("今天") => Some(base_date),
            Some("明天") => base_date.checked_add_signed(ChronoDuration::days(1)),
            Some("后天") => base_date.checked_add_signed(ChronoDuration::days(2)),
            _ => None,
        };
        let date = match labeled {
            Some(expected) if expected.day_of() == day_of_month => expected,
            Some(_) => continue,
            None => {
                // 无标签的条目按严格解析器推断，锚点取上一条被接受的日期。
                let anchor = last_date.unwrap_or(base_date);
                match resolve_forecast_date(anchor, day_of_month) {
                    Some(date) => date,
                    None => continue,
                }
            }
        };
        // 重复或逆序：跳过该条目，且不推进游标。
        if last_date.is_some_and(|last| date <= last) {
            continue;
        }
        last_date = Some(date);

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
        return Err(WeatherError::Parse(if saw_item {
            "7 日预报条目的日期全部无效（上游可能返回了损坏的日期标题）".into()
        } else {
            "7 日预报页面未解析到任何 `ul.t > li` 条目（页面结构可能已变化）".into()
        }));
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
    /// 的总跨度」，方便调用方了解数据量；`contiguous_hours` 与 `coverage_end`
    /// 只有在按窗口裁剪之后才有意义，因此这里是 `None`。
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
            contiguous_hours: None,
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

/// 逐 3 小时时点的存储上限（内存控制）。
///
/// 32 个时点 = 96 小时 = 4 天，远大于 MCP 层允许请求的 24 小时窗口，也覆盖了
/// `1d` + `23d` 能给出的全部有效范围；再往后只会在缓存里白占内存。
pub const MAX_HOURLY_POINTS: usize = 32;

/// `hour3data` 里逐时数据的字段优先级：**越靠前越权威**。
///
/// 实测（2026-11）线上布局：
///
/// | 字段 | 覆盖范围 | 布局 | 说明 |
/// | --- | --- | --- | --- |
/// | `1d` | 未来 24 小时 | `["06日11时,...", …]` 8 个元素 | 刷新最频繁，第 1 天以它为准 |
/// | `23d` | 第 2~3 天 | `[["07日08时,...", …], …]` | 第 2~3 天以它为准 |
/// | `7d` | 未来 7 天 | `[["06日11时,...", …] × 7]` | 粒度最粗，只用来补齐前两者缺失的时点 |
///
/// 同一天的时点会同时出现在多个字段里，取值以优先级高者为准。
pub const HOURLY_SOURCE_PRIORITY: [&str; 3] = ["1d", "23d", "7d"];

/// 解析 1 日页面内联的 `hour3data`。
pub fn parse_hourly_html(html: &str, city: CityInfo, url: String) -> Result<HourlyBundle> {
    Ok(parse_hourly_page(html)?.into_bundle(city, url, cst_now()))
}

/// 只解析页面内容，不绑定任何请求上下文。
///
/// # 为什么要合并 `1d` / `23d` / `7d`
///
/// `1d` 的 8 个时点只保证覆盖「从**首个预报时点**起的 24 小时」，并不保证覆盖
/// 「从**用户调用时刻**起的 24 小时」。例如 2026-11-06 的北京页面：`1d` 从
/// `06日11时` 到 `07日08时`（区间到 `07日11时`），而用户在 `06日14时` 请求未来
/// 24 小时，截止时刻是 `07日14时` —— 此时 `23d` 里的 `07日11时`、`07日14时`
/// 正是窗口缺的那一段。旧实现「`1d` 满 8 个点就跳过 `23d`」会把这段凭空截掉。
///
/// 因此这里**无条件**解析三个字段并按优先级归并，时效性与窗口长度都不再依赖
/// 「`1d` 恰好少于 8 个点」这种偶然条件。解析器只负责产出可信的时间序列，
/// 「从什么时候开始截、截多长」仍然由 [`crate::window::select`] 按本次调用时刻决定。
pub fn parse_hourly_page(html: &str) -> Result<HourlyPayload> {
    let document = Html::parse_document(html);
    let publish_raw = hidden_input(&document, "fc_3h_internal_update_time")
        .or_else(|| hidden_input(&document, "fc_24h_internal_update_time"));
    let published_at = publish_raw.as_deref().and_then(parse_datetime_flexible);

    let raw = extract_json_object(html, "hour3data").ok_or_else(|| {
        WeatherError::Parse("1 日页面未包含 hour3data 数据块（数据源可能已调整）".into())
    })?;
    let value: Value = serde_json::from_str(raw)?;

    // 「日 + 时」只有日号，月/年必须由基准时刻推断；基准优先取页面自己的发布时间，
    // 页面没给（或改版）时退化为本次抓取时刻。
    let base = published_at.unwrap_or_else(cst_now);

    let mut candidates: Vec<(usize, HourlyForecast)> = Vec::new();
    for (priority, key) in HOURLY_SOURCE_PRIORITY.iter().enumerate() {
        collect_hour_points(value.get(*key), base, priority, &mut candidates);
    }

    if candidates.is_empty() {
        return Err(WeatherError::Parse(
            "hour3data 中没有可解析的时点（数据源可能已调整结构）".into(),
        ));
    }

    // 归并：时间升序 → 同一时刻**带气象数据者优先** → 字段优先级（`1d` 在前）。
    // 第二个排序键不是可有可无的：上游会用 `"06日11时,,,,,"` 这种「只有时间戳、
    // 没有任何气象信息」的记录占位，若只按来源优先级排序，`1d` 的空记录会把
    // `23d` 的完整记录整条挤掉。`sort_by` 是稳定排序，同一字段内部的原始顺序
    // 也会保留 —— 输出与任何哈希容器的迭代顺序无关。
    candidates.sort_by(|(a_priority, a), (b_priority, b)| {
        a.time
            .cmp(&b.time)
            .then_with(|| hour_point_has_data(b).cmp(&hour_point_has_data(a)))
            .then(a_priority.cmp(b_priority))
    });
    // `dedup_by` 保留每组重复中的**第一个**：先有效、再按来源优先级。旧预报
    // 永远不会覆盖更可信的新预报，空记录也永远不会覆盖有效记录。
    candidates.dedup_by(|(_, a), (_, b)| a.time == b.time);

    // 去重后仍然一条气象信息都没有的时点整条丢弃。这会在时间序列里留下空洞 ——
    // 空洞是诚实的，补点或填 0/""/0.0 才是撒谎。过滤必须发生在截断之前，否则
    // 空记录会把近期的有效时点挤出 `MAX_HOURLY_POINTS`。
    let mut points: Vec<HourlyForecast> = candidates
        .into_iter()
        .filter(|(_, point)| hour_point_has_data(point))
        .map(|(_, point)| point)
        .collect();

    if points.is_empty() {
        return Err(WeatherError::Parse(
            "hour3data 中没有任何携带气象信息的时点（上游返回了空记录，不能当成有效预报）".into(),
        ));
    }

    // 按时间升序截断，因此超限时丢掉的永远是最远的时点，近期数据不受影响。
    points.truncate(MAX_HOURLY_POINTS);

    Ok(HourlyPayload {
        published_at,
        published_at_raw: publish_raw,
        points,
    })
}

/// 把一个字段里的所有时点追加进 `out`。
///
/// 兼容两种既见布局：`["06日11时,...", …]`（`1d`）与
/// `[["07日08时,...", …], …]`（`23d` / `7d`）。结构未知的条目直接跳过 ——
/// 宁可少一个时点，也不去猜它的含义。
fn collect_hour_points(
    node: Option<&Value>,
    base: DateTime<chrono::FixedOffset>,
    priority: usize,
    out: &mut Vec<(usize, HourlyForecast)>,
) {
    let Some(entries) = node.and_then(Value::as_array) else {
        return;
    };
    for entry in entries {
        match entry {
            Value::String(text) => {
                if let Some(point) = parse_hour_point(Some(text), base) {
                    out.push((priority, point));
                }
            }
            Value::Array(day) => {
                for item in day {
                    if let Some(point) = parse_hour_point(item.as_str(), base) {
                        out.push((priority, point));
                    }
                }
            }
            _ => {}
        }
    }
}

/// 一条时点记录是否携带**真实气象信息**。
///
/// 计入的只有 `weather` / `temperature_c` / `wind_direction` / `wind_scale` 四项。
/// `icon_code` **不算**：图标代码只是渲染用的编号，不描述天气，把它当成观测内容
/// 会让 `"06日11时,d00,,,,"` 这种空记录冒充有效数据。时间戳同样不算 —— 时间戳
/// 有效与气象数据有效是两件事。
fn hour_point_has_data(point: &HourlyForecast) -> bool {
    point.weather.is_some()
        || point.temperature_c.is_some()
        || point.wind_direction.is_some()
        || point.wind_scale.is_some()
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

    let time = resolve_hour_date(base, day, hour)?;

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

        // 读单条预警里的一个字段。**同时接受字符串与数字**（D4）：
        // `w1/w2/w3` 是区域、`w4/w5/w6/w7` 是代码、`w8/w15` 是时间，上游偶尔会把
        // 其中一部分写成 JSON 数字。旧实现只认 `as_str`，遇到数字时所有字段都读成
        // `None`，于是 `content` 为空、标题被兜底成「未命名气象预警…」，最终仍然
        // `alerts.push(...)`、`has_alerts=true` —— 凭空造出一条幽灵预警。
        // 其它类型（null / 数组 / 对象 / 布尔）仍然返回 `None`，不猜语义。
        let text = |key: &str| -> Option<String> {
            let value = match item.get(key)? {
                Value::String(text) => text.trim().to_string(),
                Value::Number(number) => number.to_string(),
                _ => return None,
            };
            if value.is_empty() { None } else { Some(value) }
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

/// marker 与 JSON 起始 `{` 之间允许的最大字节距离（D2）。
///
/// 真实页面里这一段只有 `=` 与空格（2 字节），64 字节已经非常宽松。设成硬上限是为了
/// 让「JSON 块缺失」这种情况**尽早失败**：旧实现在找到 marker 之后无上限地向后线性
/// 搜索第一个 `{`，当数据块被截断时它会一路扫到文档末尾，把页面里毫不相关的 `{`
/// （内联 CSS、别的 `<script>`）当成 JSON 起点 —— 审计在真实大页面里实测到 marker
/// 之前已有 44 个 CSS 下的 `{`。
const MAX_JSON_LEAD: usize = 64;

/// 从 `marker` 之后的第一个 `{` 开始做花括号配平，返回完整的 JSON 文本。
///
/// 这些接口把 JSON 内联在 `<script>` 里，直接正则截取会在嵌套 `{}` 或字符串里的
/// 花括号上出错，因此按字符扫描并跳过字符串内容。
///
/// # marker 必须落在标识符边界上（D2）
///
/// 旧实现用裸的 `source.find(marker)`，于是 `hour3data` 也会命中 `myhour3data`、
/// `hour3data_old` 这类更长的标识符。现在要求匹配位置**前一个字符**不能是 ASCII
/// 字母 / 数字 / `_` / `$`；不满足就继续往后找下一个匹配，直到找到合法匹配或耗尽。
/// 边界判断按**字符**（`chars().next_back()`）而不是按字节，避免在 UTF-8 多字节
/// 字符中间切片。
///
/// # marker 与 `{` 之间只能是「标识符续接字符 + 空白 + 可选 `=`」
///
/// 距离上限（[`MAX_JSON_LEAD`]）之外还有一条内容白名单，见下面的 gap 语法说明。
/// 两条合起来才能挡住「数据块缺失、但下游另一个无关对象的 `{` 恰好在 64 字节以内」
/// 的情况 —— 那种 `{` 属于别的对象，绝不能当成本数据块的起点。
pub fn extract_json_object<'a>(source: &'a str, marker: &str) -> Option<&'a str> {
    let bytes = source.as_bytes();
    let marker_at = find_marker_at_identifier_boundary(source, marker)?;

    // 从 marker 末尾起，最多向前扫 [`MAX_JSON_LEAD`] 字节。这段 gap 的语法是
    // `[标识符续接字符]* [空白]* [=]? [空白]*`，然后必须是 `{`：
    //
    // * 标识符续接字符：真实形态是 `var alarmDZ101060101 ={"w":...}` —— `alarmDZ`
    //   后面紧跟城市代码，它们同属一个标识符；
    // * `=` 与空白：`var dataSK={`、`var hour3data = {`。
    //
    // 这条白名单是必须的：光靠距离上限挡不住 `var hour3data=;var cfg={"debug":1};`
    // 这种「数据块缺失、但下游另一个配置对象的 `{` 恰好在 64 字节以内」的情况 ——
    // 那个 `{` 属于完全不相干的对象。`;` 与 `var cfg` 不合法，因此这里返回 `None`。
    // `{` 是 ASCII，按字节扫不会切错字符。
    let search_from = marker_at + marker.len();
    let limit = search_from.saturating_add(MAX_JSON_LEAD).min(bytes.len());
    let mut index = search_from;
    while index < limit && is_identifier_byte(bytes[index]) {
        index += 1;
    }
    while index < limit && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if index < limit && bytes[index] == b'=' {
        index += 1;
        while index < limit && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
    }
    if index >= limit || bytes[index] != b'{' {
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

/// ASCII 标识符续接字符（字母 / 数字 / `_` / `$`）。
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// 找 `marker` 的第一个「标识符边界上」的出现位置（D2）。
///
/// 前一个字符是 ASCII 字母 / 数字 / `_` / `$` 时说明它是更长标识符的一部分，跳过并
/// 继续往后找。返回的字节下标一定落在 `char` 边界上（`str::find` 的匹配位置必然如此）。
fn find_marker_at_identifier_boundary(source: &str, marker: &str) -> Option<usize> {
    let mut from = 0usize;
    loop {
        // `source.get` 而不是 `&source[from..]`：万一 `from` 落在字符中间也不能 panic。
        let at = from + source.get(from..)?.find(marker)?;
        let previous_ok = source
            .get(..at)
            .and_then(|prefix| prefix.chars().next_back())
            .is_none_or(|previous| {
                !(previous.is_ascii_alphanumeric() || matches!(previous, '_' | '$'))
            });
        if previous_ok {
            return Some(at);
        }
        // 前进一个字符（不是 1 字节）：保证下一轮的下标仍在 char 边界上。
        // `marker` 为空时 `chars().next()` 为 `None`，这里直接收工，避免死循环。
        let next_char_len = source.get(at..)?.chars().next().map(char::len_utf8)?;
        from = at + next_char_len;
    }
}

/// 实况字段的物理量程（闭区间，含端点），**依据见每个常量上方的注释**。
///
/// 这些量程是「异常输入 → 成功」的主闸门：`{"temp":"99999","SD":"250","qy":"1",
/// "njd":"-5"}` 这类明显不可能出现的数据，在读取字段时就被判为**缺失**（`None`），
/// 从而自然落进 `missing_fields`，绝不会被当成一次成功的观测返回出去。
///
/// | 字段 | 键 | 区间 | 依据 |
/// | --- | --- | --- | --- |
/// | 气温 ℃ | `temp` | `-90.0 ..= 60.0` | 地球观测极值约 -89.2 ℃ / 56.7 ℃，再留余量 |
/// | 湿度 % | `SD` | `0.0 ..= 100.0` | 相对湿度的定义域 |
/// | 气压 hPa | `qy` | `300.0 ..= 1100.0` | 地面气压极值约 870~1085 hPa，取更宽松的整数边界 |
/// | 能见度 km | `njd` | `0.0 ..= 100.0` | 站点上报的能见度上限普遍不超过 100 km |
/// | 风速 km/h | `wse` | `0.0 ..= 500.0` | 地面阵风观测记录约 400 km/h |
/// | 1h 降水 mm | `rain` | `0.0 ..= 3000.0` | 1 小时世界纪录约 305 mm，取一个数量级余量 |
/// | 24h 降水 mm | `rain24h` | `0.0 ..= 3000.0` | 24 小时世界纪录约 1825 mm |
/// | AQI | `aqi` | `0.0 ..= 1000.0` | 国标上限 500，取两倍余量 |
///
/// 注意 `999 hPa` 落在气压区间**之内**：它是真实存在的强低压值，不是缺失标记
/// （见 [`is_sentinel`] 的注释）。
const RANGE_TEMPERATURE_C: (f64, f64) = (-90.0, 60.0);
const RANGE_HUMIDITY_PERCENT: (f64, f64) = (0.0, 100.0);
const RANGE_PRESSURE_HPA: (f64, f64) = (300.0, 1100.0);
const RANGE_VISIBILITY_KM: (f64, f64) = (0.0, 100.0);
const RANGE_WIND_SPEED_KMH: (f64, f64) = (0.0, 500.0);
const RANGE_PRECIPITATION_MM: (f64, f64) = (0.0, 3000.0);
const RANGE_AQI: (f64, f64) = (0.0, 1000.0);

/// 把数值收进物理量程：非有限值（`NaN` / `±inf`）与超范围值一律变成 `None`。
fn within_range(number: f64, range: (f64, f64)) -> Option<f64> {
    if number.is_finite() && number >= range.0 && number <= range.1 {
        Some(number)
    } else {
        None
    }
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

/// 读一个数值字段（未做量程校验，调用方必须再过一道 [`within_range`]）。
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
    // 数据的唯一非有限来源是上游写了 `NaN` / `inf` 之类；这类值没有任何物理意义，
    // 直接当缺失。有限值交由调用方的量程校验判定。
    // （旧的 `is_sentinel_number` 还额外把 999/-999/9999 当魔法缺失标记，但 999 hPa
    // 是真实气压值，见 [`is_sentinel`] 的说明，因此那部分已被删除。）
    number.is_finite().then_some(number)
}

/// 读一个带物理量程的数值字段（D3）。
fn json_number_in(value: &Value, key: &str, range: (f64, f64)) -> Option<f64> {
    within_range(json_number(value, key)?, range)
}

/// 读一个 AQI 字段（D5）。
///
/// 旧实现用 `number.round().max(0.0) as u32` 做**饱和转换**：`{"aqi":"1e30"}` 会变成
/// `Some(4294967295)`，把一个明显的非法值包装成合法值。现在改成先按 AQI 量程校验
/// （非有限 / 负数 / `> 1000` 一律 `None`），再做转换。
fn json_u32(value: &Value, key: &str) -> Option<u32> {
    let checked = within_range(json_number(value, key)?, RANGE_AQI)?;
    Some(checked.round() as u32)
}

/// 读一个带单位的字段，例如 `12km/h`、`30km`、`<3级`（D7）。
///
/// 只接受「**前导**数值 + 可选的单位后缀」，不再像旧实现那样用未锚定的
/// `-?\d+(?:\.\d+)?` 从任意位置捞第一个数字 —— 那会把 `12-30km` 截成 `12.0`、
/// 把 `1e3` 截成 `1.0`，让畸形文本伪装成合法数值。
fn json_unit_number_in(value: &Value, key: &str, range: (f64, f64)) -> Option<f64> {
    let text = json_text(value, key)?;
    within_range(leading_number(&text)?, range)
}

/// 解析「前导数值 + 单位」：`"3km/h"` → `3.0`、`"9.1"` → `9.1`、`"-5"` → `-5.0`。
///
/// 取到前导数值之后还必须检查它后面紧跟的内容：若以 ASCII 数字、`.`、`e`、`E`、`-`
/// 开头，说明这段文本不是一个「数值 + 单位」（`12-30km` 是区间、`1e3` 是指数记法、
/// `1.2.3` 是畸形小数），此时宁可当缺失也不去猜：
///
/// | 输入 | 结果 | 原因 |
/// | --- | --- | --- |
/// | `3km/h` | `Some(3.0)` | 数值 + 单位 |
/// | `30km` | `Some(30.0)` | 数值 + 单位 |
/// | `9.1` | `Some(9.1)` | 纯数值 |
/// | `12-30km` | `None` | 后面紧跟 `-`，是区间不是单个数值 |
/// | `1e3` | `None` | 后面紧跟 `e`，是指数记法 |
/// | `1.2.3` | `None` | 后面紧跟 `.`，是畸形小数 |
fn leading_number(text: &str) -> Option<f64> {
    let re = regex_leading_number();
    let matched = re.captures(text)?.get(1)?;
    let rest = &text[matched.end()..];
    let ambiguous = rest
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_digit() || matches!(first, '.' | 'e' | 'E' | '-'));
    if ambiguous {
        return None;
    }
    matched
        .as_str()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// 上游的**文本**缺失标记。
///
/// 这里刻意**只保留**「整段文本就是缺失标记」的形式：`暂无`、`无`、`N/A`、`n/a`、
/// `null`、`-`、`--`，以及空串（由调用方单独判空）。
///
/// # 为什么不再把 `999` / `-999` / `9999` 当缺失标记
///
/// 那三个数字曾经被当成「万能缺失标记」，代价是把**真实数据**判成缺失：`999 hPa`
/// 是真实存在的强低压，`{"qy":"999"}` 被读成 `pressure_hpa=None` 属于反向错误。
/// 现在每个数值字段都有自己的物理量程（见 [`RANGE_TEMPERATURE_C`] 等常量），
/// 数值型缺失标记会被量程自然挡掉：
///
/// * `temp=999` 超出 `-90..=60` → `None`；
/// * `SD=999` 超出 `0..=100` → `None`；
/// * `qy=999` 落在 `300..=1100` → `Some(999.0)`（这才是正确结果）。
///
/// 因此不必再靠魔法数字，量程比哨兵更准确、也不会误杀合法数据。
fn is_sentinel(text: &str) -> bool {
    matches!(text, "暂无" | "无" | "N/A" | "n/a" | "null" | "-" | "--")
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

/// 7 日预报相邻条目允许的最大跨越天数（上一条被接受日期 → 本条日期）。
///
/// 真实页面的相邻条目最多差 1 天，7 天预报整体跨度也只有 6 天，因此 7 是一个既
/// 宽松又足够严格的上限。
const MAX_FORECAST_STEP_DAYS: i64 = 7;

/// 把预报标题里的日号还原成完整日期，**只允许两个候选月份**：
/// 锚点所在月，与锚点的下一个月（下个月用 [`shift_month`] 计算，正确处理跨年）。
///
/// # 为什么必须有间隔上限
///
/// 站点只给日号，月/年必须由上下文推断。旧实现对「同月没有这一天」的反应是
/// **向前搜索最多 14 个月**，于是 `31日` 出现在只有 30 天的月份时会命中下下个月的
/// 31 日，凭空造出一份 55 天后的预报。合法的跨月与坏标题的区别只能由间隔长度体现：
///
/// * `anchor = 2026-11-30` + `01日`：同月没有 1 日，下月候选 `2026-12-01`，间隔
///   1 天 —— 这就是真实存在的跨月；
/// * `anchor = 2026-11-06` + `31日`：11 月没有 31 日，下月候选 `2026-12-31`，
///   间隔 55 天 —— 7 天预报里不可能出现，只能判为坏标题。
///
/// 因此「候选月份只有两个」+「间隔不超过 [`MAX_FORECAST_STEP_DAYS`] 天」合起来，
/// 公开给测试（`tests/parser_property_tests.rs` 用随机 anchor / 日号验证这条不变式）。
/// 「只允许两个候选月份」+「间隔不超过 [`MAX_FORECAST_STEP_DAYS`] 天」合起来，
/// 才是既能处理跨月跨年、又不会把坏标题变成看似合理日期的判据。同月候选优先，
/// 同月不存在或不满足约束时才试下月；都不满足返回 `None`（调用方据此跳过条目）。
pub fn resolve_forecast_date(anchor: NaiveDate, day_of_month: u32) -> Option<NaiveDate> {
    // `00日`、`32日` 之类在任何月份都不可能存在。
    if !(1..=31).contains(&day_of_month) {
        return None;
    }
    let same_month = NaiveDate::from_ymd_opt(anchor.year_of(), anchor.month_of(), day_of_month)
        .filter(|candidate| *candidate >= anchor);
    let (next_year, next_month) = shift_month(anchor.year_of(), anchor.month_of(), 1);
    let next_month =
        NaiveDate::from_ymd_opt(next_year, next_month, day_of_month).filter(|c| *c >= anchor);

    [same_month, next_month]
        .into_iter()
        .flatten()
        .find(|candidate| (*candidate - anchor).num_days() <= MAX_FORECAST_STEP_DAYS)
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

/// 发布时间允许比「现在」超前的最大小时数（D9）。
///
/// 页面上的 `update_time` 只可能**略晚于**抓取时刻（页面刷新早于抓取，且上游时钟
/// 也会有一点漂移），绝不可能晚半天以上。晚 12 小时以上只可能是一种情况：抓取发生在
/// 零点之后不久、而 `update_time` 属于前一天的晚间档 —— 此时必须回滚 1 天。
const PUBLISH_CLOCK_SKEW_HOURS: i64 = 12;

/// 组合「页面日期 + update_time」得到预报发布时间。
///
/// `now` 由调用方显式传入（纯函数化），这样「跨零点回滚」不必注入时钟就能测试。
pub fn publish_datetime(
    fc_24h: Option<&str>,
    update_time: Option<&str>,
    now: DateTime<chrono::FixedOffset>,
) -> Option<DateTime<chrono::FixedOffset>> {
    let base = fc_24h.and_then(parse_datetime_flexible);
    match (base, update_time.and_then(parse_hhmm)) {
        // `fc_24h` 自带完整年月日，`update_time` 只是同一页面上的时分补充，因此这里
        // 用 `base` 自己的年月日拼装即可，**不需要** `now`，也不存在跨零点歧义：
        // 日期是数据源明确给出的，不是推断出来的。
        (Some(base), Some((hour, minute))) => {
            build_cst(base.year_of(), base.month_of(), base.day_of(), hour, minute)
        }
        (Some(base), None) => Some(base),
        // 只有时分：日期只能取「今天」，于是跨零点就成了真实存在的风险。
        // 若拼出的 candidate 比 `now` 还晚 [`PUBLISH_CLOCK_SKEW_HOURS`] 小时以上，
        // 说明它是**昨天**的发布时间，回滚 1 天。
        (None, Some((hour, minute))) => {
            let today = now.date_naive();
            let candidate = build_cst(
                today.year_of(),
                today.month_of(),
                today.day_of(),
                hour,
                minute,
            )?;
            if candidate > now + ChronoDuration::hours(PUBLISH_CLOCK_SKEW_HOURS) {
                candidate.checked_sub_signed(ChronoDuration::days(1))
            } else {
                Some(candidate)
            }
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

/// 把「日 + 时」还原成完整日期。
///
/// 上游只给日号与小时，月/年必须推断。这里在「基准月的前一个月、基准月、基准月的
/// 后一个月」三个候选里取**离基准时刻最近**的那一个：7 天预报最多偏离发布时间
/// 7 天，而另外两个候选至少偏离 23 天，所以最近的那个一定是正确的那一个。
///
/// 相比旧的「日号小于基准日号就进下个月」规则，这条同时处理两种情况：
///
/// * 跨月 / 跨年：`11-30` 页面上的 `01日02时` 归到 `12-01`，`12-31` 上的 `01日`
///   归到次年 `01-01`；
/// * **内联数据比发布时间更早**：页面 07:30 刷新、`1d` 仍带 `05日23时` 时，
///   旧规则会把 `05日` 算成下个月的同一天，凭空造出一份一个月后的天气。
fn resolve_hour_date(
    base: DateTime<chrono::FixedOffset>,
    day: u32,
    hour: u32,
) -> Option<DateTime<chrono::FixedOffset>> {
    let mut best: Option<(i64, DateTime<chrono::FixedOffset>)> = None;
    // 顺序 0 / +1 / -1：严格小于比较下偏离最小的候选胜出，平局时「当月」优先，
    // 因此结果与候选的枚举顺序无关、完全确定。
    for delta in [0i32, 1, -1] {
        let (year, month) = shift_month(base.year_of(), base.month_of(), delta);
        let Some(candidate) = build_cst(year, month, day, hour, 0) else {
            // 例如 2 月没有 30 日：这个候选不存在，换下一个。
            continue;
        };
        let distance = (candidate - base).num_seconds().abs();
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

/// 把 `(年, 月)` 平移 `delta` 个月，并正确处理跨年。
fn shift_month(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let months = year * 12 + (month as i32 - 1) + delta;
    (months.div_euclid(12), (months.rem_euclid(12) + 1) as u32)
}

/// 当前北京时间。
pub fn cst_now() -> DateTime<chrono::FixedOffset> {
    chrono::Utc::now().with_timezone(&cst_offset())
}

fn regex_leading_number() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // 锚定在字符串开头：只取**前导**数值，不做全文搜索（D7）。
    RE.get_or_init(|| Regex::new(r"^\s*(-?\d+(?:\.\d+)?)").expect("静态正则合法"))
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

    /// 既有的两条断言原样保留，只把函数名换成严格解析器 `resolve_forecast_date`
    /// （旧实现 `next_date_with_day` 会向前搜索 14 个月，正是 W2 的根因）。
    #[test]
    fn finds_next_date_with_day() {
        let start = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
        assert_eq!(
            resolve_forecast_date(start, 6),
            Some(NaiveDate::from_ymd_opt(2026, 11, 6).unwrap())
        );
        assert_eq!(
            resolve_forecast_date(NaiveDate::from_ymd_opt(2026, 12, 30).unwrap(), 2),
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

    /// 线上多日字段叫 `23d`（数组的数组），`1d` 不足 8 个点时必须能续上它。
    #[test]
    fn hourly_merges_23d_when_1d_is_short() {
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

    // -----------------------------------------------------------------------
    // W1：逐三小时预报合并时空记录不得覆盖有效记录
    // -----------------------------------------------------------------------

    fn hourly_doc(published: &str, hour3data: &str) -> String {
        format!(
            r#"<html><body>
<input type="hidden" id="fc_3h_internal_update_time" value="{published}"/>
<script>var hour3data={hour3data};</script>
</body></html>"#
        )
    }

    /// `1d` 的同刻记录只有时间戳（`"06日11时,,,,,"`），`23d` 有完整数据：
    /// 唯一留下的时点必须来自 `23d`，不能被空记录整条挤掉。
    #[test]
    fn hourly_empty_record_does_not_evict_a_complete_23d_point() {
        let html = hourly_doc(
            "2026-11-06 07:30",
            r#"{"1d":["06日11时,,,,,"],
                "23d":[["06日11时,d00,晴,12℃,西风,<3级,1"]],
                "7d":[["06日11时,d07,小雨,-9℃,南风,8级,9"]]}"#,
        );
        let payload = parse_hourly_page(&html).expect("应可解析");
        assert_eq!(payload.points.len(), 1, "同一时刻只能出现一次");
        let point = &payload.points[0];
        assert_eq!(point.time, build_cst(2026, 11, 6, 11, 0).expect("合法时间"));
        assert_eq!(point.temperature_c, Some(12));
        assert_eq!(point.weather.as_deref(), Some("晴"));
        assert_eq!(point.wind_direction.as_deref(), Some("西风"));
        assert_eq!(point.wind_scale.as_deref(), Some("<3级"));
    }

    /// 「整条记录」语义：`1d` 部分缺失时仍然整条优先，绝不从 `23d` 借字段拼一条
    /// 混合记录；`1d` 与 `23d` 内容冲突时同样是 `1d` 的值胜出。
    #[test]
    fn hourly_partial_1d_wins_whole_record_without_borrowing_fields() {
        let html = hourly_doc(
            "2026-11-06 07:30",
            r#"{"1d":["06日11时,d00,晴,,,","06日14时,d00,多云,12℃,西风,<3级,1"],
                "23d":[["06日11时,d00,晴,99℃,北风,9级,9","06日14时,n09,暴雨,-30℃,北风,9级,9"]]}"#,
        );
        let payload = parse_hourly_page(&html).expect("应可解析");
        assert_eq!(payload.points.len(), 2);

        let partial = &payload.points[0];
        assert_eq!(partial.weather.as_deref(), Some("晴"));
        assert_eq!(partial.temperature_c, None, "不得从 23d 借温度");
        assert_eq!(partial.wind_direction, None, "不得从 23d 借风向");
        assert_eq!(partial.wind_scale, None, "不得从 23d 借风力");

        let conflicting = &payload.points[1];
        assert_eq!(conflicting.weather.as_deref(), Some("多云"), "`1d` 优先");
        assert_eq!(conflicting.temperature_c, Some(12));
        assert_eq!(conflicting.wind_scale.as_deref(), Some("<3级"));
    }

    /// 三个来源同刻都没有气象信息 → 该时点整条丢弃（时间序列留空洞，不补点）；
    /// 全页只剩这类记录时报 `parse_error`，文案与「没有可解析时间戳」区分开。
    #[test]
    fn hourly_drops_points_without_weather_data() {
        let mixed = hourly_doc(
            "2026-11-06 07:30",
            r#"{"1d":["06日11时,d00,,,,","06日14时,d00,晴,12℃,西风,<3级,1"],
                "23d":[["06日11时,,,,,"]],
                "7d":[["06日11时,n01,,,,"]]}"#,
        );
        let payload = parse_hourly_page(&mixed).expect("有效时点仍然存在");
        assert_eq!(
            payload.points.len(),
            1,
            "06日11时 三个来源全空，必须整条丢弃而不是留一个空壳时点"
        );
        assert_eq!(
            payload.points[0].time,
            build_cst(2026, 11, 6, 14, 0).expect("合法时间")
        );

        let all_empty = hourly_doc(
            "2026-11-06 07:30",
            r#"{"1d":["06日11时,,,,,","06日14时,d00,,,,"],
                "7d":[["06日11时,n01,,,,"]]}"#,
        );
        let err = parse_hourly_page(&all_empty).expect_err("没有任何气象信息必须报错");
        assert_eq!(err.code(), "parse_error");
        assert!(err.to_string().contains("没有任何携带气象信息"), "{err}");
    }

    /// 同一字段内出现重复时间戳时只保留一条，且保留的是**有效**的那一条：
    /// 空记录排在前也不能把有效记录挤掉。
    #[test]
    fn hourly_keeps_the_valid_copy_for_duplicate_timestamps() {
        let html = hourly_doc(
            "2026-11-06 07:30",
            r#"{"1d":["06日11时,,,,,","06日11时,d00,晴,12℃,西风,<3级,1"]}"#,
        );
        let payload = parse_hourly_page(&html).expect("应可解析");
        assert_eq!(payload.points.len(), 1, "同一时刻只能出现一次");
        assert_eq!(payload.points[0].temperature_c, Some(12));
        assert_eq!(payload.points[0].weather.as_deref(), Some("晴"));
    }

    /// 回归保护：真实页面解析后仍是 32 个时点，首末点与逐点字段都完整，
    /// 说明「过滤空记录」没有误伤真实数据（真实样本里本来就没有空记录）。
    #[test]
    fn real_full_hourly_page_still_yields_complete_points() {
        const FULL_PAGE: &str =
            include_str!("../../tests/fixtures/hourly_1d_101010100_20261106.html");
        let payload = parse_hourly_page(FULL_PAGE).expect("真实页面应可解析");
        assert_eq!(payload.points.len(), MAX_HOURLY_POINTS);
        assert_eq!(
            payload.points[0].time.format("%Y-%m-%d %H:%M").to_string(),
            "2026-11-06 11:00"
        );
        assert_eq!(
            payload.points[MAX_HOURLY_POINTS - 1]
                .time
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-11-10 08:00"
        );
        for point in &payload.points {
            assert!(point.temperature_c.is_some(), "{:?}", point.time);
            assert!(point.weather.is_some(), "{:?}", point.time);
        }
    }

    // -----------------------------------------------------------------------
    // W2：7 日预报日期解析失败必须跳过条目，不能回退成「今天」
    // -----------------------------------------------------------------------

    fn forecast_doc(published: &str, items: &str) -> String {
        format!(
            r#"<html><body>
<input type="hidden" id="fc_24h_internal_update_time" value="{published}"/>
<ul class="t">{items}</ul>
</body></html>"#
        )
    }

    fn forecast_item(heading: &str, weather: &str) -> String {
        format!(r#"<li><h1>{heading}</h1><p class="wea">{weather}</p></li>"#)
    }

    fn dates_of(payload: &ForecastPayload) -> Vec<String> {
        payload.days.iter().map(|day| day.date.clone()).collect()
    }

    /// 严格解析器：日号越界、跨月跳月、2 月 29 日都必须按「是否真实存在」判定，
    /// 合法的跨月 / 跨年仍然要能解析出来。
    #[test]
    fn forecast_date_resolution_rejects_impossible_or_far_off_days() {
        let nov6 = NaiveDate::from_ymd_opt(2026, 11, 6).expect("合法日期");
        assert_eq!(resolve_forecast_date(nov6, 0), None, "00日 不存在");
        assert_eq!(resolve_forecast_date(nov6, 32), None, "32日 不存在");
        assert_eq!(
            resolve_forecast_date(nov6, 31),
            None,
            "11 月只有 30 天，12-31 距锚点 55 天，属于坏标题"
        );
        assert_eq!(
            resolve_forecast_date(NaiveDate::from_ymd_opt(2026, 11, 30).expect("合法日期"), 1),
            Some(NaiveDate::from_ymd_opt(2026, 12, 1).expect("合法日期")),
            "跨月：间隔 1 天，合法"
        );
        assert_eq!(
            resolve_forecast_date(NaiveDate::from_ymd_opt(2026, 12, 31).expect("合法日期"), 1),
            Some(NaiveDate::from_ymd_opt(2027, 1, 1).expect("合法日期")),
            "跨年：12-31 的 01日 → 次年 01-01"
        );
        assert_eq!(
            resolve_forecast_date(NaiveDate::from_ymd_opt(2026, 2, 1).expect("合法日期"), 29),
            None,
            "平年没有 2 月 29 日"
        );
        assert_eq!(
            resolve_forecast_date(NaiveDate::from_ymd_opt(2028, 2, 28).expect("合法日期"), 29),
            Some(NaiveDate::from_ymd_opt(2028, 2, 29).expect("合法日期")),
            "闰年 2 月 29 日合法"
        );
    }

    /// 重复日期与逆序日期的条目被跳过，其它有效条目原样保留；游标不被污染。
    #[test]
    fn forecast_page_skips_duplicate_and_backwards_items() {
        let html = forecast_doc(
            "202611060800",
            &[
                forecast_item("6日（今天）", "晴"),
                forecast_item("6日（今天）", "晴"),
                forecast_item("5日（周四）", "雨"),
                forecast_item("7日（周六）", "多云"),
            ]
            .concat(),
        );
        let payload = parse_forecast_page(&html).expect("有效条目仍然存在");
        assert_eq!(dates_of(&payload), vec!["2026-11-06", "2026-11-07"]);
        assert_eq!(payload.days[1].weather.as_deref(), Some("多云"));
    }

    /// 日期标题与 `今天`/`明天` 标签矛盾时该条目跳过，绝不把期望日期硬塞进去。
    #[test]
    fn forecast_page_skips_items_whose_heading_contradicts_the_label() {
        let html = forecast_doc(
            "202611060800",
            &[
                forecast_item("6日（今天）", "晴"),
                forecast_item("9日（明天）", "雨"),
                forecast_item("7日（明天）", "多云"),
            ]
            .concat(),
        );
        let payload = parse_forecast_page(&html).expect("有效条目仍然存在");
        assert_eq!(dates_of(&payload), vec!["2026-11-06", "2026-11-07"]);
        assert_eq!(payload.days[1].label.as_deref(), Some("明天"));
    }

    /// 有条目但日期全部无效 → 结构化 `parse_error`，文案与「一个条目都没有」区分开。
    #[test]
    fn forecast_page_reports_parse_error_when_all_dates_are_invalid() {
        let html = forecast_doc(
            "202611060800",
            &[
                forecast_item("32日（今天）", "晴"),
                forecast_item("00日（周五）", "晴"),
                forecast_item("31日（周五）", "晴"),
            ]
            .concat(),
        );
        let err = parse_forecast_page(&html).expect_err("日期全部无效必须报错");
        assert_eq!(err.code(), "parse_error");
        assert!(err.to_string().contains("日期全部无效"), "{err}");

        let err = parse_forecast_page("<html><body><p>改版了</p></body></html>")
            .expect_err("没有条目必须报错");
        assert_eq!(err.code(), "parse_error");
        assert!(err.to_string().contains("页面结构可能已变化"), "{err}");
    }

    // -----------------------------------------------------------------------
    // W3：实况接口空数据不得伪装成成功
    // -----------------------------------------------------------------------

    /// 一条气象字段都没有的 dataSK 必须是 `parse_error`，不能返回全空的观测。
    #[test]
    fn current_rejects_payload_without_any_weather_field() {
        for body in [
            "var dataSK={};",
            r#"var dataSK={"cityname":"北京"}"#,
            r#"var dataSK={"temp":"N/A"}"#,
        ] {
            let err = parse_current_js(body).expect_err("空 dataSK 必须报错");
            assert_eq!(err.code(), "parse_error", "{body}");
            assert!(
                err.to_string().contains("不含任何气象字段"),
                "{body} → {err}"
            );
        }
    }

    /// 部分有效（只给温度）也算成功：观测时间缺失不是失败，缺失字段如实标注，
    /// 不得把缺失的温度/湿度填成 0。真实样本同样必须仍可解析。
    #[test]
    fn current_accepts_partial_payloads_and_marks_missing_fields() {
        let only_temp =
            parse_current_js(r#"var dataSK={"temp":"12"}"#).expect("只有温度也算部分有效");
        assert_eq!(only_temp.temperature_c, Some(12.0));
        let observation = only_temp.into_observation(city(None), "u".into(), fetched_at());
        assert!(
            observation.missing_fields.iter().any(|f| f == "weather"),
            "{:?}",
            observation.missing_fields
        );
        assert!(
            !observation
                .missing_fields
                .iter()
                .any(|f| f == "temperature_c"),
            "真实存在的字段不得出现在 missing_fields 里：{:?}",
            observation.missing_fields
        );
        assert_eq!(observation.temperature_c, Some(12.0));

        let with_weather =
            parse_current_js(r#"var dataSK={"temp":"12","weather":"晴"}"#).expect("两个字段都有效");
        assert_eq!(with_weather.temperature_c, Some(12.0));
        assert_eq!(with_weather.weather.as_deref(), Some("晴"));

        const SAMPLE: &str = include_str!("../../tests/fixtures/current_sk_2d.txt");
        let real = parse_current_js(SAMPLE).expect("真实样本应可解析");
        assert!(real.temperature_c.is_some(), "{:?}", real.temperature_c);
        assert_eq!(real.weather.as_deref(), Some("晴"));
    }

    /// dataSK 不是对象（null / 数组 / 数字）时直接报错，不继续读字段。
    #[test]
    fn current_rejects_non_object_datask() {
        for body in ["var dataSK=null;", "var dataSK=[1,2,3];", "var dataSK=42;"] {
            let err = parse_current_js(body).expect_err("非对象 dataSK 必须报错");
            assert_eq!(err.code(), "parse_error", "{body}");
        }
        // 数据块彻底缺失（页面改版）走另一条措辞，同样是 `parse_error`。
        let err = parse_current_js("<html>改版了</html>").expect_err("没有 dataSK 必须报错");
        assert_eq!(err.code(), "parse_error");
        assert!(err.to_string().contains("未返回 dataSK"), "{err}");
    }
}
