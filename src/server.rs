//! MCP 服务器：把天气数据源包装成 MCP 工具。

use std::sync::Arc;

use chrono::{DateTime, FixedOffset, Timelike};
use rmcp::ErrorData;
use rmcp::handler::server::tool::{ToolRouter, schema_for_output};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Serialize;

use crate::city::{CityIndex, entry_to_info, is_city_code};
use crate::error::WeatherError;
use crate::models::{AlertBundle, ForecastBundle, HourlyBundle, WeatherObservation};
use crate::providers::WeatherProvider;
use crate::providers::weather_com_cn::cst_now;
use crate::tools::{
    AlertQuery, CityIndexMeta, CurrentWeatherQuery, ForecastQuery, HourlyQuery, SearchCitiesOutput,
    SearchCitiesQuery,
};
use crate::window;

/// 服务器指令，会在 `initialize` 响应里发给客户端。
const INSTRUCTIONS: &str = "\
weather-mcp-cn 通过中国天气网（www.weather.com.cn）的公开页面与数据接口提供中国境内天气查询。\n\
使用要点：\n\
1. city 参数既可以是中文名称（北京、深圳市南山区、湖北省武汉市），也可以是 9 位城市代码；不确定时先调用 search_cities。\n\
2. get_current_weather 返回的是气象站实况观测，带观测时间；数据源不提供体感温度、降水概率，这些字段不会出现。\n\
3. get_weather_forecast 最多 7 天，来自 7 日预报页面。\n\
4. get_hourly_weather 的数据粒度是 3 小时（数据源不提供逐 1 小时预报），只返回仍然覆盖未来的时点，绝不插值。requested_hours / covered_hours（窗口内累计被覆盖的小时数）/ contiguous_hours（从现在起连续被覆盖的小时数）/ reference_time 一起说明窗口与真实覆盖时长；两者不相等就说明窗口里有数据缺口。\n\
5. get_weather_alerts 中 has_alerts=false 与查询失败是两种不同状态，请分别处理。\n\
6. 工具执行失败会返回 isError=true 的结果，其中 structuredContent.code 是稳定错误码（city_not_found / ambiguous_city / timeout / parse_error 等），retryable 与 hint 说明该不该重试、怎么改写查询。\n\
所有结果都带 source.url 与抓取时间，请勿把抓取时间当成观测时间。";

/// 可注入的时钟，默认取系统时间（UTC+8）。
///
/// 时间窗语义必须能用固定时间验证，所以这里不直接调 `cst_now()`：
/// 生产环境用默认值，测试可以注入一个固定时刻，从而得到完全确定的结果。
pub type Clock = Arc<dyn Fn() -> DateTime<FixedOffset> + Send + Sync>;

/// MCP 天气服务器。
pub struct WeatherServer {
    provider: Arc<dyn WeatherProvider>,
    index: Arc<CityIndex>,
    clock: Clock,
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for WeatherServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WeatherServer")
            .field("provider", &self.provider.id())
            .field("cities", &self.index.len())
            .finish()
    }
}

/// 把工具执行结果转成 MCP `CallToolResult`。
///
/// * `Ok(value)` → `structuredContent` + `content[0].text`，`isError=false`；
/// * `Err(WeatherError)` → `isError=true` 的结构化工具错误（带机器可读错误码）。
///
/// 只有「结果序列化失败」这种真正的服务端内部故障才会返回 JSON-RPC 协议错误。
fn tool_outcome<T: Serialize>(
    result: Result<T, WeatherError>,
) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => {
            let value = serde_json::to_value(&value).map_err(|err| {
                ErrorData::internal_error(format!("工具结果序列化失败：{err}"), None)
            })?;
            Ok(CallToolResult::structured(value))
        }
        Err(err) => Ok(err.to_tool_result()),
    }
}

