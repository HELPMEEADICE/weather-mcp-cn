//! 解析器测试：全部使用从中国天气网抓取的真实响应样本，
//! 不依赖外部网络，也不会因为站点改版而随机失败（样本已落盘）。

use weather_mcp_cn::models::{CityInfo, CityLevel};
use weather_mcp_cn::providers::weather_com_cn::{
    cst_now, parse_alerts_payload, parse_current_js, parse_forecast_html, parse_hourly_html,
};

const CURRENT_SAMPLE: &str = include_str!("fixtures/current_sk_2d.txt");
const FORECAST_SAMPLE: &str = include_str!("fixtures/forecast_7d_101010100.html");
const HOURLY_SAMPLE: &str = include_str!("fixtures/hourly_1d_101010100.html");
/// 2026-11-06 抓取的**完整**逐 3 小时页面（67 KB 原始 HTML，含大量内联脚本）。
/// 用来验证 `hour3data` 的提取在真实体积/真实花括号密度下依然正确。
const HOURLY_FULL_PAGE: &str = include_str!("fixtures/hourly_1d_101010100_20261106.html");
const ALERT_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_101060101.txt");
/// 真实返回：`var alarmDZ101010100 ={"w":[]}`，前后还夹着 `cityDZ` 数据块。
const ALERT_EMPTY_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_empty_101010100.txt");
/// 结构异常样本：`w` 是 `null`，必须报错而不是当成「没有预警」。
const ALERT_NULL_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_null_w_101010100.txt");
/// 结构异常样本：数组被截断、字符串未闭合。
const ALERT_CORRUPT_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_corrupt_101060101.txt");

fn beijing() -> CityInfo {
    CityInfo {
        code: "101010100".to_string(),
        name: "北京".to_string(),
        province: Some("北京市".to_string()),
        city: Some("北京市".to_string()),
        district: None,
        display_name: "北京市".to_string(),
        level: Some(CityLevel::Municipality),
        resolution_note: None,
    }
}

#[test]
fn parses_current_observation_from_real_sample() {
    let payload = parse_current_js(CURRENT_SAMPLE).expect("真实实况样本应可解析");
    assert_eq!(payload.city_name.as_deref(), Some("北京"));
    let temp = payload.temperature_c.expect("样本里有气温");
    assert!((-60.0..60.0).contains(&temp), "气温应落在合理范围：{temp}");
    assert_eq!(payload.humidity_percent, Some(25.0));
    assert_eq!(payload.wind_direction.as_deref(), Some("东北风"));
    assert_eq!(payload.wind_scale.as_deref(), Some("1级"));
    assert_eq!(payload.weather.as_deref(), Some("晴"));
    assert_eq!(payload.pressure_hpa, Some(1021.0));
    assert_eq!(payload.visibility_km, Some(30.0));
    assert_eq!(payload.aqi, Some(16));

    let observed = payload.observed_at.expect("样本里有观测时间");
    // 数据源给的是北京时间，必须落在 UTC+8。
    assert_eq!(observed.offset().local_minus_utc(), 8 * 3600);
    assert_eq!(observed.format("%m-%d").to_string(), "11-05");
    assert_eq!(observed.format("%H:%M").to_string(), "23:05");
    assert!(payload.observed_at_note.is_some());
}

#[test]
fn current_observation_marks_absent_fields() {
    let payload = parse_current_js(CURRENT_SAMPLE).unwrap();
    let observation = payload.into_observation(
        beijing(),
        "https://d1.weather.com.cn/sk_2d/101010100.html".to_string(),
        cst_now(),
    );
    // 中国天气网实况接口没有体感温度与降水概率，不能凭空编造。
    assert!(observation.feels_like_c.is_none());
    assert!(observation.is_observation);
    assert!(
        observation
            .missing_fields
            .iter()
            .any(|field| field == "feels_like_c"),
        "缺失字段要显式列出：{:?}",
        observation.missing_fields
    );
    assert_eq!(observation.source.provider, "weather.com.cn");
    assert!(observation.source.url.contains("sk_2d"));
}

