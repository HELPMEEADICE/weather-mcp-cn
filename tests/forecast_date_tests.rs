//! 7 日预报日期严格解析的集成回归测试。
//!
//! 站点只在 `<h1>` 里给「日号」，月/年必须由上下文推断。这一组用例把
//! `input#fc_24h_internal_update_time` 固定成被测的基准日期，因此结果与系统时钟
//! 完全无关。
//!
//! 按基线 `17a4419` 的实测结果，本文件的用例分两类并在文档注释第一行标明：
//! **判别性回归测试**（旧实现会失败，例如 `31日` 被向前搜成 `12-31`）与
//! **语义护栏**（旧实现恰好也满足，用来防止后续严格化改动把合法日期一起弄坏）。

use weather_mcp_cn::providers::weather_com_cn::{ForecastPayload, parse_forecast_page};

/// 拼一个最小的 7 日预报页面。
///
/// `published` 走隐藏字段 `fc_24h_internal_update_time`，它同时决定
/// `published_at` 与推断日期用的 `base_date`；测试因此不依赖当前时间。
fn forecast_page(published: &str, items: &str) -> String {
    format!(
        r#"<html><body>
<input type="hidden" id="fc_24h_internal_update_time" value="{published}"/>
<ul class="t">{items}</ul>
</body></html>"#
    )
}

/// 单条预报：标题 + 天气 + 温度组，字段形态与真实页面一致。
fn forecast_item(heading: &str, weather: &str) -> String {
    format!(
        r#"<li><h1>{heading}</h1><p class="wea">{weather}</p><p class="tem"><span>13℃</span>/<i>5℃</i></p></li>"#
    )
}

fn dates_of(payload: &ForecastPayload) -> Vec<String> {
    payload.days.iter().map(|day| day.date.clone()).collect()
}

/// 缺陷：`32日` 这种不存在的日号必须整条跳过。
///
/// 旧实现的日期推断对「不存在的日号」没有边界判断，会把它推到下个月，凭空造出
/// 一份远期预报。新期望：该条目不产生任何日期，同页合法条目不受影响。
#[test]
fn skips_day_of_month_32() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[forecast_item("6日", "晴"), forecast_item("32日", "暴雨")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("合法条目仍然存在");
    assert_eq!(dates_of(&payload), vec!["2026-11-06"]);
    assert_eq!(
        payload.days[0].weather.as_deref(),
        Some("晴"),
        "32日 那条的天气不得混进结果"
    );
}

/// 缺陷：`00日` 同样不可能存在，必须跳过而不是被算成「上个月最后一天」或「今天」。
#[test]
fn skips_day_of_month_00() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[forecast_item("6日", "晴"), forecast_item("00日", "暴雨")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("合法条目仍然存在");
    assert_eq!(dates_of(&payload), vec!["2026-11-06"]);
}

/// 缺陷（核心）：`31日` 出现在只有 30 天的月份。
///
/// 旧实现对「同月没有这一天」的反应是**向前搜最多 14 个月**，于是 11 月的 `31日`
/// 会命中 `2026-12-31`，造出一份 55 天后的预报。
/// 新期望：没有合法候选时整条跳过，绝不能变成 `2026-12-31`。
#[test]
fn day_31_in_a_30_day_month_is_rejected_not_moved_to_december() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[forecast_item("6日", "晴"), forecast_item("31日", "暴雨")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("合法条目仍然存在");
    assert_eq!(dates_of(&payload), vec!["2026-11-06"]);
    assert!(
        payload.days.iter().all(|day| day.date != "2026-12-31"),
        "31日 被错误地推到 12 月：{:?}",
        dates_of(&payload)
    );
}

