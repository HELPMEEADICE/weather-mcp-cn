//! 实况（`sk_2d`）有效性门槛的集成回归测试。
//!
//! 核心命题：**上游没给数据就不能返回成功**。`var dataSK={}`、`{"cityname":"北京"}`、
//! `{"temp":"N/A"}` 这类空壳如果被当成成功解析，调用方会以为「查到了天气」。
//! 因此门槛必须设在解析边界（`parse_current_js`），而不是靠调用方自己判断字段是否为空。
//!
//! 全部用例离线：既不发请求，也不读系统时钟（`fetched_at` 由 `build_cst` 固定）。

use chrono::{DateTime, FixedOffset};
use weather_mcp_cn::models::{CityInfo, CityLevel};
use weather_mcp_cn::providers::weather_com_cn::{build_cst, parse_current_js};

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

fn fetched_at() -> DateTime<FixedOffset> {
    build_cst(2026, 11, 6, 9, 0).expect("合法时间")
}

/// 缺陷 1：`var dataSK={};` —— 接口 200 但没有任何字段。
///
/// 旧实现直接返回一份「字段全空」的 `ObservationPayload`，工具层于是给出
/// `isError: false` 的空观测。新期望：`parse_error`。
#[test]
fn empty_datask_is_a_parse_error() {
    let err = parse_current_js("var dataSK={};").expect_err("空 dataSK 不能当成功");
    assert_eq!(err.code(), "parse_error");
    assert!(
        err.to_string().contains("不含任何气象字段"),
        "错误文案应说明没有任何气象字段：{err}"
    );
}

