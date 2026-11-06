//! 解析器加固测试：每条断言都显式标注它对应的审计缺陷编号。
//!
//! | 缺陷 | 主题 | 本文件的测试 |
//! | --- | --- | --- |
//! | D2 | `extract_json_object` 标识符边界 + 前导距离上限 | [`d2_marker_must_be_a_whole_identifier`]、[`d2_json_lead_is_bounded`]、[`d2_real_fixtures_still_extract_their_blocks`] |
//! | D3 | 实况字段物理量程 | [`d3_out_of_range_values_become_missing`]、[`d3_real_sample_survives_all_ranges`] |
//! | D4 | 预警字段接受数字，不再产生幽灵预警 | [`d4_numeric_alert_fields_produce_real_alert`]、[`d4_abnormal_w_shapes_still_error`] |
//! | D5 | AQI 不再饱和成 `u32::MAX` | [`d5_aqi_is_range_checked_not_saturated`] |
//! | D6 | `999` 不再被当成万能缺失标记 | [`d6_999_is_a_real_pressure_not_a_sentinel`] |
//! | D7 | 带单位字段只认前导数值 | [`d7_unit_fields_reject_malformed_numbers`] |
//! | D9 | 发布时间跨零点回滚 | [`d9_publish_time_rolls_back_across_midnight`]、[`d9_publish_time_with_base_date_ignores_now`]、[`d9_real_forecast_page_publish_time_unchanged`] |
//! | D10 | 日期不存在时观测时间说明必须如实 | [`d10_impossible_date_reports_honest_note`] |
//!
//! 全部使用落盘的真实样本与内联字符串，不依赖网络。

use weather_mcp_cn::models::{CityInfo, CityLevel};
use weather_mcp_cn::providers::weather_com_cn::{
    build_cst, extract_json_object, parse_alerts_payload, parse_current_js, parse_forecast_page,
    parse_hourly_page, publish_datetime,
};

const CURRENT_SAMPLE: &str = include_str!("fixtures/current_sk_2d.txt");
const ALERT_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_101060101.txt");
const ALERT_EMPTY_SAMPLE: &str = include_str!("fixtures/alerts_dingzhi_empty_101010100.txt");
const HOURLY_FULL_PAGE: &str = include_str!("fixtures/hourly_1d_101010100_20261106.html");
const FORECAST_SAMPLE: &str = include_str!("fixtures/forecast_7d_101010100.html");

