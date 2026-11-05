//! MCP 服务器：把天气数据源包装成 MCP 工具。

use std::sync::Arc;

use chrono::Duration as ChronoDuration;
use rmcp::ErrorData;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};

use crate::city::{CityIndex, entry_to_info, is_city_code};
use crate::error::WeatherError;
use crate::models::{AlertBundle, ForecastBundle, HourlyBundle, WeatherObservation};
use crate::providers::WeatherProvider;
use crate::providers::weather_com_cn::cst_now;
use crate::tools::{
    AlertQuery, CityIndexMeta, CurrentWeatherQuery, ForecastQuery, HourlyQuery, SearchCitiesOutput,
    SearchCitiesQuery,
};

/// 服务器指令，会在 `initialize` 响应里发给客户端。
const INSTRUCTIONS: &str = "\
weather-mcp-cn 通过中国天气网（www.weather.com.cn）的公开页面与数据接口提供中国境内天气查询。\n\
使用要点：\n\
1. city 参数既可以是中文名称（北京、深圳市南山区、湖北省武汉市），也可以是 9 位城市代码；不确定时先调用 search_cities。\n\
2. get_current_weather 返回的是气象站实况观测，带观测时间；数据源不提供体感温度、降水概率，这些字段不会出现。\n\
3. get_weather_forecast 最多 7 天，来自 7 日预报页面。\n\
4. get_hourly_weather 的数据粒度是 3 小时（数据源不提供逐 1 小时预报），返回结果会明确标注。\n\
5. get_weather_alerts 中 has_alerts=false 与查询失败是两种不同状态，请分别处理。\n\
所有结果都带 source.url 与抓取时间，请勿把抓取时间当成观测时间。";

/// MCP 天气服务器。
pub struct WeatherServer {
    provider: Arc<dyn WeatherProvider>,
    index: Arc<CityIndex>,
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

#[tool_router]
impl WeatherServer {
    /// 创建服务器实例。
    pub fn new(provider: Arc<dyn WeatherProvider>, index: Arc<CityIndex>) -> Self {
        Self {
            provider,
            index,
            tool_router: Self::tool_router(),
        }
    }

    /// 解析城市名，失败时转换成 MCP 错误。
    fn resolve_city(&self, raw: &str) -> Result<crate::city::ResolvedCity, ErrorData> {
        self.index.resolve(raw).map_err(|err| err.to_error_data())
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
        description = "查询中国城市当前天气实况。city 支持中文名称（北京 / 深圳市南山区 / 湖北省武汉市）或 9 位中国天气网城市代码。返回气温、天气现象、湿度、风向、风力、风速、气压、能见度、AQI、观测时间与来源 URL；数据源没有的字段不会出现在结果里。"
    )]
    async fn get_current_weather(
        &self,
        Parameters(query): Parameters<CurrentWeatherQuery>,
    ) -> Result<Json<WeatherObservation>, ErrorData> {
        let resolved = self.resolve_city(&query.city)?;
        let city = resolved.to_info();
        let observation = self
            .provider
            .current_weather(&city)
            .await
            .map_err(|err| err.to_error_data())?;
        Ok(Json(observation))
    }