#[tool_router]
impl WeatherServer {
    /// 创建服务器实例（使用系统时钟）。
    pub fn new(provider: Arc<dyn WeatherProvider>, index: Arc<CityIndex>) -> Self {
        Self::with_clock(provider, index, Arc::new(cst_now))
    }

    /// 创建服务器实例并注入时钟（确定性测试用）。
    pub fn with_clock(
        provider: Arc<dyn WeatherProvider>,
        index: Arc<CityIndex>,
        clock: Clock,
    ) -> Self {
        Self {
            provider,
            index,
            clock,
            tool_router: Self::tool_router(),
        }
    }

    /// 解析城市名，失败时返回结构化错误。
    fn resolve_city(&self, raw: &str) -> Result<crate::city::ResolvedCity, WeatherError> {
        self.index.resolve(raw)
    }

    /// 本次调用的「现在」。
    ///
    /// 截断到整秒：时间窗按 3 小时粒度裁剪，纳秒精度没有意义，只会让
    /// `reference_time` / `coverage_end` 的输出变吵。
    fn now(&self) -> DateTime<FixedOffset> {
        let now = (self.clock)();
        now.with_nanosecond(0).unwrap_or(now)
    }

    fn city_index_meta(&self) -> CityIndexMeta {
        CityIndexMeta {
            total: self.index.len(),
            generated_at: self.index.generated_at().to_string(),
            source: self.index.source().to_string(),
        }
    }

