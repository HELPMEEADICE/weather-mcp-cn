//! 缓存一致性集成测试。
//!
//! 这里驱动的是**真实的** [`WeatherComCn`]（含真实的 LRU+TTL 缓存与 singleflight），
//! 只有「取文本」这一层换成回放落盘样本的假上游。因此验证的是生产代码路径：
//!
//! * 缓存里只有可共享的气象数据，城市上下文按每次调用重新绑定（不回退说明互相继承、
//!   也不丢失应有的回退说明）；
//! * 缓存命中不再打上游，且 `source.fetched_at` 保持第一次真正抓取的时间；
//! * 同一键的并发未命中只打一次上游（singleflight）；
//! * 上游失败不进缓存。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use weather_mcp_cn::config::Config;
use weather_mcp_cn::error::{Result as WeatherResult, WeatherError};
use weather_mcp_cn::models::{CityInfo, CityLevel};
use weather_mcp_cn::providers::weather_com_cn::WeatherComCn;
use weather_mcp_cn::providers::{BoxFut, TextFetcher, WeatherProvider};

const CURRENT_SAMPLE: &str = include_str!("fixtures/current_sk_2d.txt");
const FORECAST_SAMPLE: &str = include_str!("fixtures/forecast_7d_101010100.html");
const HOURLY_SAMPLE: &str = include_str!("fixtures/hourly_1d_101010100.html");
const ALERT_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_101060101.txt");

const NOTE: &str = "「粤海街道」在中国天气网城市库中没有独立站点，已使用所属行政区「深圳市」的数据";