#[test]
fn rejects_current_payload_without_datask_block() {
    let err = parse_current_js("<html>结构变了</html>").unwrap_err();
    assert_eq!(err.code(), "parse_error");
}

#[test]
fn parses_seven_day_forecast_from_real_sample() {
    let bundle = parse_forecast_html(
        FORECAST_SAMPLE,
        beijing(),
        "https://www.weather.com.cn/weather/101010100.shtml".to_string(),
    )
    .expect("真实预报样本应可解析");

    assert!(bundle.available_days >= 7, "应解析出 7 天");
    assert_eq!(bundle.city.code, "101010100");

    let published = bundle.published_at.expect("样本里有预报发布时间");
    assert_eq!(published.offset().local_minus_utc(), 8 * 3600);
    assert_eq!(
        published.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-05 18:00"
    );

    let today = &bundle.days[0];
    assert_eq!(today.date, "2026-11-05");
    assert_eq!(today.label.as_deref(), Some("今天"));
    assert_eq!(today.weather.as_deref(), Some("晴"));
    // 当天已入夜，数据源只给最低温，最高温必须留空而不是编一个数字。
    assert_eq!(today.temperature_low_c, Some(0));
    assert_eq!(today.temperature_high_c, None);

    let tomorrow = &bundle.days[1];
    assert_eq!(tomorrow.date, "2026-11-06");
    assert_eq!(tomorrow.temperature_high_c, Some(14));
    assert_eq!(tomorrow.temperature_low_c, Some(2));
    assert_eq!(tomorrow.weather.as_deref(), Some("晴转多云"));
    assert_eq!(tomorrow.weather_day.as_deref(), Some("晴"));
    assert_eq!(tomorrow.weather_night.as_deref(), Some("多云"));
    assert_eq!(tomorrow.wind_direction_day.as_deref(), Some("南风"));
    assert_eq!(tomorrow.wind_direction_night.as_deref(), Some("东北风"));

    let third = &bundle.days[2];
    assert_eq!(third.weather.as_deref(), Some("小雨转雨夹雪"));
    assert_eq!(third.temperature_high_c, Some(5));
    assert_eq!(third.temperature_low_c, Some(0));
}

#[test]
fn predicts_weekday_and_splits_weather() {
    let bundle = parse_forecast_html(
        FORECAST_SAMPLE,
        beijing(),
        "https://www.weather.com.cn/weather/101010100.shtml".to_string(),
    )
    .unwrap();
    assert_eq!(bundle.days[1].weekday.as_deref(), Some("周五"));
    assert_eq!(bundle.days[3].weather_day.as_deref(), Some("雨夹雪"));
    assert_eq!(bundle.days[3].weather_night.as_deref(), Some("晴"));
    assert_eq!(bundle.days[3].temperature_low_c, Some(-1));
}