    /// 查询指定城市当前天气实况（温度、天气现象、湿度、风向风力、气压、能见度、AQI 等）。
    /// 返回的是气象站实况观测值，不是预报值；带观测时间与数据来源 URL。
    #[tool(
        name = "get_current_weather",
        description = "查询中国城市当前天气实况。city 支持中文名称（北京 / 深圳市南山区 / 湖北省武汉市）或 9 位中国天气网城市代码。返回气温、天气现象、湿度、风向、风力、风速、气压、能见度、AQI、观测时间与来源 URL；数据源没有的字段不会出现在结果里。",
        output_schema = schema_for_output::<WeatherObservation>()
    )]
    async fn get_current_weather(
        &self,
        Parameters(query): Parameters<CurrentWeatherQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let observation: Result<WeatherObservation, WeatherError> = async {
            let city = self.resolve_city(&query.city)?.to_info();
            self.provider.current_weather(&city).await
        }
        .await;
        tool_outcome(observation)
    }

    /// 查询指定城市未来 1-7 天天气预报。
    #[tool(
        name = "get_weather_forecast",
        description = "查询中国城市未来逐日天气预报（最多 7 天，默认 3 天）。city 支持中文名称或 9 位城市代码；days 取值 1-7。返回每天的日期、白天/夜间天气、最高/最低气温、风向风力、预报发布时间与来源 URL。",
        output_schema = schema_for_output::<ForecastBundle>()
    )]
    async fn get_weather_forecast(
        &self,
        Parameters(query): Parameters<ForecastQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let days = query.days.unwrap_or(3).clamp(1, 7) as usize;
        let bundle: Result<ForecastBundle, WeatherError> = async {
            let city = self.resolve_city(&query.city)?.to_info();
            let mut bundle = self.provider.daily_forecast(&city).await?;
            if bundle.days.is_empty() {
                return Err(WeatherError::Unsupported(
                    "中国天气网当前没有返回任何预报数据".into(),
                ));
            }
            if bundle.days.len() > days {
                bundle.days.truncate(days);
            }
            Ok(bundle)
        }
        .await;
        tool_outcome(bundle)
    }

    /// 查询指定城市未来逐 3 小时天气（数据源为 3 小时粒度）。
    #[tool(
        name = "get_hourly_weather",
        description = "查询中国城市未来天气的逐时点预报。重要：中国天气网公开数据只有 3 小时粒度，本工具返回时间窗内每 3 小时一个时点，绝不插值或推算。hours 取值 1-24，默认 12。返回 requested_hours、covered_hours（窗口内累计被数据覆盖的小时数，不把缺口算进去）、contiguous_hours（从 reference_time 起连续被覆盖的小时数）、reference_time（裁剪窗口所用的当前时刻）与 hours 数组；covered_hours 与 contiguous_hours 不相等说明窗口内存在数据缺口。窗口内没有任何有效未来时点时会返回 isError=true 的工具错误。",
        output_schema = schema_for_output::<HourlyBundle>()
    )]
    async fn get_hourly_weather(
        &self,
        Parameters(query): Parameters<HourlyQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let hours = query.hours.unwrap_or(12).clamp(1, 24);
        let bundle: Result<HourlyBundle, WeatherError> = async {
            let city = self.resolve_city(&query.city)?.to_info();
            let mut bundle = self.provider.hourly_forecast(&city).await?;
            let now = self.now();
            match window::select(&bundle.hours, now, i64::from(hours)) {
                Ok(selection) => {
                    bundle.hours = selection.points;
                    bundle.requested_hours = hours;
                    bundle.covered_hours = selection.covered_hours;
                    bundle.contiguous_hours = Some(selection.contiguous_hours);
                    bundle.reference_time = Some(selection.reference_time);
                    bundle.coverage_end = Some(selection.coverage_end);
                    Ok(bundle)
                }
                Err(miss) => Err(WeatherError::ForecastWindowUnavailable {
                    message: miss.message(hours),
                    requested_hours: hours,
                    available_from: miss.available_from,
                    available_until: miss.available_until,
                }),
            }
        }
        .await;
        tool_outcome(bundle)
    }

    /// 搜索城市并返回中国天气网城市代码。
    #[tool(
        name = "search_cities",
        description = "搜索中国城市，返回城市名称、所属行政区划（省/市/区县）、中国天气网 9 位城市代码与行政区划层级。query 可以是名称片段（南山、天河、苏州）或城市代码。用于把用户说的地名映射成可查询的城市与代码。",
        output_schema = schema_for_output::<SearchCitiesOutput>()
    )]
    async fn search_cities(
        &self,
        Parameters(query): Parameters<SearchCitiesQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = query.limit.unwrap_or(10).clamp(1, 50) as usize;

        let normalized = crate::city::normalize(&query.query);
        let entries = if is_city_code(&normalized) {
            self.index.get(&normalized).cloned().into_iter().collect()
        } else {
            self.index.search(&query.query, limit)
        };

        tool_outcome(Ok(SearchCitiesOutput {
            query: query.query,
            count: entries.len(),
            cities: entries.iter().map(entry_to_info).collect(),
            city_index: self.city_index_meta(),
        }))
    }

    /// 查询指定城市当前生效的气象预警。
    #[tool(
        name = "get_weather_alerts",
        description = "查询中国城市当前生效的气象预警。返回预警类型、级别、发布时间、失效时间、发布机构、正文与详情链接。has_alerts=false 表示查询成功且当前确实没有预警；查询失败会返回 isError=true 的错误结果而不是空数组，两者不可混淆。",
        output_schema = schema_for_output::<AlertBundle>()
    )]
    async fn get_weather_alerts(
        &self,
        Parameters(query): Parameters<AlertQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let bundle: Result<AlertBundle, WeatherError> = async {
            let city = self.resolve_city(&query.city)?.to_info();
            self.provider.alerts(&city).await
        }
        .await;
        tool_outcome(bundle)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WeatherServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result as WeatherResult;
    use crate::models::{
        CityInfo, CityLevel, DailyForecast, HourlyForecast, SourceInfo, WeatherAlert, cst_offset,
    };
    use crate::providers::BoxFut;
    use chrono::{Duration as ChronoDuration, TimeZone};
    use std::sync::Mutex;

    /// 只回放固定数据的假数据源。
    ///
    /// 关键点：它把**本次调用传入的 city** 原样回填到结果里。如果服务器把上一次请求的
    /// 城市上下文缓存下来复用，下面的「同一代码两次调用」测试就会失败。
    struct FakeProvider {
        seen: Mutex<Vec<CityInfo>>,
        hourly: Vec<HourlyForecast>,
        hourly_error: Option<WeatherError>,
    }

    impl FakeProvider {
        fn new(hourly: Vec<HourlyForecast>) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                hourly,
                hourly_error: None,
            }
        }

        fn failing(error: WeatherError) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                hourly: Vec::new(),
                hourly_error: Some(error),
            }
        }

        fn record(&self, city: &CityInfo) {
            self.seen.lock().expect("未中毒").push(city.clone());
        }
    }

    impl WeatherProvider for FakeProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn homepage(&self) -> &'static str {
            "https://example.invalid/"
        }

        fn current_weather<'a>(
            &'a self,
            city: &'a CityInfo,
        ) -> BoxFut<'a, WeatherResult<WeatherObservation>> {
            Box::pin(async move {
                self.record(city);
                Ok(WeatherObservation {
                    city: city.clone(),
                    temperature_c: Some(20.5),
                    weather: Some("晴".into()),
                    feels_like_c: None,
                    humidity_percent: Some(40.0),
                    wind_direction: Some("北风".into()),
                    wind_scale: Some("2级".into()),
                    wind_speed_kmh: Some(8.0),
                    precipitation_mm: Some(0.0),
                    precipitation_24h_mm: Some(0.0),
                    pressure_hpa: Some(1013.0),
                    visibility_km: Some(20.0),
                    aqi: Some(30),
                    observed_at: Some(cst_now()),
                    observed_at_raw: Some("11月06日 09:00".into()),
                    observed_at_note: None,
                    missing_fields: vec!["feels_like_c".into()],
                    source: SourceInfo::new("https://example.invalid/sk_2d", "假数据源（仅测试）"),
                    is_observation: true,
                })
            })
        }

        fn daily_forecast<'a>(
            &'a self,
            city: &'a CityInfo,
        ) -> BoxFut<'a, WeatherResult<ForecastBundle>> {
            Box::pin(async move {
                self.record(city);
                let days: Vec<DailyForecast> = (0..3)
                    .map(|index| DailyForecast {
                        date: format!("2026-11-0{}", index + 6),
                        label: Some(format!("第 {index} 天")),
                        weekday: None,
                        weather: Some("晴".into()),
                        weather_day: Some("晴".into()),
                        weather_night: None,
                        temperature_high_c: Some(15),
                        temperature_low_c: Some(5),
                        wind_direction_day: None,
                        wind_direction_night: None,
                        wind_scale: None,
                    })
                    .collect();
                Ok(ForecastBundle {
                    city: city.clone(),
                    published_at: Some(cst_now()),
                    published_at_raw: None,
                    available_days: days.len(),
                    days,
                    weather_split_note: None,
                    source: SourceInfo::new(
                        "https://example.invalid/weather",
                        "假数据源（仅测试）",
                    ),
                })
            })
        }

        fn hourly_forecast<'a>(
            &'a self,
            city: &'a CityInfo,
        ) -> BoxFut<'a, WeatherResult<HourlyBundle>> {
            Box::pin(async move {
                self.record(city);
                if let Some(error) = self.hourly_error.clone() {
                    return Err(error);
                }
                Ok(HourlyBundle {
                    city: city.clone(),
                    published_at: Some(cst_now()),
                    published_at_raw: None,
                    resolution_hours: 3,
                    requested_hours: 0,
                    covered_hours: 0,
                    contiguous_hours: None,
                    reference_time: None,
                    coverage_end: None,
                    // 故意不排序、并塞一个重复点：窗口函数必须自己保证顺序与去重。
                    hours: {
                        let mut points = self.hourly.clone();
                        points.reverse();
                        if let Some(first) = points.first().cloned() {
                            points.push(first);
                        }
                        points
                    },
                    note: "假数据源".into(),
                    source: SourceInfo::new("https://example.invalid/1d", "假数据源（仅测试）"),
                })
            })
        }

        fn alerts<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, WeatherResult<AlertBundle>> {
            Box::pin(async move {
                self.record(city);
                let alerts = vec![WeatherAlert {
                    title: "测试预警".into(),
                    alert_type: Some("大风".into()),
                    level: Some("蓝色".into()),
                    level_code: None,
                    type_code: None,
                    publisher: None,
                    published_at: None,
                    published_at_raw: None,
                    expires_at: None,
                    content: "测试正文".into(),
                    detail_url: None,
                    region: "测试省".into(),
                }];
                Ok(AlertBundle {
                    city: city.clone(),
                    has_alerts: true,
                    alerts,
                    status: "active_alerts_found".into(),
                    source: SourceInfo::new(
                        "https://example.invalid/dingzhi",
                        "假数据源（仅测试）",
                    ),
                })
            })
        }
    }

    fn index() -> Arc<CityIndex> {
        Arc::new(CityIndex::from_json(crate::EMBEDDED_CITIES_JSON).expect("内置城市库可解析"))
    }

    fn fixed_clock(hour: u32, minute: u32) -> Clock {
        let now = cst_offset()
            .with_ymd_and_hms(2026, 11, 5, hour, minute, 0)
            .single()
            .expect("合法时间");
        Arc::new(move || now)
    }

    fn hourly_points(start_hour: u32, count: usize) -> Vec<HourlyForecast> {
        let base = cst_offset()
            .with_ymd_and_hms(2026, 11, 5, start_hour, 0, 0)
            .single()
            .expect("合法时间");
        (0..count)
            .map(|index| HourlyForecast {
                time: base + ChronoDuration::hours(3 * index as i64),
                weather: Some("晴".into()),
                temperature_c: Some(5),
                wind_direction: None,
                wind_scale: None,
                icon_code: None,
            })
            .collect()
    }

    fn server(provider: Arc<dyn WeatherProvider>, clock: Clock) -> WeatherServer {
        WeatherServer::with_clock(provider, index(), clock)
    }

    /// 从工具结果里取出结构化负载，并断言它是成功结果。
    fn payload(result: &CallToolResult) -> serde_json::Value {
        assert_ne!(result.is_error, Some(true), "不应是错误结果：{result:?}");
        result
            .structured_content
            .clone()
            .expect("成功结果必须带 structuredContent")
    }

    /// 从工具结果里取出结构化负载，并断言它确实是 `isError: true`。
    fn error_payload(result: &CallToolResult) -> serde_json::Value {
        assert_eq!(
            result.is_error,
            Some(true),
            "应当是 isError=true：{result:?}"
        );
        let value = result
            .structured_content
            .clone()
            .expect("工具错误必须带 structuredContent");
        // 错误也必须在 content[0].text 里可见（有些客户端不读 structuredContent）。
        let text = result
            .content
            .first()
            .and_then(|block| block.as_text())
            .map(|text| text.text.clone())
            .unwrap_or_default();
        assert!(!text.is_empty(), "工具错误必须带可读文本");
        value
    }

    #[tokio::test]
    async fn hourly_tool_reports_honest_window() {
        let provider = Arc::new(FakeProvider::new(hourly_points(23, 9)));
        let server = server(provider, fixed_clock(23, 30));

        let result = server
            .get_hourly_weather(Parameters(HourlyQuery {
                city: "北京".into(),
                hours: Some(12),
            }))
            .await
            .expect("工具调用本身不应报协议错误");

        let payload = payload(&result);
        assert_eq!(payload["requested_hours"], 12);
        assert_eq!(payload["covered_hours"], 12);
        assert_eq!(payload["contiguous_hours"], 12, "数据连续，两者相等");
        assert_eq!(payload["reference_time"], "2026-11-05T23:30:00+08:00");
        assert_eq!(payload["coverage_end"], "2026-11-06T11:30:00+08:00");
        let hours = payload["hours"].as_array().expect("hours 数组");
        assert_eq!(hours.len(), 5, "23/02/05/08/11 五个时点");
        assert_eq!(hours[0]["time"], "2026-11-05T23:00:00+08:00");
        assert_eq!(hours[4]["time"], "2026-11-06T11:00:00+08:00");
    }

    /// 上游数据中间有缺口时，工具输出必须同时给出「累计覆盖」与「连续覆盖」，
    /// 且都不能等于请求的 12 小时。
    #[tokio::test]
    async fn hourly_tool_distinguishes_total_and_contiguous_coverage() {
        // 23:00（覆盖到 02:00）与 05:00（覆盖到 08:00），02:00~05:00 没有数据。
        let mut points = hourly_points(23, 1);
        points.extend(hourly_points(5, 1));
        points[1].time = cst_offset()
            .with_ymd_and_hms(2026, 11, 6, 5, 0, 0)
            .single()
            .expect("合法时间");

        let provider = Arc::new(FakeProvider::new(points));
        let server = server(provider, fixed_clock(23, 30));

        let result = server
            .get_hourly_weather(Parameters(HourlyQuery {
                city: "北京".into(),
                hours: Some(12),
            }))
            .await
            .expect("工具调用本身不应报协议错误");

        let payload = payload(&result);
        assert_eq!(payload["requested_hours"], 12);
        // [23:30,02:00) = 2.5h，[05:00,08:00) = 3h → 合计 5.5h → 5。
        assert_eq!(payload["covered_hours"], 5, "{payload}");
        assert_eq!(payload["contiguous_hours"], 2, "{payload}");
        assert_eq!(payload["coverage_end"], "2026-11-06T08:00:00+08:00");
        assert_eq!(payload["hours"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn hourly_tool_errors_when_window_has_no_future_points() {
        // 数据只到 08:00–17:00，而「现在」是 23:00：窗口完全没被覆盖。
        let provider = Arc::new(FakeProvider::new(hourly_points(8, 4)));
        let server = server(provider, fixed_clock(23, 0));

        let result = server
            .get_hourly_weather(Parameters(HourlyQuery {
                city: "北京".into(),
                hours: Some(12),
            }))
            .await
            .expect("窗口无数据不是协议错误");

        let payload = error_payload(&result);
        assert_eq!(payload["code"], "forecast_window_unavailable");
        assert_eq!(payload["kind"], "unavailable");
        assert_eq!(payload["retryable"], false);
        assert_eq!(payload["requested_hours"], 12);
        assert!(
            payload["available_from"].as_str().is_some(),
            "错误里要给出上游可用区间：{payload}"
        );
    }

    #[tokio::test]
    async fn hourly_tool_propagates_upstream_error_as_tool_error() {
        let provider = Arc::new(FakeProvider::failing(WeatherError::Timeout("超时".into())));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .get_hourly_weather(Parameters(HourlyQuery {
                city: "北京".into(),
                hours: Some(3),
            }))
            .await
            .expect("上游超时不是协议错误");
        let payload = error_payload(&result);
        assert_eq!(payload["code"], "timeout");
        assert_eq!(payload["retryable"], true);
    }

    #[tokio::test]
    async fn unknown_city_is_a_tool_error_not_a_protocol_error() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .get_current_weather(Parameters(CurrentWeatherQuery {
                city: "不存在的城市名".into(),
            }))
            .await
            .expect("城市解析失败不是协议错误");
        let payload = error_payload(&result);
        assert_eq!(payload["code"], "city_not_found");
        assert_eq!(payload["kind"], "not_found");
        assert_eq!(payload["retryable"], false);
        assert!(payload["hint"].as_str().is_some());
    }

    #[tokio::test]
    async fn ambiguous_city_carries_candidates() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .get_current_weather(Parameters(CurrentWeatherQuery {
                city: "鼓楼".into(),
            }))
            .await
            .expect("歧义不是协议错误");
        let payload = error_payload(&result);
        assert_eq!(payload["code"], "ambiguous_city");
        let candidates = payload["candidates"].as_array().expect("候选城市数组");
        assert!(candidates.len() >= 2, "应给出候选：{payload}");
        assert!(candidates[0]["code"].as_str().is_some());
    }

    /// 同一城市代码的两次不同查询，必须各自绑定自己的 resolution_note。
    #[tokio::test]
    async fn city_context_is_bound_per_request() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let fallback = server
            .get_current_weather(Parameters(CurrentWeatherQuery {
                city: "深圳市粤海街道".into(),
            }))
            .await
            .expect("工具调用成功");
        let fallback = payload(&fallback);
        assert_eq!(fallback["city"]["code"], "101280601");
        assert!(
            fallback["city"]["resolution_note"].as_str().is_some(),
            "街道级地名回退到深圳时应带说明：{fallback}"
        );

        let plain = server
            .get_current_weather(Parameters(CurrentWeatherQuery {
                city: "深圳".into(),
            }))
            .await
            .expect("工具调用成功");
        let plain = payload(&plain);
        assert_eq!(plain["city"]["code"], "101280601");
        assert!(
            plain["city"]["resolution_note"].is_null(),
            "直接查询深圳时不能继承上一次的回退说明：{plain}"
        );
    }

    #[tokio::test]
    async fn forecast_tool_truncates_days() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .get_weather_forecast(Parameters(ForecastQuery {
                city: "北京".into(),
                days: Some(2),
            }))
            .await
            .expect("工具调用成功");
        let payload = payload(&result);
        assert_eq!(payload["days"].as_array().expect("days").len(), 2);
        assert_eq!(payload["available_days"], 3);
    }

    #[tokio::test]
    async fn alerts_tool_returns_structured_bundle() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .get_weather_alerts(Parameters(AlertQuery {
                city: "长春".into(),
            }))
            .await
            .expect("工具调用成功");
        let payload = payload(&result);
        assert_eq!(payload["has_alerts"], true);
        assert_eq!(payload["city"]["level"], "city");
        assert_eq!(payload["alerts"][0]["title"], "测试预警");
    }

    #[tokio::test]
    async fn search_cities_is_offline_and_structured() {
        let provider = Arc::new(FakeProvider::new(hourly_points(9, 4)));
        let server = server(provider, fixed_clock(9, 0));

        let result = server
            .search_cities(Parameters(SearchCitiesQuery {
                query: "南山".into(),
                limit: Some(3),
            }))
            .await
            .expect("工具调用成功");
        let payload = payload(&result);
        assert_eq!(payload["query"], "南山");
        assert!(payload["count"].as_u64().unwrap_or(0) > 0);
        let codes: Vec<String> = payload["cities"]
            .as_array()
            .expect("cities")
            .iter()
            .filter_map(|city| city["code"].as_str().map(str::to_string))
            .collect();
        assert!(codes.contains(&"101280604".to_string()), "{codes:?}");
        // 城市库元信息必须存在，且条数与内置库一致。
        assert_eq!(
            payload["city_index"]["total"].as_u64(),
            Some(server.index.len() as u64)
        );
    }

    /// 假数据源里 `CityLevel` 的序列化形态必须与对外契约一致。
    #[test]
    fn city_level_serializes_as_lowercase() {
        let info = crate::city::entry_to_info(&crate::city::CityEntry {
            code: "101010100".into(),
            name: "北京".into(),
            province: "北京".into(),
            city: "北京".into(),
            district: None,
            level: CityLevel::Municipality,
        });
        let json = serde_json::to_value(info).expect("序列化");
        assert_eq!(json["level"], "municipality");
    }
}