/// 回放固定样本的假上游。
struct Replay {
    delay: Duration,
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl Replay {
    fn instant() -> Self {
        Self {
            delay: Duration::ZERO,
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn slow(delay: Duration) -> Self {
        Self {
            delay,
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn sample_for(url: &str) -> Option<&'static str> {
    if url.contains("/sk_2d/") {
        Some(CURRENT_SAMPLE)
    } else if url.contains("/dingzhi/") {
        Some(ALERT_SAMPLE)
    } else if url.contains("/weather1d/") {
        Some(HOURLY_SAMPLE)
    } else if url.contains("/weather/") {
        Some(FORECAST_SAMPLE)
    } else {
        None
    }
}

impl TextFetcher for Replay {
    fn get_text<'a>(
        &'a self,
        url: &'a str,
        _referer: Option<&'a str>,
    ) -> BoxFut<'a, WeatherResult<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail.load(Ordering::SeqCst);
        let delay = self.delay;
        let body = sample_for(url).map(str::to_string);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if fail {
                return Err(WeatherError::Timeout("假上游故意超时".into()));
            }
            body.ok_or_else(|| WeatherError::Parse(format!("测试没有准备 {url} 的样本")))
        })
    }
}

fn build_provider<F>(fetcher: Arc<F>) -> WeatherComCn
where
    F: TextFetcher + 'static,
{
    let config = Config {
        max_retries: 0,
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    WeatherComCn::with_fetcher(&config, fetcher)
}

fn city(note: Option<&str>) -> CityInfo {
    CityInfo {
        code: "101280601".into(),
        name: "深圳".into(),
        province: Some("广东省".into()),
        city: Some("深圳市".into()),
        district: None,
        display_name: "广东省深圳市".into(),
        level: Some(CityLevel::City),
        resolution_note: note.map(str::to_string),
    }
}

/// 四种缓存都必须只缓存气象数据：回退说明要么随请求出现，要么随请求消失。
#[tokio::test]
async fn city_context_is_bound_per_request_in_every_cache() {
    let replay = Arc::new(Replay::instant());
    let provider = build_provider(Arc::clone(&replay));

    let with_note = city(Some(NOTE));
    let plain = city(None);

    // 第一轮：带回退说明（模拟查询「深圳市粤海街道」）
    let current_with = provider.current_weather(&with_note).await.expect("实况");
    let forecast_with = provider.daily_forecast(&with_note).await.expect("预报");
    let hourly_with = provider
        .hourly_forecast(&with_note)
        .await
        .expect("逐 3 小时");
    let alerts_with = provider.alerts(&with_note).await.expect("预警");

    // 第二轮：同一城市代码，不带说明（模拟查询「深圳」）
    let current_plain = provider.current_weather(&plain).await.expect("实况");
    let forecast_plain = provider.daily_forecast(&plain).await.expect("预报");
    let hourly_plain = provider.hourly_forecast(&plain).await.expect("逐 3 小时");
    let alerts_plain = provider.alerts(&plain).await.expect("预警");

    assert_eq!(current_with.city.resolution_note.as_deref(), Some(NOTE));
    assert_eq!(forecast_with.city.resolution_note.as_deref(), Some(NOTE));
    assert_eq!(hourly_with.city.resolution_note.as_deref(), Some(NOTE));
    assert_eq!(alerts_with.city.resolution_note.as_deref(), Some(NOTE));

    for (label, note) in [
        ("实况", current_plain.city.resolution_note.as_ref()),
        ("预报", forecast_plain.city.resolution_note.as_ref()),
        ("逐 3 小时", hourly_plain.city.resolution_note.as_ref()),
        ("预警", alerts_plain.city.resolution_note.as_ref()),
    ] {
        assert!(
            note.is_none(),
            "{label}：第二次查询不能继承上一次的回退说明：{note:?}"
        );
    }

    // 两轮都是同一城市代码，命中缓存，上游只该被打 4 次（每类缓存各一次）。
    assert_eq!(replay.calls(), 4, "四类数据各打一次上游后应全部命中缓存");

    // 缓存命中时抓取时间必须保持第一次的值，URL 也不能变。
    assert_eq!(
        current_with.source.fetched_at,
        current_plain.source.fetched_at
    );
    assert_eq!(
        forecast_with.source.fetched_at,
        forecast_plain.source.fetched_at
    );
    assert_eq!(
        hourly_with.source.fetched_at,
        hourly_plain.source.fetched_at
    );
    assert_eq!(
        alerts_with.source.fetched_at,
        alerts_plain.source.fetched_at
    );
    assert_eq!(current_with.source.url, current_plain.source.url);
    assert_eq!(forecast_with.source.url, forecast_plain.source.url);
    assert_eq!(hourly_with.source.url, hourly_plain.source.url);
    assert_eq!(alerts_with.source.url, alerts_plain.source.url);
}

/// 反过来：先普通查询、再带说明查询，说明必须出现（不能「第一次查了什么就永久格式化」）。
#[tokio::test]
async fn fallback_note_appears_when_a_later_request_needs_it() {
    let replay = Arc::new(Replay::instant());
    let provider = build_provider(Arc::clone(&replay));

    let plain = provider.daily_forecast(&city(None)).await.expect("预报");
    assert!(plain.city.resolution_note.is_none());

    let with_note = provider
        .daily_forecast(&city(Some(NOTE)))
        .await
        .expect("预报");
    assert_eq!(with_note.city.resolution_note.as_deref(), Some(NOTE));
    assert_eq!(replay.calls(), 1, "第二次应命中缓存");
    assert_eq!(plain.days.len(), with_note.days.len());
}

/// 缓存命中时，真正从上游拿到的气象字段必须原样保留。
#[tokio::test]
async fn cache_hit_preserves_upstream_values() {
    let replay = Arc::new(Replay::instant());
    let provider = build_provider(Arc::clone(&replay));

    let first = provider.current_weather(&city(None)).await.expect("实况");
    let second = provider.current_weather(&city(None)).await.expect("实况");

    assert_eq!(replay.calls(), 1);
    assert_eq!(first.temperature_c, second.temperature_c);
    assert_eq!(
        first.observed_at, second.observed_at,
        "观测时间是数据自身的属性"
    );
    assert_eq!(first.humidity_percent, second.humidity_percent);
    assert_eq!(first.missing_fields, second.missing_fields);
    assert!(first.is_observation && second.is_observation);
}

/// 不同城市代码必须使用不同的缓存条目。
#[tokio::test]
async fn different_codes_do_not_share_cache_entries() {
    let replay = Arc::new(Replay::instant());
    let provider = build_provider(Arc::clone(&replay));

    let mut shenzhen = city(None);
    let mut beijing = city(None);
    beijing.code = "101010100".into();

    provider.current_weather(&shenzhen).await.expect("深圳实况");
    provider.current_weather(&beijing).await.expect("北京实况");
    assert_eq!(replay.calls(), 2);

    // 再各查一次，全部命中。
    provider.current_weather(&shenzhen).await.expect("深圳实况");
    provider.current_weather(&beijing).await.expect("北京实况");
    assert_eq!(replay.calls(), 2, "两个城市都应在缓存里");
    shenzhen.code = "101280601".into();
}

/// 同一键的并发未命中只打一次上游。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_misses_are_coalesced_into_one_request() {
    let replay = Arc::new(Replay::slow(Duration::from_millis(60)));
    let provider = Arc::new(build_provider(Arc::clone(&replay)));

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let provider = Arc::clone(&provider);
        tasks.push(tokio::spawn(async move {
            provider.current_weather(&city(None)).await.is_ok()
        }));
    }
    for task in tasks {
        assert!(task.await.expect("任务不应 panic"), "并发查询都应成功");
    }
    assert_eq!(
        replay.calls(),
        1,
        "4 个并发未命中应被合并成 1 次上游请求，实际 {} 次",
        replay.calls()
    );
}

/// 失败结果不进缓存：上游恢复后下一次调用必须重新请求并成功。
#[tokio::test]
async fn failure_is_not_cached() {
    let replay = Arc::new(Replay::instant());
    replay.fail.store(true, Ordering::SeqCst);
    let provider = build_provider(Arc::clone(&replay));

    let err = provider
        .current_weather(&city(None))
        .await
        .expect_err("上游失败必须报错");
    assert_eq!(err.code(), "timeout");

    replay.fail.store(false, Ordering::SeqCst);
    let observation = provider
        .current_weather(&city(None))
        .await
        .expect("上游恢复后应成功");
    assert!(observation.temperature_c.is_some());
    assert_eq!(replay.calls(), 2, "失败那次不能被缓存");
}
