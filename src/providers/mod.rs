//! 天气数据源抽象。
//!
//! 上层（MCP 工具）只依赖 [`WeatherProvider`]，新增公开数据源时实现该 trait 即可，
//! 不影响城市解析、缓存与协议层。

pub mod weather_com_cn;

use std::future::Future;
use std::pin::Pin;

use crate::error::Result;
use crate::models::{AlertBundle, CityInfo, ForecastBundle, HourlyBundle, WeatherObservation};

/// 装箱后的异步返回类型，用于让 trait 保持 dyn 兼容。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 统一天气数据源接口。
///
/// 所有实现都必须如实反映上游能力：上游没有的字段一律返回 `None`，
/// 上游不支持的查询返回 [`crate::error::WeatherError::Unsupported`]。
pub trait WeatherProvider: Send + Sync + 'static {
    /// 数据源标识（写入 `source.provider`）。
    fn id(&self) -> &'static str;

    /// 数据源主页，用于署名与合规说明。
    fn homepage(&self) -> &'static str;

    /// 查询实况观测。
    fn current_weather<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<WeatherObservation>>;

    /// 查询未来逐日预报（返回数据源能提供的全部天数，由调用方截断）。
    fn daily_forecast<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<ForecastBundle>>;

    /// 查询未来逐时（3 小时粒度）预报。
    fn hourly_forecast<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<HourlyBundle>>;

    /// 查询气象预警。
    fn alerts<'a>(&'a self, city: &'a CityInfo) -> BoxFut<'a, Result<AlertBundle>>;
}