    /// 查询指定城市未来 1-7 天天气预报。
    #[tool(
        name = "get_weather_forecast",
        description = "查询中国城市未来逐日天气预报（最多 7 天，默认 3 天）。city 支持中文名称或 9 位城市代码；days 取值 1-7。返回每天的日期、白天/夜间天气、最高/最低气温、风向风力、预报发布时间与来源 URL。"
    )]
    async fn get_weather_forecast(
        &self,
        Parameters(query): Parameters<ForecastQuery>,
    ) -> Result<Json<ForecastBundle>, ErrorData> {
        let days = query.days.unwrap_or(3).clamp(1, 7) as usize;
        let resolved = self.resolve_city(&query.city)?;
        let city = resolved.to_info();
        let mut bundle = self
            .provider
            .daily_forecast(&city)
            .await
            .map_err(|err| err.to_error_data())?;
        if bundle.days.is_empty() {
            return Err(
                WeatherError::Unsupported("中国天气网当前没有返回任何预报数据".into())
                    .to_error_data(),
            );
        }
        if bundle.days.len() > days {
            bundle.days.truncate(days);
        }
        Ok(Json(bundle))
    }

    /// 查询指定城市未来逐 3 小时天气（数据源为 3 小时粒度）。
    #[tool(
        name = "get_hourly_weather",
        description = "查询中国城市未来天气的逐时点预报。重要：中国天气网公开数据只有 3 小时粒度，本工具返回时间窗内每 3 小时一个时点，绝不插值或推算。hours 取值 1-24，默认 12。每个时点含时间、天气、气温、风向、风力。"
    )]
    async fn get_hourly_weather(
        &self,
        Parameters(query): Parameters<HourlyQuery>,
    ) -> Result<Json<HourlyBundle>, ErrorData> {
        let hours = query.hours.unwrap_or(12).clamp(1, 24);
        let resolved = self.resolve_city(&query.city)?;
        let city = resolved.to_info();
        let mut bundle = self
            .provider
            .hourly_forecast(&city)
            .await
            .map_err(|err| err.to_error_data())?;

        let now = cst_now();
        let deadline = now + ChronoDuration::hours(i64::from(hours));
        let floor = now - ChronoDuration::hours(3);
        bundle
            .hours
            .retain(|point| point.time >= floor && point.time <= deadline);
        if bundle.hours.is_empty() {
            return Err(WeatherError::Unsupported(format!(
                "中国天气网当前预报窗口没有覆盖未来 {hours} 小时的逐 3 小时数据"
            ))
            .to_error_data());
        }
        let covered = bundle
            .hours
            .last()
            .map(|point| (point.time - now).num_hours().clamp(0, 24) as u32)
            .unwrap_or(0);
        bundle.requested_hours = hours;
        bundle.covered_hours = covered.max(3);
        Ok(Json(bundle))
    }

    /// 搜索城市并返回中国天气网城市代码。
    #[tool(
        name = "search_cities",
        description = "搜索中国城市，返回城市名称、所属行政区划（省/市/区县）、中国天气网 9 位城市代码与行政区划层级。query 可以是名称片段（南山、天河、苏州）或城市代码。用于把用户说的地名映射成可查询的城市与代码。"
    )]
    async fn search_cities(
        &self,
        Parameters(query): Parameters<SearchCitiesQuery>,
    ) -> Result<Json<SearchCitiesOutput>, ErrorData> {
        let limit = query.limit.unwrap_or(10).clamp(1, 50) as usize;

        let entries = if is_city_code(crate::city::normalize(&query.query).as_str()) {
            let code = crate::city::normalize(&query.query);
            self.index.get(&code).cloned().into_iter().collect()
        } else {
            self.index.search(&query.query, limit)
        };

        Ok(Json(SearchCitiesOutput {
            query: query.query,
            count: entries.len(),
            cities: entries.iter().map(entry_to_info).collect(),
            city_index: self.city_index_meta(),
        }))
    }

    /// 查询指定城市当前生效的气象预警。
    #[tool(
        name = "get_weather_alerts",
        description = "查询中国城市当前生效的气象预警。返回预警类型、级别、发布时间、失效时间、发布机构、正文与详情链接。has_alerts=false 表示查询成功且当前确实没有预警；查询失败会返回错误而不是空结果，两者不可混淆。"
    )]
    async fn get_weather_alerts(
        &self,
        Parameters(query): Parameters<AlertQuery>,
    ) -> Result<Json<AlertBundle>, ErrorData> {
        let resolved = self.resolve_city(&query.city)?;
        let city = resolved.to_info();
        let bundle = self
            .provider
            .alerts(&city)
            .await
            .map_err(|err| err.to_error_data())?;
        Ok(Json(bundle))
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
