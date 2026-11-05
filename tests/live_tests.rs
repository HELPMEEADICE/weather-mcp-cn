//! 联网集成测试：真的去访问中国天气网，验证数据源可用、返回的是当前有效数据。
//!
//! 这些用例默认被 `#[ignore]` 跳过，因为 `cargo test` 不应该依赖外部网络的稳定性。
//! 需要联网验证时显式执行：
//!
//! ```bash
//! cargo test --test live_tests -- --ignored --test-threads=1 --nocapture
//! ```

use std::time::Duration;

use weather_mcp_cn::city::CityIndex;
use weather_mcp_cn::config::Config;
use weather_mcp_cn::error::WeatherError;
use weather_mcp_cn::http::HttpClient;
use weather_mcp_cn::providers::WeatherProvider;
use weather_mcp_cn::providers::weather_com_cn::WeatherComCn;

fn index() -> CityIndex {
    CityIndex::from_json(weather_mcp_cn::EMBEDDED_CITIES_JSON).expect("城市库可解析")
}

fn provider() -> WeatherComCn {
    let config = Config {
        request_timeout: Duration::from_secs(20),
        max_retries: 1,
        ..Default::default()
    };
    WeatherComCn::new(&config).expect("构建数据源")
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn beijing_current_weather_is_live() {
    let provider = provider();
    let city = index().resolve("北京").unwrap().to_info();
    let observation = provider
        .current_weather(&city)
        .await
        .expect("北京实况应可获取");

    println!(
        "北京实况：{:?}℃ {} 湿度 {:?} 风 {:?}{:?} 观测时间 {:?} 来源 {}",
        observation.temperature_c,
        observation.weather.as_deref().unwrap_or("?"),
        observation.humidity_percent,
        observation.wind_direction,
        observation.wind_scale,
        observation.observed_at,
        observation.source.url
    );

    let temperature = observation.temperature_c.expect("实况必须有气温");
    assert!(
        (-60.0..60.0).contains(&temperature),
        "气温应在合理区间：{temperature}"
    );
    assert!(observation.weather.is_some(), "实况必须有天气现象");
    assert!(observation.is_observation);
    assert!(observation.source.url.contains("sk_2d"));
    assert!(
        observation.source.url.contains("101010100"),
        "数据来源 URL 应指向北京"
    );

    let observed = observation.observed_at.expect("必须带观测时间");
    let now = weather_mcp_cn::providers::weather_com_cn::cst_now();
    let age = (now - observed).num_hours();
    assert!(
        (-6..=6).contains(&age),
        "实况观测时间应接近当前时间，实际相差 {age} 小时（观测 {observed}，现在 {now}）"
    );
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn shanghai_forecast_three_days_is_live() {
    let provider = provider();
    let city = index().resolve("上海").unwrap().to_info();
    let bundle = provider
        .daily_forecast(&city)
        .await
        .expect("上海预报应可获取");

    assert!(bundle.days.len() >= 3, "至少要有 3 天");
    for day in bundle.days.iter().take(3) {
        println!(
            "上海 {} {} 高 {:?} 低 {:?} 风 {:?}",
            day.date,
            day.weather.as_deref().unwrap_or("?"),
            day.temperature_high_c,
            day.temperature_low_c,
            day.wind_scale
        );
        assert!(day.temperature_high_c.is_some() || day.temperature_low_c.is_some());
    }
    assert!(bundle.published_at.is_some(), "应有预报发布时间");
    assert!(bundle.source.url.contains("101020100"));
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn guangzhou_forecast_seven_days_is_live() {
    let provider = provider();
    let city = index().resolve("广州").unwrap().to_info();
    let bundle = provider
        .daily_forecast(&city)
        .await
        .expect("广州预报应可获取");
    assert!(bundle.available_days >= 7, "应能拿到 7 天");
    let mut dates: Vec<&str> = bundle.days.iter().map(|day| day.date.as_str()).collect();
    dates.dedup();
    assert_eq!(dates.len(), bundle.days.len(), "日期不应重复");
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn wuhan_and_shenzhen_resolution_is_live() {
    let index = index();
    assert_eq!(index.resolve("武汉").unwrap().code, "101200101");
    let nanshan = index.resolve("深圳市南山区").unwrap();
    assert_eq!(nanshan.code, "101280604");
    println!("深圳市南山区 → {}", nanshan.to_info().display_name);
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn alerts_are_reported_or_empty_explicitly() {
    let provider = provider();
    for name in ["北京", "长春", "乌鲁木齐"] {
        let city = index().resolve(name).unwrap().to_info();
        let bundle = provider.alerts(&city).await.expect("预警应可查询");
        println!(
            "{name}: has_alerts={} count={} status={}",
            bundle.has_alerts,
            bundle.alerts.len(),
            bundle.status
        );
        assert_eq!(bundle.has_alerts, !bundle.alerts.is_empty());
        assert!(bundle.status == "active_alerts_found" || bundle.status == "no_active_alerts");
    }
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn hourly_data_is_three_hourly_and_live() {
    let provider = provider();
    let city = index().resolve("北京").unwrap().to_info();
    let bundle = provider
        .hourly_forecast(&city)
        .await
        .expect("逐三小时数据应可获取");
    assert_eq!(bundle.resolution_hours, 3);
    assert!(!bundle.hours.is_empty());
    for pair in bundle.hours.windows(2) {
        assert_eq!((pair[1].time - pair[0].time).num_hours(), 3);
    }
    println!("北京逐 3 小时预报 {} 个时点", bundle.hours.len());
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn nonexistent_city_fails_before_network() {
    let index = index();
    let err = index.resolve("这个地方不存在啊").unwrap_err();
    assert_eq!(err.code(), "city_not_found");
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn invalid_city_code_is_reported_by_upstream() {
    let provider = provider();
    let index = index();
    let resolved = index.resolve("101999999").expect("格式合法即放行");
    assert!(resolved.entry.is_none());
    let err = provider
        .current_weather(&resolved.to_info())
        .await
        .expect_err("不存在的代码上游应返回错误");
    println!("上游错误：{err}");
    assert!(
        matches!(
            err,
            WeatherError::UpstreamStatus { .. }
                | WeatherError::Parse(_)
                | WeatherError::Upstream(_)
        ),
        "应报告上游错误而不是给出伪造数据：{err}"
    );
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn timeout_is_reported_not_swallowed() {
    let config = Config {
        request_timeout: Duration::from_millis(1),
        connect_timeout: Duration::from_millis(1),
        max_retries: 0,
        ..Default::default()
    };
    let provider = WeatherComCn::new(&config).expect("构建数据源");
    let city = index().resolve("北京").unwrap().to_info();
    let err = provider
        .current_weather(&city)
        .await
        .expect_err("超时应报错");
    println!("超时错误：{err} / code={}", err.code());
    assert!(
        matches!(err, WeatherError::Timeout(_) | WeatherError::Upstream(_)),
        "应报告超时类错误：{err}"
    );
}

#[tokio::test]
#[ignore = "需要联网访问中国天气网"]
async fn city_index_source_is_reachable() {
    let config = Config::default();
    let http = HttpClient::new(&config).expect("HTTP 客户端");
    let body = http
        .get_text(
            weather_mcp_cn::citygen::CITY_JS,
            Some("https://www.weather.com.cn/"),
        )
        .await
        .expect("城市库数据源应可访问");
    assert!(body.contains("AREAID"), "城市库数据源结构变了");
    assert!(body.contains("101280604"), "应包含深圳南山");
}
