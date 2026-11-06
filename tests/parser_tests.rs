//! 解析器测试：全部使用从中国天气网抓取的真实响应样本，
//! 不依赖外部网络，也不会因为站点改版而随机失败（样本已落盘）。

use weather_mcp_cn::models::{CityInfo, CityLevel};
use weather_mcp_cn::providers::weather_com_cn::{
    MAX_HOURLY_POINTS, build_cst, cst_now, parse_alerts_payload, parse_current_js,
    parse_forecast_html, parse_hourly_html, parse_hourly_page,
};
use weather_mcp_cn::window;

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
    // `1d` 只给 8 个时点，但页面同时给了 `23d`（第 2~3 天）与 `7d`（7 天全集）。
    // 合并后一直排到内存上限 `MAX_HOURLY_POINTS` 个时点，而不是只剩 1d 的 8 个。
    assert_eq!(
        bundle.hours.len(),
        MAX_HOURLY_POINTS,
        "1d+23d+7d 合并后受上限约束"
    );
    assert_eq!(
        bundle.hours[0].time.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-06 11:00"
    );
    assert_eq!(
        bundle
            .hours
            .last()
            .expect("最后一个时点")
            .time
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        "2026-11-10 08:00",
        "06日11时 + 31×3 小时"
    );
    for pair in bundle.hours.windows(2) {
        assert_eq!((pair[1].time - pair[0].time).num_hours(), 3);
    }
    // 未按请求窗口裁剪：requested_hours 为 0，covered_hours 是上游总跨度。
    assert_eq!(bundle.requested_hours, 0);
    assert_eq!(bundle.covered_hours, 96, "32 个时点 × 3 小时");
    assert_eq!(
        bundle.source.fetched_at.offset().local_minus_utc(),
        8 * 3600,
        "抓取时间必须是 UTC+8"
    );
    assert!(bundle.source.url.ends_with("weather1d/101010100.shtml"));
}

// ---------------------------------------------------------------------------
// 逐 3 小时预报的 `1d` / `23d` / `7d` 合并
// ---------------------------------------------------------------------------

/// 用真实页面的发布时间与 `hour3data` 内容拼一个最小页面。
fn hourly_page(published: &str, hour3data: &str) -> String {
    format!(
        r#"<html><body>
<input type="hidden" id="fc_3h_internal_update_time" value="{published}"/>
<script>var hour3data={hour3data};</script>
</body></html>"#
    )
}

fn times_of(points: &[weather_mcp_cn::models::HourlyForecast]) -> Vec<String> {
    points
        .iter()
        .map(|point| point.time.format("%d日%H:%M").to_string())
        .collect()
}

/// 2026-11-06 的 `1d`：8 个时点，覆盖 06日11时 ~ 07日08时。
const DAY_ONE_8_POINTS: &str = r#"["06日11时,d00,晴,12℃,西风,<3级,1","06日14时,d00,晴,13℃,东南风,<3级,1","06日17时,d00,晴,13℃,东南风,<3级,1","06日20时,n00,晴,6℃,东南风,<3级,0","06日23时,n01,多云,4℃,东风,<3级,0","07日02时,n01,多云,4℃,东北风,<3级,0","07日05时,n01,多云,2℃,东北风,<3级,0","07日08时,d01,多云,3℃,东北风,<3级,2"]"#;

/// 回归（核心缺陷）：`1d` 已经有 8 个时点，仍然必须合并 `23d` 的后续时点。
///
/// 旧实现用 `if points.len() < 8` 决定要不要读 `23d` / `7d`，于是这里只会返回 8 个点，
/// `07日11时`、`07日14时` 被丢掉。
#[test]
fn merges_23d_even_when_1d_already_has_eight_points() {
    let html = hourly_page(
        "2026-11-06 07:30",
        &format!(
            r#"{{"1d":{DAY_ONE_8_POINTS},"23d":[["07日08时,d01,多云,3℃,东北风,<3级,2","07日11时,d01,多云,5℃,东北风,<3级,3","07日14时,d07,小雨,4℃,北风,<3级,3","07日17时,d07,小雨,2℃,东北风,<3级,3"]]}}"#
        ),
    );

    let payload = parse_hourly_page(&html).expect("应可解析");
    let times = times_of(&payload.points);
    assert_eq!(
        times,
        vec![
            "06日11:00",
            "06日14:00",
            "06日17:00",
            "06日20:00",
            "06日23:00",
            "07日02:00",
            "07日05:00",
            "07日08:00",
            "07日11:00",
            "07日14:00",
            "07日17:00",
        ],
        "1d 的 8 个点之后必须继续接上 23d 的后续时点"
    );
    assert_eq!(
        payload.points[8].time,
        build_cst(2026, 11, 7, 11, 0).expect("合法时间")
    );
    assert_eq!(payload.points[8].temperature_c, Some(5));
    assert_eq!(payload.points[8].weather.as_deref(), Some("多云"));
}