/// 缺陷 2：只有城市名（`{"cityname":"北京"}`）—— 有元信息、没有气象值。
///
/// 城市名不是气象数据；旧实现仅凭「解析出一个对象」就报成功。
#[test]
fn datask_with_only_city_name_is_a_parse_error() {
    let err = parse_current_js(r#"var dataSK={"cityname":"北京"}"#)
        .expect_err("只有城市名不能算查到了天气");
    assert_eq!(err.code(), "parse_error");
}

/// 缺陷 3：`{"temp":"N/A"}` —— 哨兵字符串必须当成「没有值」，而不是当成 0 或
/// 把 `N/A` 原样塞进气温字段。
#[test]
fn sentinel_temperature_string_is_not_a_measurement() {
    let err = parse_current_js(r#"var dataSK={"temp":"N/A"}"#).expect_err("N/A 不是气温");
    assert_eq!(err.code(), "parse_error");
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：只给气温属于**部分有效**，必须解析成功。
///
/// 不是缺陷判别测试 —— 旧实现这里本来就对，`missing_fields` 的统计也本来就正确。
/// 它防止的是**反向风险**：为了让空壳载荷被拒绝，把门槛收得过紧（例如「必须同时有
/// 气温与天气」）会连「只给气温」这种真实存在的上游响应一起拒掉。断言同时锁住两件事：
/// 温度真实存在就不得出现在 `missing_fields` 里、`weather` 确实缺失就必须列出，
/// 且绝不允许把缺失的温度填成 0 或编一个天气现象。
#[test]
fn temperature_only_payload_succeeds_and_marks_weather_missing() {
    let payload = parse_current_js(r#"var dataSK={"temp":"12"}"#).expect("只有气温也算部分有效");
    assert_eq!(payload.temperature_c, Some(12.0));

    let observation = payload.into_observation(beijing(), "u".to_string(), fetched_at());
    assert_eq!(observation.temperature_c, Some(12.0));
    assert!(observation.is_observation);
    assert!(
        observation.missing_fields.iter().any(|f| f == "weather"),
        "weather 确实缺失，必须列出：{:?}",
        observation.missing_fields
    );
    assert!(
        !observation
            .missing_fields
            .iter()
            .any(|f| f == "temperature_c"),
        "温度真实存在，不得出现在 missing_fields：{:?}",
        observation.missing_fields
    );
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：气温与天气同时存在时两个字段都不应进
/// `missing_fields`。
///
/// 与上一条互补，防止「缺失字段统计反了」这类改动：只要有人把 `track!` 的条件写反、
/// 或把有效值误判成缺失，这条会立刻失败。它不是历史缺陷的判别性测试。
#[test]
fn temperature_and_weather_are_not_marked_missing_when_present() {
    let payload =
        parse_current_js(r#"var dataSK={"temp":"12","weather":"晴"}"#).expect("两个字段都有效");
    assert_eq!(payload.temperature_c, Some(12.0));
    assert_eq!(payload.weather.as_deref(), Some("晴"));

    let observation = payload.into_observation(beijing(), "u".to_string(), fetched_at());
    assert!(observation.missing_fields.iter().all(|f| f != "weather"));
    assert!(
        observation
            .missing_fields
            .iter()
            .all(|f| f != "temperature_c")
    );
    assert_eq!(observation.weather.as_deref(), Some("晴"));
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：HTML 响应里没有 `dataSK` 块。
///
/// 旧实现在「找不到 `dataSK`」这一步就已经返回 `parse_error`（文案「未返回 dataSK」），
/// 所以这条不是历史缺陷的判别性测试。它锁住的是「整页 HTML 必须被拒绝」这个语义：
/// 上游被 CDN 拦截时返回的正是 HTML，任何把「找不到标记」默默当成「字段全空」的实现
/// 改动都会立刻在这里失败。
#[test]
fn html_response_without_datask_is_a_parse_error() {
    let err = parse_current_js("<html><body><h1>403 Forbidden</h1></body></html>")
        .expect_err("HTML 响应不能当成功");
    assert_eq!(err.code(), "parse_error");
    assert!(
        err.to_string().contains("未返回 dataSK"),
        "文案应指向 dataSK 缺失：{err}"
    );
}

/// 缺陷 7：`var dataSK=null;` —— 标记存在、值是 null。
///
/// 旧实现会截到一个空块后当「无字段」处理并返回成功；新期望是 `parse_error`。
#[test]
fn null_datask_is_a_parse_error() {
    let err = parse_current_js("var dataSK=null;").expect_err("null 不能当成功");
    assert_eq!(err.code(), "parse_error");
    assert!(
        err.to_string().contains("不是 JSON 对象"),
        "文案应说明结构异常：{err}"
    );
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：`dataSK` 是数组 / 数字 —— 类型完全错了。
///
/// 旧实现已在读取字段之前做了 `is_object()` 检查并报错，因此这不是判别性测试。
/// 它防止的**未来风险**：若有人删掉对象类型检查、改成「按字段读，读不到就算了」，
/// 就会得到一份「全部字段为 `None`」的载荷，与「成功但没有数据」完全无法区分 ——
/// 那时这条会失败。
#[test]
fn non_object_datask_is_a_parse_error() {
    for body in ["var dataSK=[1,2,3];", "var dataSK=42;"] {
        let err = parse_current_js(body).expect_err("非对象 dataSK 必须报错");
        assert_eq!(err.code(), "parse_error", "{body}");
    }
}

/// 语义护栏（基线 `17a4419` 上本条实测是 `ok`）：截断数据（缺右花括号）。
///
/// 旧实现的 `extract_json_object` 在花括号不配平时返回 `None`，同样落进
/// `parse_error`，所以这不是判别性测试。它锁住「半份 JSON 绝不能变成一次成功的观测」：
/// 任何试图把不配平的花括号「尽量截到最后一个 `}`」再交给 serde 的容错改动，都会
/// 让这里失败（或让结果随上游噪声漂移）。
#[test]
fn truncated_datask_is_a_parse_error() {
    let err = parse_current_js(r#"var dataSK={"temp":"12","weather":"晴""#)
        .expect_err("截断数据必须报错");
    assert_eq!(err.code(), "parse_error");
}

/// 缺陷 10 的「已能拒绝」的一半：`{"temp":"abc"}` 不是数字。
///
/// 这一条在当前实现里已经成立（`json_number` 的 `parse::<f64>()` 会失败），
/// 因此不加 `#[ignore]`：温度拿不到 → 一条气象字段都没有 → `parse_error`。
#[test]
fn non_numeric_temperature_is_not_a_measurement() {
    let err = parse_current_js(r#"var dataSK={"temp":"abc"}"#).expect_err("abc 不是气温");
    assert_eq!(err.code(), "parse_error");
}

/// 缺陷 10 的「量程」一半：`{"temp":"99999"}`、`{"SD":"250"}` 必须不能作为有效值。
///
/// **历史行为记录**：不加量程校验时，`json_number` 只过滤哨兵值，于是
/// `{"temp":"99999"}` 会返回 `Ok` 且 `temperature_c == Some(99999.0)`，
/// `{"SD":"250"}` 会返回 `Ok` 且 `humidity_percent == Some(250.0)` —— 上游把
/// 不可能出现的数值塞进来时会被当成真实观测。
///
/// 与本测试同批的并行任务已经把物理量程校验合并进 `src/providers/weather_com_cn.rs`
/// （`RANGE_TEMPERATURE_C = -90..=60`、`RANGE_HUMIDITY_PERCENT = 0..=100`，逐字段
/// `json_number_in` / `json_unit_number_in`），越界值在读取时即被判为缺失，因此整条
/// 载荷落到「不含任何气象字段」这一 `parse_error` 分支。断言据此写死，作为可执行的
/// 回归门槛。
#[test]
fn out_of_range_temperature_and_humidity_are_not_valid_measurements() {
    // 99999℃ 远超 -90..=60：整条载荷只剩一个越界值，必须报错而不是返回 Some(99999.0)。
    let err =
        parse_current_js(r#"var dataSK={"temp":"99999"}"#).expect_err("越界气温不能被当成有效气温");
    assert_eq!(err.code(), "parse_error");
    assert!(
        err.to_string().contains("不含任何气象字段"),
        "越界值被当成缺失后应落到「没有气象字段」：{err}"
    );

    // 250% 湿度不可能存在（量程 0..=100）。
    let err = parse_current_js(r#"var dataSK={"SD":"250"}"#).expect_err("越界湿度不能当有效湿度");
    assert_eq!(err.code(), "parse_error");

    // 对照：合法边界内的湿度必须照旧解析出来，量程校验不能顺手误杀真实数据。
    let payload = parse_current_js(r#"var dataSK={"SD":"25"}"#).expect("25% 是合法湿度");
    assert_eq!(payload.humidity_percent, Some(25.0));
}

/// 回归保护（基线 `17a4419` 上本条实测是 `ok`）：真实样本必须仍然解析成功，观测时间落在 UTC+8。
///
/// 不是判别性测试：它守的是「有效性门槛 / 量程校验不能把真实数据一起拒掉」，以及
/// 「不得为了让门槛通过而改动任何字段值」。真实样本的字段值是硬约束
/// （`25%` / `1021` / `16` / `晴` / `23:05`）。
#[test]
fn real_current_sample_is_valid_and_timestamped_in_cst() {
    const SAMPLE: &str = include_str!("fixtures/current_sk_2d.txt");

    let payload = parse_current_js(SAMPLE).expect("真实样本应可解析");
    assert_eq!(payload.city_name.as_deref(), Some("北京"));
    let temperature = payload.temperature_c.expect("样本里有气温");
    assert!(
        (-60.0..60.0).contains(&temperature),
        "气温应落在合理范围：{temperature}"
    );
    assert_eq!(payload.weather.as_deref(), Some("晴"));
    assert_eq!(payload.humidity_percent, Some(25.0));
    assert_eq!(payload.pressure_hpa, Some(1021.0));
    assert_eq!(payload.aqi, Some(16));

    let observed_at = payload.observed_at.expect("样本里有观测时间");
    assert_eq!(
        observed_at.offset().local_minus_utc(),
        8 * 3600,
        "观测时间必须是北京时间（UTC+8）"
    );
    assert_eq!(observed_at.format("%m-%d").to_string(), "11-05");
    assert_eq!(observed_at.format("%H:%M").to_string(), "23:05");
}