fn city() -> CityInfo {
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

fn at(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
) -> chrono::DateTime<chrono::FixedOffset> {
    build_cst(year, month, day, hour, minute).expect("测试时间是合法时间")
}

// ---------------------------------------------------------------------------
// D2：extract_json_object 的标识符边界与前导距离上限
// ---------------------------------------------------------------------------

/// D2 缺陷（根因）：`source.find(marker)` 是裸子串匹配，且 marker 到 `{` 的搜索
/// **没有上限**，于是「数据块缺失」会被下游第一个无关的 `{` 顶替成 JSON 起点。
#[test]
fn d2_marker_must_be_a_whole_identifier() {
    // 审计给的复现输入：`hour3data` 的数据块根本不存在，后面的 `{` 属于别的配置对象。
    assert_eq!(
        extract_json_object(r#"var hour3data=;var cfg={"debug":1};"#, "hour3data"),
        None,
        "D2：数据块缺失时绝不能把下游无关对象的 `{{` 当成 hour3data 的起点"
    );

    // 裸子串匹配：`myhour3data` 里的 `hour3data` 不是标识符本身，前一个字符是标识符字符。
    assert_eq!(
        extract_json_object(r#"var myhour3data={"a":1};"#, "hour3data"),
        None,
        "D2：marker 前一个字符是标识符字符时不是合法匹配"
    );
    // 注意有意的不对称：marker **后面**紧跟的标识符续接字符被视为同一标识符的后缀，
    // 因为真实形态 `var alarmDZ101060101 ={"w":...}` 正是「marker + 城市代码」。
    // 因此在 `hour3data_old={` 上仍会命中 —— 这个方向的代价只是「提取到同一脚本里
    // 前缀相同的对象」，而审计要求的边界规则本身只管前一个字符。
    assert_eq!(
        extract_json_object(r#"var alarmDZ101060101 ={"w":[]}"#, "alarmDZ"),
        Some(r#"{"w":[]}"#),
        "D2：marker 后跟城市代码是真实形态，必须继续命中"
    );

    // 跳过非法匹配后必须继续找下一个合法匹配，而不是直接放弃。
    assert_eq!(
        extract_json_object(r#"var xhour3data=1; var hour3data={"a":2};"#, "hour3data"),
        Some(r#"{"a":2}"#),
        "D2：非法匹配被跳过后仍要能找到真正的数据块"
    );

    // 真实写法（`var dataSK={`、`var alarmDZ101060101 ={`）必须继续工作。
    assert_eq!(
        extract_json_object(r#"var dataSK={"a":1};"#, "dataSK"),
        Some(r#"{"a":1}"#)
    );
    assert_eq!(
        extract_json_object(r#"var alarmDZ101060101 ={"w":[]}"#, "alarmDZ"),
        Some(r#"{"w":[]}"#)
    );
}

/// D2 缺陷（根因补充）：无界线性扫描可以跨过任意远的、毫不相关的 `{`。
#[test]
fn d2_json_lead_is_bounded() {
    // 短 gap（真实页面形态）仍然能提取。
    let short = r#"var hour3data = {"1d":[]};"#;
    assert_eq!(
        extract_json_object(short, "hour3data"),
        Some(r#"{"1d":[]}"#)
    );

    // gap 超过 64 字节（这里全是合法空白）就必须失败，而不是继续往文档后面找。
    let long = format!("var hour3data={}{}", " ".repeat(200), r#"{"1d":[]}"#);
    assert_eq!(
        extract_json_object(&long, "hour3data"),
        None,
        "D2：marker 到 `{{` 的距离超过 MAX_JSON_LEAD 时不得继续扫描"
    );

    // gap 里出现非「标识符续接 / 空白 / =」的字符同样是失败。
    assert_eq!(
        extract_json_object(r#"var hour3data:{"1d":[]}"#, "hour3data"),
        None,
        "D2：gap 只允许标识符续接字符、空白与 `=`"
    );
}

/// 真实 fixture 的三种 marker 改完后都必须仍然能提取并解析。
#[test]
fn d2_real_fixtures_still_extract_their_blocks() {
    assert!(
        extract_json_object(CURRENT_SAMPLE, "dataSK").is_some(),
        "D2：真实 sk_2d 样本必须仍可提取"
    );
    assert!(
        extract_json_object(ALERT_SAMPLE, "alarmDZ").is_some(),
        "D2：真实 dingzhi 样本（marker 后紧跟城市代码）必须仍可提取"
    );
    assert!(
        extract_json_object(ALERT_EMPTY_SAMPLE, "alarmDZ").is_some(),
        "D2：真实空预警样本必须仍可提取"
    );
    assert!(
        extract_json_object(HOURLY_FULL_PAGE, "hour3data").is_some(),
        "D2：真实 67KB 逐三小时页面必须仍可提取"
    );

    assert!(parse_current_js(CURRENT_SAMPLE).is_ok());
    assert_eq!(
        parse_alerts_payload(ALERT_SAMPLE)
            .expect("真实预警样本")
            .alerts
            .len(),
        2
    );
    assert!(
        parse_alerts_payload(ALERT_EMPTY_SAMPLE)
            .expect("真实空预警样本")
            .alerts
            .is_empty()
    );
    assert!(
        !parse_hourly_page(HOURLY_FULL_PAGE)
            .expect("真实逐三小时页面")
            .points
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// D3：实况字段物理量程
// ---------------------------------------------------------------------------

/// D3 缺陷（根因）：`parse_current_js` 读字段时没有任何量程校验，
/// `{"temp":"99999","SD":"250","qy":"1","njd":"-5"}` 会作为**有效观测**返回。
#[test]
fn d3_out_of_range_values_become_missing() {
    let body = r#"var dataSK={"temp":"99999","SD":"250","qy":"1","njd":"-5","wse":"999km/h","rain":"99999","weather":"晴"}"#;
    let payload = parse_current_js(body).expect("有一条有效的 weather，整体仍应解析成功");

    assert!(payload.temperature_c.is_none(), "99999℃ 超出 -90..=60");
    assert!(payload.humidity_percent.is_none(), "250% 超出 0..=100");
    assert!(payload.pressure_hpa.is_none(), "1hPa 超出 300..=1100");
    assert!(payload.visibility_km.is_none(), "-5km 超出 0..=100");
    assert!(payload.wind_speed_kmh.is_none(), "999km/h 超出 0..=500");
    assert!(payload.precipitation_mm.is_none(), "99999mm 超出 0..=3000");

    // 超量程必须自然落进 missing_fields，而不是被填成 0。
    let observation =
        payload.into_observation(city(), "test://sk".to_string(), at(2026, 11, 6, 9, 0));
    for field in [
        "temperature_c",
        "humidity_percent",
        "pressure_hpa",
        "visibility_km",
        "wind_speed_kmh",
        "precipitation_mm",
    ] {
        assert!(
            observation.missing_fields.iter().any(|name| name == field),
            "D3：{field} 应出现在 missing_fields 里：{:?}",
            observation.missing_fields
        );
    }
    assert!(observation.temperature_c.is_none(), "D3：绝不把缺失填成 0");
    assert_eq!(
        observation.weather.as_deref(),
        Some("晴"),
        "有效字段不受影响"
    );
}

/// 反方向保护：真实样本的每个值都在量程内，加固不能误杀真实数据。
#[test]
fn d3_real_sample_survives_all_ranges() {
    let payload = parse_current_js(CURRENT_SAMPLE).expect("真实样本必须仍可解析");
    assert_eq!(payload.temperature_c, Some(9.1), "D3：气温必须保留");
    assert_eq!(payload.humidity_percent, Some(25.0), "D3：湿度必须保留");
    assert_eq!(payload.pressure_hpa, Some(1021.0), "D3：气压必须保留");
    assert_eq!(payload.visibility_km, Some(30.0), "D3：能见度必须保留");
    assert_eq!(payload.aqi, Some(16), "D3/D5：AQI 必须保留");
    assert_eq!(
        payload.precipitation_mm,
        Some(0.0),
        "D3：0mm 是真实值，不是缺失"
    );
    assert_eq!(payload.precipitation_24h_mm, Some(0.0));
    assert_eq!(payload.wind_speed_kmh, Some(3.0), "D3/D7：`3km/h` → 3.0");
}

// ---------------------------------------------------------------------------
// D5：AQI 不许饱和转换
// ---------------------------------------------------------------------------

/// D5 缺陷（根因）：`as u32` 之前没有校验，`{"aqi":"1e30"}` → `Some(4294967295)`。
#[test]
fn d5_aqi_is_range_checked_not_saturated() {
    let huge = parse_current_js(r#"var dataSK={"aqi":"1e30","weather":"晴"}"#)
        .expect("weather 有效，整体成功");
    assert!(
        huge.aqi.is_none(),
        "D5：1e30 必须判为缺失，而不是饱和成 {}",
        u32::MAX
    );

    let negative =
        parse_current_js(r#"var dataSK={"aqi":"-3","weather":"晴"}"#).expect("weather 有效");
    assert!(negative.aqi.is_none(), "D5：负数 AQI 必须判为缺失");

    let over =
        parse_current_js(r#"var dataSK={"aqi":"1001","weather":"晴"}"#).expect("weather 有效");
    assert!(over.aqi.is_none(), "D5：> 1000 必须判为缺失");

    // 端点必须保留：0 与 1000 都是合法值。
    let zero = parse_current_js(r#"var dataSK={"aqi":"0","weather":"晴"}"#).expect("weather 有效");
    assert_eq!(zero.aqi, Some(0), "D5：AQI=0 是合法值");
    let top =
        parse_current_js(r#"var dataSK={"aqi":"1000","weather":"晴"}"#).expect("weather 有效");
    assert_eq!(top.aqi, Some(1000), "D5：AQI=1000 是量程端点，合法");
}

// ---------------------------------------------------------------------------
// D6：999 不是万能缺失标记
// ---------------------------------------------------------------------------

/// D6 缺陷（根因）：`is_sentinel` / `is_sentinel_number` 把 `999` 当缺失标记，
/// 于是真实存在的强低压 `999 hPa` 被判成缺失 —— 把有效数据判成缺失的反向错误。
#[test]
fn d6_999_is_a_real_pressure_not_a_sentinel() {
    let pressure =
        parse_current_js(r#"var dataSK={"qy":"999","weather":"晴"}"#).expect("weather 有效");
    assert_eq!(
        pressure.pressure_hpa,
        Some(999.0),
        "D6：999 hPa 是真实的强低压，必须保留"
    );

    // 同一个数字出现在气温上仍然必须是缺失 —— 这一次由 D3 的量程挡掉，不是靠魔法数字。
    let temperature =
        parse_current_js(r#"var dataSK={"temp":"999","weather":"晴"}"#).expect("weather 有效");
    assert!(
        temperature.temperature_c.is_none(),
        "D6：temp=999 超出 -90..=60，仍然是缺失"
    );

    // 真正的文本缺失标记必须继续生效。
    for sentinel in ["暂无", "无", "N/A", "n/a", "null", "-", "--"] {
        let body = format!(r#"var dataSK={{"temp":"{sentinel}","weather":"晴"}}"#);
        let payload = parse_current_js(&body).expect("weather 有效");
        assert!(
            payload.temperature_c.is_none(),
            "D6：文本哨兵「{sentinel}」仍然表示缺失"
        );
    }
}

// ---------------------------------------------------------------------------
// D7：带单位字段只认前导数值
// ---------------------------------------------------------------------------

/// D7 缺陷（根因）：`json_unit_number` 用未锚定的 `-?\d+(?:\.\d+)?`，
/// 把 `12-30km` 截成 12.0、`1e3` 截成 1.0，畸形文本被包装成合法数值。
#[test]
fn d7_unit_fields_reject_malformed_numbers() {
    let bad = parse_current_js(r#"var dataSK={"wse":"12-30km","njd":"1e3","weather":"晴"}"#)
        .expect("weather 有效");
    assert!(
        bad.wind_speed_kmh.is_none(),
        "D7：`12-30km` 是区间，不是单个数值"
    );
    assert!(
        bad.visibility_km.is_none(),
        "D7：`1e3` 是指数记法，不是单个数值"
    );

    let dotted =
        parse_current_js(r#"var dataSK={"njd":"1.2.3","weather":"晴"}"#).expect("weather 有效");
    assert!(dotted.visibility_km.is_none(), "D7：`1.2.3` 是畸形小数");

    // 合法的「数值 + 单位」写法必须继续正确解析。
    let good = parse_current_js(r#"var dataSK={"wse":"3km/h","njd":"30km","weather":"晴"}"#)
        .expect("weather 有效");
    assert_eq!(good.wind_speed_kmh, Some(3.0), "D7：`3km/h` → 3.0");
    assert_eq!(good.visibility_km, Some(30.0), "D7：`30km` → 30.0");

    let bare =
        parse_current_js(r#"var dataSK={"njd":"9.1","weather":"晴"}"#).expect("weather 有效");
    assert_eq!(bare.visibility_km, Some(9.1), "D7：纯数值 `9.1` → 9.1");
}

// ---------------------------------------------------------------------------
// D9：发布时间跨零点回滚
// ---------------------------------------------------------------------------

/// D9 缺陷（根因）：`(fc_24h=None, update_time=Some(hh:mm))` 分支用「今天」拼日期，
/// 时钟 2026-11-07 00:20 + `update_time="23:00"` 会得到一个落在未来 22 小时的发布时间。
#[test]
fn d9_publish_time_rolls_back_across_midnight() {
    // 零点后不久抓到「昨天 23:00」的页面：必须回滚到 11-06，不能落在未来。
    let now = at(2026, 11, 7, 0, 20);
    assert_eq!(
        publish_datetime(None, Some("23:00"), now),
        Some(at(2026, 11, 6, 23, 0)),
        "D9：跨零点必须回滚 1 天"
    );

    // 正常情况下（update_time 早于现在）不许回滚。
    let now = at(2026, 11, 7, 9, 0);
    assert_eq!(
        publish_datetime(None, Some("08:00"), now),
        Some(at(2026, 11, 7, 8, 0)),
        "D9：同一自然日内的发布时间保持不动"
    );
}

/// D9 的第二条分支：`fc_24h` 自带完整年月日时，日期由数据源给定，
/// 因此**不需要** `now`，也不做回滚。
#[test]
fn d9_publish_time_with_base_date_ignores_now() {
    let now = at(2026, 11, 7, 0, 20);
    assert_eq!(
        publish_datetime(Some("2026-11-06 18:00"), Some("07:30"), now),
        Some(at(2026, 11, 6, 7, 30)),
        "D9：base 分支用 base 自己的年月日，与 now 无关"
    );
    // 只有 base、没有 update_time 时同样原样返回。
    assert_eq!(
        publish_datetime(Some("2026-11-06 18:00"), None, now),
        Some(at(2026, 11, 6, 18, 0))
    );
}

/// 回归保护：真实 7 日预报页面的发布时间仍然是 `2026-11-05 18:00`。
#[test]
fn d9_real_forecast_page_publish_time_unchanged() {
    let payload = parse_forecast_page(FORECAST_SAMPLE).expect("真实预报样本应可解析");
    assert_eq!(
        payload.published_at,
        Some(at(2026, 11, 5, 18, 0)),
        "D9：base 分支的既有行为必须保持不变"
    );
}

// ---------------------------------------------------------------------------
// D10：日期不存在时说明必须如实
// ---------------------------------------------------------------------------

/// D10 缺陷（根因）：`observed_at` 为 `None` 时 note 仍然宣称「已按北京时间解析」，
/// 让人误以为观测时间已经解出来了。
#[test]
fn d10_impossible_date_reports_honest_note() {
    // 02月30日 在任何年份都不存在：与闰年无关，断言因此是确定的。
    let payload = parse_current_js(r#"var dataSK={"date":"02月30日","time":"23:05","temp":"12"}"#)
        .expect("有气温，整体解析成功");
    assert!(
        payload.observed_at.is_none(),
        "D10：该日期不存在，观测时间无法确定"
    );
    let note = payload.observed_at_note.expect("必须有说明");
    assert!(
        note.contains("不存在") || note.contains("无法确定"),
        "D10：说明必须如实指出日期不存在 / 时间无法确定，实际：{note}"
    );
    assert!(
        !note.contains("年份由抓取日期推断"),
        "D10：不得继续宣称已按北京时间解析：{note}"
    );
    assert_eq!(payload.observed_at_raw.as_deref(), Some("02月30日 23:05"));

    // 02月29日：闰年合法、平年不存在。两种结果都必须与 note 一致。
    let leap = parse_current_js(r#"var dataSK={"date":"02月29日","time":"23:05","temp":"12"}"#)
        .expect("有气温，整体解析成功");
    let note = leap.observed_at_note.expect("必须有说明");
    if leap.observed_at.is_some() {
        assert!(
            note.contains("按北京时间"),
            "D10：解析成功时 note 应说明解析口径：{note}"
        );
    } else {
        assert!(
            note.contains("不存在") || note.contains("无法确定"),
            "D10：02月29日 落空时 note 不得撒谎：{note}"
        );
    }

    // 正常日期仍然给出「按北京时间解析」的说明。
    let ok = parse_current_js(r#"var dataSK={"date":"11月05日","time":"23:05","temp":"12"}"#)
        .expect("有气温，整体解析成功");
    assert!(ok.observed_at.is_some());
    assert!(
        ok.observed_at_note
            .expect("必须有说明")
            .contains("按北京时间"),
        "D10：可解析时说明口径不变"
    );
}

// ---------------------------------------------------------------------------
// D4：预警内联字段接受数字
// ---------------------------------------------------------------------------

/// D4 缺陷（根因）：局部闭包只认 `Value::as_str`，代码字段写成数字时所有字段都读成
/// `None`，`content` 为空、标题被兜底成「未命名气象预警…」，却仍然 push 进 `alerts`
/// 并让 `has_alerts = true` —— 凭空造出一条幽灵预警。
#[test]
fn d4_numeric_alert_fields_produce_real_alert() {
    let payload =
        parse_alerts_payload(r#"var alarmDZ101010100 ={"w":[{"w13":123,"w5":123,"w1":110000}]}"#)
            .expect("结构合法，字段是数字也应解析成功");
    assert_eq!(payload.alerts.len(), 1, "D4：不应丢条");
    let alert = &payload.alerts[0];

    assert_eq!(alert.title, "123", "D4：`w13` 是数字时标题应读成 \"123\"");
    assert_eq!(
        alert.alert_type.as_deref(),
        Some("123"),
        "D4：`w5` → alert_type"
    );
    assert_eq!(
        alert.region, "110000",
        "D4：`w1` 是数字时区域应读成 \"110000\""
    );
    assert!(
        !alert.title.contains("未命名"),
        "D4：不再出现幽灵预警的兜底标题，实际：{}",
        alert.title
    );

    // 真实样本字段全是字符串，行为不变。
    let real = parse_alerts_payload(ALERT_SAMPLE).expect("真实预警样本");
    assert_eq!(real.alerts.len(), 2);
    assert_eq!(real.alerts[0].alert_type.as_deref(), Some("大风"));
}

/// D3 收尾：被量程拒绝的字段必须在 `missing_fields` 里**显式可见**。
///
/// `precipitation_24h_mm`（`rain24h`）与 `aqi` 都加了物理量程校验（[`RANGE_PRECIPITATION_MM`]
/// / [`RANGE_AQI`]）。若它们既不出现在 JSON 里（`skip_serializing_if`）又不出现在
/// `missing_fields` 里，调用方就完全无法区分「上游没给」与「给了但被量程拒绝」——
/// 那就等于 D3 的诚实性在这两个字段上漏了一环。注意 `precipitation_mm` 一直是受追踪的，
/// 只有它的 24 小时版本不追踪，这个不对称本身就是缺陷。
#[test]
fn d3_range_rejected_fields_are_listed_in_missing_fields() {
    let payload = parse_current_js(r#"var dataSK={"temp":"12","aqi":"1e30","rain24h":"99999"}"#)
        .expect("temp 有效，整体应解析成功");
    assert!(payload.aqi.is_none(), "D5：1e30 超出 AQI 量程 0..=1000");
    assert!(
        payload.precipitation_24h_mm.is_none(),
        "D3：99999mm 超出降水 0..=3000"
    );

    let observation =
        payload.into_observation(city(), "test://sk".to_string(), at(2026, 11, 6, 9, 0));

    // rustfmt 要求的换行保持原样；下面这行是「实际内容」的证据来源。
    println!(
        "D3 收尾：{{\"temp\":\"12\",\"aqi\":\"1e30\",\"rain24h\":\"99999\"}} 的 missing_fields = {:?}",
        observation.missing_fields
    );

    // 一、越界字段不得出现在 JSON 输出里（缺失不序列化）。
    let json = serde_json::to_value(&observation).expect("观测可序列化");
    assert!(
        json.get("aqi").is_none(),
        "D3：越界 AQI 不得出现在 JSON 里：{json}"
    );
    assert!(
        json.get("precipitation_24h_mm").is_none(),
        "D3：越界 24h 降水不得出现在 JSON 里：{json}"
    );

    // 二、但它们必须出现在 missing_fields 里 —— 缺失必须显式可见。
    for field in ["aqi", "precipitation_24h_mm"] {
        assert!(
            observation.missing_fields.iter().any(|name| name == field),
            "D3：{field} 必须显式列进 missing_fields，实际 {:?}",
            observation.missing_fields
        );
    }

    // 三、反方向保护：真实样本有 aqi=16 / rain24h=0，补追踪后绝不能被误标成缺失。
    let real = parse_current_js(CURRENT_SAMPLE)
        .expect("真实样本应可解析")
        .into_observation(city(), "test://sk".to_string(), at(2026, 11, 6, 9, 0));
    assert_eq!(real.aqi, Some(16), "真实样本的 AQI 必须保留");
    assert_eq!(
        real.precipitation_24h_mm,
        Some(0.0),
        "真实样本的 0mm 是真实值"
    );
    for field in ["aqi", "precipitation_24h_mm"] {
        assert!(
            !real.missing_fields.iter().any(|name| name == field),
            "D3：真实样本提供了 {field}，不得标成缺失，实际 {:?}",
            real.missing_fields
        );
    }
}

/// D4 的边界：改闭包**不许**顺带改变 6 种既有异常形态的行为，它们仍然必须是
/// `parse_error`，而不是「没有预警」。
#[test]
fn d4_abnormal_w_shapes_still_error() {
    let cases: [(&str, &str); 6] = [
        (r#"var alarmDZ101010100 ={"w":null}"#, "w=null"),
        (r#"var alarmDZ101010100 ={"w":{}}"#, "w 是对象"),
        (r#"var alarmDZ101010100 ={"w":""}"#, "w 是字符串"),
        (r#"var alarmDZ101010100 ={"city":"101010100"}"#, "缺少 w"),
        (r#"var alarmDZ101010100 ={"w":[{"w1":"吉"#, "JSON 截断"),
        ("<html><body>403 Forbidden</body></html>", "HTML 错误页"),
    ];
    for (body, label) in cases {
        let err = parse_alerts_payload(body).expect_err("D4：必须报解析错误，不能当成没有预警");
        assert_eq!(err.code(), "parse_error", "D4：{label} 的错误码");
    }
}