/// 在真实页面上，请求 1 / 3 / 12 / 24 小时都必须被完整覆盖。
///
/// 上游已经提供了足够的数据（`1d` + `23d` 合起来 40 个小时），窗口裁剪不应该
/// 因为「只读 `1d`」而少算任何一个长度。
#[test]
fn real_page_fully_covers_one_three_twelve_and_twenty_four_hours() {
    let payload = parse_hourly_page(HOURLY_FULL_PAGE).expect("真实页面应可解析");
    let now = build_cst(2026, 11, 6, 14, 0).expect("合法时间");

    for hours in [1i64, 3, 12, 24] {
        let selection = window::select(&payload.points, now, hours).expect("窗口应被覆盖");
        assert_eq!(
            selection.covered_hours, hours as u32,
            "请求 {hours} 小时时应当被完整覆盖"
        );
        assert_eq!(
            selection.contiguous_hours, hours as u32,
            "请求 {hours} 小时时应当是连续覆盖"
        );
        assert_eq!(
            selection.coverage_end,
            now + chrono::Duration::hours(hours),
            "请求 {hours} 小时时覆盖必须正好到截止时刻"
        );
        // 保留的时点必须是上游真实给的那些，不插值、不补点。
        for point in &selection.points {
            assert!(
                payload
                    .points
                    .iter()
                    .any(|source| source.time == point.time),
                "时点 {:?} 不是上游给出的时点",
                point.time
            );
        }
    }
}

/// 用完整真实页面复现「旧实现截断窗口、新实现覆盖满 24 小时」。
///
/// 真实数据（2026-11-06 北京）：
/// * `1d` 覆盖 06日11时（区间到 14:00）~ 07日08时（区间到 11:00）；
/// * `23d` 里还有 07日11时、07日14时……
///
/// 在 06日14时 请求未来 24 小时，截止时刻是 07日14时。旧实现因为 `1d` 满 8 个点
/// 就跳过 `23d`，只能覆盖 21 小时。
#[test]
fn full_real_page_covers_a_full_day_window_after_merge() {
    let payload = parse_hourly_page(HOURLY_FULL_PAGE).expect("真实页面应可解析");
    let now = build_cst(2026, 11, 6, 14, 0).expect("合法时间");
    let deadline = build_cst(2026, 11, 7, 14, 0).expect("合法时间");

    let selection = window::select(&payload.points, now, 24).expect("窗口应被覆盖");
    assert_eq!(selection.covered_hours, 24, "必须能凑满 24 小时");
    assert_eq!(selection.contiguous_hours, 24, "而且必须连续");
    assert_eq!(selection.coverage_end, deadline);

    let times = times_of(&selection.points);
    assert!(times.contains(&"07日11:00".to_string()), "{times:?}");
    assert!(
        times.contains(&"07日14:00".to_string()),
        "截止时刻的时点也要作为边界保留：{times:?}"
    );
    assert_eq!(times.last().map(String::as_str), Some("07日14:00"));

    // 只用 `1d` 的那 8 个点（= 旧实现实际拿到的数据）复现旧行为：只能覆盖 21 小时。
    let one_day_only: Vec<_> = payload
        .points
        .iter()
        .filter(|point| point.time <= build_cst(2026, 11, 7, 8, 0).expect("合法时间"))
        .cloned()
        .collect();
    assert_eq!(one_day_only.len(), 8, "`1d` 恰好 8 个时点");
    let old = window::select(&one_day_only, now, 24).expect("仍有部分覆盖");
    assert_eq!(
        old.covered_hours, 21,
        "旧实现会把 24 小时窗口截断到 21 小时"
    );
    assert!(old.contiguous_hours < 24);
    assert!(old.coverage_end < deadline);
}