/// 缺陷：平年的 `02月29日` 不存在。
///
/// 旧实现跳到 3 月 29 日（或更远的月份），把不存在的日期变成一个合法日期。
/// 新期望：2026 年没有 2 月 29 日 → 该条目跳过。
#[test]
fn feb_29_in_a_common_year_is_rejected() {
    let html = forecast_page(
        "2026-02-27 07:30",
        &[forecast_item("27日", "晴"), forecast_item("29日", "暴雨")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("合法条目仍然存在");
    assert_eq!(dates_of(&payload), vec!["2026-02-27"]);
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：闰年 2028 的 `02月29日` 是真实存在的
/// 日期，必须被解析出来 —— 不能因为「29 日在小月/平年上非法」就一刀切。
#[test]
fn feb_29_in_a_leap_year_is_accepted() {
    let html = forecast_page(
        "2028-02-27 07:30",
        &[forecast_item("27日", "晴"), forecast_item("29日", "小雨")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("应可解析");
    assert_eq!(dates_of(&payload), vec!["2028-02-27", "2028-02-29"]);
    assert_eq!(payload.days[1].weather.as_deref(), Some("小雨"));
    assert_eq!(payload.days[1].weekday.as_deref(), Some("周二"));
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：跨年 —— `12-31` 页面上的 `01日`
/// 落到次年 `01-01`，而不是上一个月。
///
/// 旧实现的 `next_date_with_day` 在这一例上恰好给出了正确结果（`12-31` 的下一个
/// 「1 日」就是明年 1 月 1 日），因此这不是判别性测试。它锁住的是「跨年推断不能被
/// 后续严格化改动弄坏」：例如把候选月份过度收紧成「只允许锚点所在月」，会让 `01-01`
/// 整条消失、只剩 `2026-12-31`。
#[test]
fn resolves_across_the_year_boundary() {
    let html = forecast_page(
        "2026-12-31 07:30",
        &[forecast_item("31日", "晴"), forecast_item("01日", "多云")].concat(),
    );
    let payload = parse_forecast_page(&html).expect("应可解析");
    assert_eq!(dates_of(&payload), vec!["2026-12-31", "2027-01-01"]);
    assert_eq!(payload.days[1].weather.as_deref(), Some("多云"));
}

/// 缺陷：重复日期必须去重，且保留**第一条**（真实页面里第二条往往是脏数据）。
#[test]
fn duplicate_dates_keep_only_the_first_item() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[
            forecast_item("6日（今天）", "晴"),
            forecast_item("6日（今天）", "暴雨"),
        ]
        .concat(),
    );
    let payload = parse_forecast_page(&html).expect("应可解析");
    assert_eq!(dates_of(&payload), vec!["2026-11-06"]);
    assert_eq!(payload.days.len(), 1, "重复日期只保留一条");
    assert_eq!(payload.days[0].weather.as_deref(), Some("晴"));
}

/// 缺陷：日期逆序（`7日` 之后又来 `5日`）必须跳过后者，且不能污染后续游标。
///
/// 旧实现把 `5日` 算成下个月的 5 日（28 天后），于是后面的 `8日` 被顶掉。
#[test]
fn backwards_date_is_skipped_without_poisoning_the_cursor() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[
            forecast_item("7日", "多云"),
            forecast_item("5日", "暴雨"),
            forecast_item("8日", "小雨"),
        ]
        .concat(),
    );
    let payload = parse_forecast_page(&html).expect("应可解析");
    assert_eq!(dates_of(&payload), vec!["2026-11-07", "2026-11-08"]);
    assert!(
        payload
            .days
            .iter()
            .all(|day| day.weather.as_deref() != Some("暴雨")),
        "逆序条目的内容不得出现在结果里"
    );
}

/// 缺陷：日期标题与「明天」标签矛盾时，不能把标签给的期望日期硬塞进去。
///
/// base = 2026-11-06，「明天」= 11-07，但标题写的是 `9日`。旧实现会产出
/// `2026-11-07`（把 9 日的数据当成明天的数据），等于替上游编造日期。
/// 新期望：整条跳过；紧随其后的合法 `7日（明天）` 正常保留。
#[test]
fn heading_contradicting_its_label_is_skipped() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[
            forecast_item("6日（今天）", "晴"),
            forecast_item("9日（明天）", "暴雨"),
            forecast_item("7日（明天）", "多云"),
        ]
        .concat(),
    );
    let payload = parse_forecast_page(&html).expect("合法条目仍然存在");
    assert_eq!(dates_of(&payload), vec!["2026-11-06", "2026-11-07"]);
    assert_eq!(payload.days[1].weather.as_deref(), Some("多云"));
    assert_eq!(payload.days[1].label.as_deref(), Some("明天"));
}