#[test]
fn reports_parse_error_when_forecast_html_changes() {
    let err = parse_forecast_html(
        "<html><body><p>页面改版了</p></body></html>",
        beijing(),
        "https://www.weather.com.cn/weather/101010100.shtml".to_string(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "parse_error");
    assert!(err.to_string().contains("页面结构可能已变化"));
}

#[test]
fn parses_three_hourly_forecast_from_real_sample() {
    let bundle = parse_hourly_html(
        HOURLY_SAMPLE,
        beijing(),
        "https://www.weather.com.cn/weather1d/101010100.shtml".to_string(),
    )
    .expect("真实逐三小时样本应可解析");

    assert_eq!(bundle.resolution_hours, 3, "数据源粒度就是 3 小时");
    assert!(bundle.hours.len() >= 8, "至少覆盖 24 小时");
    let published = bundle.published_at.expect("样本里有发布时间");
    assert_eq!(
        published.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-05 18:00"
    );

    let first = &bundle.hours[0];
    assert_eq!(
        first.time.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-05 23:00"
    );
    assert_eq!(first.temperature_c, Some(5));
    assert_eq!(first.weather.as_deref(), Some("晴"));
    assert_eq!(first.wind_direction.as_deref(), Some("东北风"));
    assert_eq!(first.icon_code.as_deref(), Some("n00"));

    // 时点必须严格递增且间隔 3 小时。
    for pair in bundle.hours.windows(2) {
        let gap = pair[1].time - pair[0].time;
        assert_eq!(gap.num_hours(), 3, "逐三小时数据不允许被插值");
    }
}

#[test]
fn reports_parse_error_when_hourly_block_missing() {
    let err = parse_hourly_html(
        "<html><body>没有内联数据了</body></html>",
        beijing(),
        "https://www.weather.com.cn/weather1d/101010100.shtml".to_string(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "parse_error");
}

#[test]
fn parses_alerts_from_real_sample() {
    let alerts = parse_alerts_payload(ALERT_SAMPLE)
        .expect("真实预警样本应可解析")
        .alerts;
    assert!(!alerts.is_empty(), "吉林样本里应当有生效中的预警");

    let first = &alerts[0];
    assert!(!first.title.is_empty());
    assert!(first.content.contains("预警"));
    assert!(!first.region.is_empty());
    let published = first.published_at.expect("预警应有发布时间");
    assert_eq!(published.offset().local_minus_utc(), 8 * 3600);
    assert!(
        first.publisher.is_some(),
        "正文里带有发布机构，应能提取：{}",
        first.content
    );
}

/// 真实返回的空预警：`cityDZ` 数据块在前，`alarmDZ...={"w":[]}` 在后。
#[test]
fn real_empty_alert_response_means_no_alerts() {
    let payload = parse_alerts_payload(ALERT_EMPTY_SAMPLE).expect("空预警是成功响应");
    assert!(payload.alerts.is_empty());
}

/// `w:null` 绝不能当成「没有预警」。
#[test]
fn null_alert_field_is_a_parse_error() {
    let err = parse_alerts_payload(ALERT_NULL_SAMPLE).expect_err("w=null 必须报错");
    assert_eq!(err.code(), "parse_error");
    assert!(err.to_string().contains("null"), "{err}");
}

/// 截断/损坏的预警 JSON 必须报错，而不是丢掉整条预警当作「无预警」。
#[test]
fn corrupt_alert_response_is_a_parse_error() {
    let err = parse_alerts_payload(ALERT_CORRUPT_SAMPLE).expect_err("损坏 JSON 必须报错");
    assert_eq!(err.code(), "parse_error");
}

#[test]
fn empty_alert_list_is_not_an_error() {
    let alerts = parse_alerts_payload(r#"var alarmDZ101010100 ={"w":[]}"#)
        .expect("空数组是成功响应")
        .alerts;
    assert!(alerts.is_empty());
}

#[test]
fn alerts_without_block_report_parse_error() {
    let err = parse_alerts_payload("<html>接口改版</html>").unwrap_err();
    assert_eq!(err.code(), "parse_error");
}

/// 完整的真实页面（67 KB，含大量内联脚本）也必须能被正确提取。
#[test]
fn parses_full_real_hourly_page() {
    let bundle = parse_hourly_html(
        HOURLY_FULL_PAGE,
        beijing(),
        "https://www.weather.com.cn/weather1d/101010100.shtml".to_string(),
    )
    .expect("完整真实页面应可解析");

    assert_eq!(bundle.resolution_hours, 3);
    assert_eq!(
        bundle
            .published_at
            .expect("页面里有发布时间")
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        "2026-11-06 07:30"
    );
    assert_eq!(bundle.hours.len(), 8, "`1d` 给 8 个时点");
    assert_eq!(
        bundle.hours[0].time.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-06 11:00"
    );
    for pair in bundle.hours.windows(2) {
        assert_eq!((pair[1].time - pair[0].time).num_hours(), 3);
    }
    // 未按请求窗口裁剪：requested_hours 为 0，covered_hours 是上游总跨度。
    assert_eq!(bundle.requested_hours, 0);
    assert_eq!(bundle.covered_hours, 24);
    assert_eq!(
        bundle.source.fetched_at.offset().local_minus_utc(),
        8 * 3600,
        "抓取时间必须是 UTC+8"
    );
    assert!(bundle.source.url.ends_with("weather1d/101010100.shtml"));
}