/// 同一时刻同时出现在 `1d` / `23d` / `7d` 里时，取优先级最高的那一份（`1d`），
/// 旧预报不能覆盖更可信的新预报。
#[test]
fn prefers_1d_then_23d_then_7d_for_duplicate_instants() {
    let html = hourly_page(
        "2026-11-06 07:30",
        // 三个字段都带 06日11时，数据互相冲突；23d 与 7d 还共享 07日11时。
        r#"{"1d":["06日11时,d00,晴,12℃,西风,<3级,1"],
            "23d":[["06日11时,n09,暴雨,-30℃,北风,9级,9","07日11时,d01,多云,5℃,东北风,<3级,3"]],
            "7d":[["06日11时,d07,小雨,1℃,南风,3级,3","07日11时,d99,雷阵雨,-9℃,西风,8级,9","08日11时,d00,晴,9℃,西风,<3级,1"]]}"#,
    );

    let payload = parse_hourly_page(&html).expect("应可解析");
    let times = times_of(&payload.points);
    assert_eq!(
        times,
        vec!["06日11:00", "07日11:00", "08日11:00"],
        "同一时刻只能出现一次"
    );

    let first = &payload.points[0];
    assert_eq!(first.weather.as_deref(), Some("晴"), "`1d` 优先");
    assert_eq!(first.temperature_c, Some(12));
    assert_eq!(first.icon_code.as_deref(), Some("d00"));

    let second = &payload.points[1];
    assert_eq!(second.weather.as_deref(), Some("多云"), "`23d` 优先于 `7d`");
    assert_eq!(second.temperature_c, Some(5));

    let third = &payload.points[2];
    assert_eq!(third.weather.as_deref(), Some("晴"), "只剩 `7d` 时用它补齐");
}

/// 输入乱序（字段内部与字段之间都不按时间排）时，输出必须严格升序。
#[test]
fn sorts_unordered_fields_into_one_chronological_series() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"7d":[["07日14时,d00,晴,9℃,西风,<3级,1","06日11时,d00,晴,12℃,西风,<3级,1"]],
            "23d":[["06日20时,d00,晴,6℃,西风,<3级,1"]],
            "1d":["06日17时,d00,晴,8℃,西风,<3级,1","06日14时,d00,晴,10℃,西风,<3级,1"]}"#,
    );

    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(
        times_of(&payload.points),
        vec![
            "06日11:00",
            "06日14:00",
            "06日17:00",
            "06日20:00",
            "07日14:00"
        ]
    );
}

/// 跨月：11-30 的页面里，`01日` 属于 12 月；跨年：12-31 的页面里，`01日` 属于次年。
#[test]
fn infers_month_and_year_across_the_boundary() {
    let month_edge = hourly_page(
        "2026-11-30 08:00",
        r#"{"1d":["30日11时,d00,晴,3℃,西风,<3级,1","30日14时,d00,晴,4℃,西风,<3级,1","30日17时,d00,晴,2℃,西风,<3级,1","30日20时,n00,晴,0℃,西风,<3级,1","30日23时,n00,晴,-1℃,西风,<3级,1","01日02时,n00,晴,-2℃,西风,<3级,1","01日05时,n00,晴,-3℃,西风,<3级,1","01日08时,d00,晴,-1℃,西风,<3级,1"]}"#,
    );
    let payload = parse_hourly_page(&month_edge).expect("应可解析");
    assert_eq!(payload.points.len(), 8);
    assert_eq!(
        payload.points[5].time,
        build_cst(2026, 12, 1, 2, 0).unwrap()
    );
    assert_eq!(
        payload.points[7].time,
        build_cst(2026, 12, 1, 8, 0).unwrap()
    );

    let year_edge = hourly_page(
        "2026-12-31 08:00",
        r#"{"1d":["31日11时,d00,晴,3℃,西风,<3级,1","31日14时,d00,晴,4℃,西风,<3级,1","31日17时,d00,晴,2℃,西风,<3级,1","31日20时,n00,晴,0℃,西风,<3级,1","31日23时,n00,晴,-1℃,西风,<3级,1","01日02时,n00,晴,-2℃,西风,<3级,1","01日05时,n00,晴,-3℃,西风,<3级,1","01日08时,d00,晴,-1℃,西风,<3级,1"]}"#,
    );
    let payload = parse_hourly_page(&year_edge).expect("应可解析");
    assert_eq!(
        payload.points[4].time,
        build_cst(2026, 12, 31, 23, 0).unwrap()
    );
    assert_eq!(payload.points[5].time, build_cst(2027, 1, 1, 2, 0).unwrap());
    assert_eq!(payload.points[7].time, build_cst(2027, 1, 1, 8, 0).unwrap());
}