/// 缺陷：部分日期损坏时，只有坏条目被丢弃，有效日期必须**全部**保留且顺序不变。
///
/// 旧实现遇到坏条目会污染游标，导致后面的有效条目一起错位或被算到错误的月份。
#[test]
fn damaged_items_are_dropped_while_valid_dates_survive_in_order() {
    let html = forecast_page(
        "2026-11-06 07:30",
        &[
            forecast_item("6日", "晴"),
            forecast_item("32日", "暴雨"),
            forecast_item("7日", "多云"),
            forecast_item("00日", "暴雨"),
            forecast_item("8日", "小雨"),
        ]
        .concat(),
    );
    let payload = parse_forecast_page(&html).expect("应可解析");
    assert_eq!(
        dates_of(&payload),
        vec!["2026-11-06", "2026-11-07", "2026-11-08"]
    );
    assert_eq!(payload.days[1].weather.as_deref(), Some("多云"));
    assert_eq!(payload.days[2].weather.as_deref(), Some("小雨"));
    assert_eq!(
        payload.published_at_raw.as_deref(),
        Some("fc_24h_internal_update_time=2026-11-06 07:30")
    );
}

/// 缺陷：日期全部无效时报 `parse_error`，且文案必须与「一个条目都没有」区分开 ——
/// 前者是上游日期坏了，后者是页面结构变了，运维要能一眼分辨。
#[test]
fn all_invalid_dates_and_zero_items_produce_distinct_parse_errors() {
    let all_invalid = forecast_page(
        "2026-11-06 07:30",
        &[
            forecast_item("32日", "晴"),
            forecast_item("00日", "晴"),
            forecast_item("31日", "晴"),
        ]
        .concat(),
    );
    let err = parse_forecast_page(&all_invalid).expect_err("日期全部无效必须报错");
    assert_eq!(err.code(), "parse_error");
    let invalid_message = err.to_string();
    assert!(
        invalid_message.contains("日期全部无效"),
        "文案应说明日期全部无效：{invalid_message}"
    );

    let no_items = r#"<html><body><input type="hidden" id="fc_24h_internal_update_time" value="2026-11-06 07:30"/><ul class="t"></ul></body></html>"#;
    let err = parse_forecast_page(no_items).expect_err("没有条目必须报错");
    assert_eq!(err.code(), "parse_error");
    let empty_message = err.to_string();
    assert!(
        empty_message.contains("页面结构可能已变化"),
        "文案应说明页面结构变化：{empty_message}"
    );
    assert_ne!(
        invalid_message, empty_message,
        "「日期全部无效」与「零条目」必须是两条可区分的错误"
    );
}

/// 回归保护（基线 `17a4419` 上本条实测是 `ok`）：真实样本的日期链与温度仍必须原样成立。
///
/// 真实页面（2026-11-05 抓取）里当天已入夜，只给最低温，因此
/// `today.temperature_high_c` 必须是 `None` —— 严格日期解析不能顺手把温度补上。
#[test]
fn real_forecast_fixture_still_resolves_seven_days() {
    const SAMPLE: &str = include_str!("fixtures/forecast_7d_101010100.html");

    let payload = parse_forecast_page(SAMPLE).expect("真实样本应可解析");
    assert_eq!(payload.days.len(), 7, "7 日预报页面应有 7 天");
    assert_eq!(dates_of(&payload)[0], "2026-11-05");

    let today = &payload.days[0];
    assert_eq!(today.label.as_deref(), Some("今天"));
    assert_eq!(today.temperature_low_c, Some(0));
    assert_eq!(
        today.temperature_high_c, None,
        "当天已入夜，最高温必须留空而不是编一个数字"
    );

    let tomorrow = &payload.days[1];
    assert_eq!(tomorrow.date, "2026-11-06");
    assert_eq!(tomorrow.temperature_high_c, Some(14));
    assert_eq!(tomorrow.temperature_low_c, Some(2));
    assert_eq!(tomorrow.weather.as_deref(), Some("晴转多云"));
    assert_eq!(tomorrow.weather_day.as_deref(), Some("晴"));
    assert_eq!(tomorrow.weather_night.as_deref(), Some("多云"));
}
