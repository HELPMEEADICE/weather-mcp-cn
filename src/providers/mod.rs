//! 天气数据源抽象。
//!
//! 上层（MCP 工具）只依赖 [`WeatherProvider`]，新增公开数据源时实现该 trait 即可，
//! 不影响城市解析、缓存与协议层。

pub mod weather_com_cn;

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, FixedOffset};

use crate::error::Result;
use crate::http::HttpClient;
use crate::models::{AlertBundle, CityInfo, ForecastBundle, HourlyBundle, WeatherObservation};

/// 装箱后的异步返回类型，用于让 trait 保持 dyn 兼容。
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 「一份上游数据 + 它真正的抓取时刻」。
///
/// 缓存的**值类型**只能长成这样：可共享的气象数据 + 抓取时间。
/// 与单次请求绑定的东西（用户输入的地名、解析出的 [`CityInfo`]、`resolution_note`）
/// 一律留在缓存外面，在每次调用返回时现场绑定 —— 这是本项目「缓存不污染请求上下文」
/// 的结构性保证：类型系统不允许把 `CityInfo` 塞进缓存。
#[derive(Debug, Clone)]
pub struct Fetched<T> {
    /// 解析后的上游数据（不含任何请求上下文）。
    pub data: T,
    /// 真正抓到这份数据的时刻（UTC+8）。
    pub fetched_at: DateTime<FixedOffset>,
}

impl<T> Fetched<T> {
    /// 记录一份刚抓到的数据。
    pub fn new(data: T, fetched_at: DateTime<FixedOffset>) -> Self {
        Self { data, fetched_at }
    }
}

/// 最小可替换的「取文本」上游接口。
///
/// 生产实现是 [`HttpClient`]；测试实现可以直接回放落盘的样本，从而在没有网络的情况下
/// 验证缓存命中、singleflight 与请求上下文绑定。
pub trait TextFetcher: Send + Sync {
    /// GET 一个上游地址并返回解码后的文本。
    fn get_text<'a>(&'a self, url: &'a str, referer: Option<&'a str>)
    -> BoxFut<'a, Result<String>>;
}

impl TextFetcher for HttpClient {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        referer: Option<&'a str>,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(HttpClient::get_text(self, url, referer))
    }
}

/// 统一天气数据源接口。
///
/// 所有实现都必须如实反映上游能力：上游没有的字段一律返回 `None`，
/// 上游不支持的查询返回 [`crate::error::WeatherError::Unsupported`]。
///
/// 实现方有义务保证「同一个城市代码的多次调用只共享可共享的数据」：
/// 返回值的 `city` 字段必须是**本次调用**传进来的 `city`，不能是缓存里第一次
/// 解析出来的那一份。
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