/// 页面已刷新但内联数据还没跟上（`1d` 首个时点早于发布时间）时，不能把时点算到下个月。
#[test]
fn keeps_points_earlier_than_the_publish_time_in_the_same_month() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["05日23时,n00,晴,5℃,东北风,<3级,0","06日02时,n00,晴,2℃,东南风,<3级,0","06日05时,n00,晴,1℃,西风,<3级,0","06日08时,d00,晴,2℃,西风,<3级,1"]}"#,
    );
    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(
        payload.points[0].time,
        build_cst(2026, 11, 5, 23, 0).unwrap()
    );
    for point in &payload.points {
        assert_eq!(
            point.time.format("%Y-%m").to_string(),
            "2026-11",
            "不能因为日号更小就跳到下个月：{}",
            point.time
        );
    }
}

/// 结构异常 / 缺字段 / 非法时点的条目必须被丢弃，且不影响同页其它有效时点。
#[test]
fn drops_unparsable_entries_without_inventing_data() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,d00,晴,12℃,西风,<3级,1","这不是一个时点","06日14时,d00","99日11时,d00,晴,1℃,西风,<3级,1","06日32时,d00,晴,1℃,西风,<3级,1",null,{"x":1},{"y":[1]}],
            "23d":"不是数组","7d":[[null,true,"06日17时,d00,晴,9℃,西风,<3级,1"]]}"#,
    );

    let payload = parse_hourly_page(&html).expect("有效时点仍然存在");
    assert_eq!(
        times_of(&payload.points),
        vec!["06日11:00", "06日17:00"],
        "非法条目被丢弃，有效时点保留并按时间排序"
    );
}

/// 三个字段缺失任一 / 全部缺失时的行为：有数据就合并，没数据就报错。
#[test]
fn tolerates_missing_fields_but_reports_a_total_miss() {
    let only_7d = hourly_page(
        "2026-11-06 07:30",
        r#"{"7d":[["06日11时,d00,晴,12℃,西风,<3级,1","06日14时,d00,晴,13℃,西风,<3级,1"]]}"#,
    );
    let payload = parse_hourly_page(&only_7d).expect("只有 7d 也能解析");
    assert_eq!(times_of(&payload.points), vec!["06日11:00", "06日14:00"]);

    let empty = hourly_page("2026-11-06 07:30", r#"{"1d":[],"23d":[],"7d":[]}"#);
    let err = parse_hourly_page(&empty).expect_err("没有任何时点必须报错");
    assert_eq!(err.code(), "parse_error");
}

/// 7 天全集（56 个时点）必须被截断到内存上限，且被丢掉的永远是最远的时点。
#[test]
fn caps_the_series_without_dropping_near_term_points() {
    let start = build_cst(2026, 11, 6, 11, 0).expect("合法时间");
    let mut days: Vec<Vec<String>> = Vec::new();
    for day in 0..7 {
        let mut entries = Vec::new();
        for slot in 0..8 {
            let time = start + chrono::Duration::hours(24 * day + 3 * slot);
            entries.push(format!(
                "{}日{}时,d00,晴,1℃,西风,<3级,1",
                time.format("%d"),
                time.format("%H")
            ));
        }
        days.push(entries);
    }
    let html = hourly_page(
        "2026-11-06 07:30",
        &serde_json::json!({ "7d": days }).to_string(),
    );

    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(
        payload.points.len(),
        MAX_HOURLY_POINTS,
        "必须受内存上限约束"
    );
    assert_eq!(
        payload.points[0].time, start,
        "超限时丢弃的是最远的时点，近期数据必须原样保留"
    );
    assert_eq!(
        payload.points[MAX_HOURLY_POINTS - 1].time,
        start + chrono::Duration::hours(3 * (MAX_HOURLY_POINTS as i64 - 1)),
    );
    // 上限必须至少能装下一个完整的 24 小时窗口（8 个时点），否则补充远期预报
    // 反而会把近期数据截掉。
    const { assert!(MAX_HOURLY_POINTS >= 9) };
}
