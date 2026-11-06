//! 解析器性质测试：**不使用 proptest**（离线环境没有该依赖，`Cargo.lock` 里也没有），
//! 而是手写一个确定性的 64 位 xorshift PRNG，用固定种子生成随机输入并断言不变式。
//!
//! 覆盖四类不变式：
//!
//! 1. 逐三小时：时间严格升序、无重复时点、点数不超过 [`MAX_HOURLY_POINTS`]、
//!    每个时点至少带一项气象信息、同输入两次解析完全一致；
//! 2. 顺序无关：同一组条目改变 `1d` 内部顺序后，输出时点集合不变；
//! 3. 日期推断：`Some(d)` 时 `d` 必须落在锚点当天到 7 天后、日号一致、月份只能是
//!    锚点当月或下一个月；
//! 4. 字段缺失：被标记为缺失的字段必须是 `None`，绝不会被填成 `0`。
//!
//! 所有随机性都来自固定种子，因此测试是完全可复现的：失败时打印的输入可以直接
//! 粘贴成单元测试。

use chrono::{Datelike, Duration, NaiveDate};
use weather_mcp_cn::providers::weather_com_cn::{
    MAX_HOURLY_POINTS, parse_current_js, parse_hourly_page, resolve_forecast_date,
};

// ---------------------------------------------------------------------------
// 手写 PRNG（64 位 xorshift，无任何外部依赖）
// ---------------------------------------------------------------------------

/// 64 位 xorshift PRNG。
///
/// ```text
/// x ^= x << 13;  x ^= x >> 7;  x ^= x << 17;
/// ```
///
/// 状态必须非零。种子里 `| 1` 保证这一点，否则全零状态会永远停在零。
struct Rng(u64);

impl Rng {
    /// 用固定种子构造。**测试里绝不允许使用真随机种子**，否则失败无法复现。
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `0..bound` 的均匀取值（`bound` 为 0 时返回 0）。
    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    /// 闭区间 `[low, high]`。
    fn range_inclusive(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }

    /// Fisher–Yates 洗牌。
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below(index as u64 + 1) as usize;
            items.swap(index, other);
        }
    }
}

// ---------------------------------------------------------------------------
// 逐三小时：随机输入生成
// ---------------------------------------------------------------------------

/// 上游可能的时点小时值（3 小时粒度）。
const HOURS: [u32; 8] = [2, 5, 8, 11, 14, 17, 20, 23];
const WEATHERS: [&str; 5] = ["晴", "多云", "小雨", "阴", "雨夹雪"];
const WINDS: [&str; 4] = ["西风", "东南风", "北风", "东北风"];

fn json_array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

/// 生成一条随机时点记录（JSON 字面量，可能是 `null`，也可能是残缺/非法记录）。
fn random_entry(rng: &mut Rng, day: u32) -> String {
    match rng.below(10) {
        // `null` 条目：结构未知，解析器只能跳过。
        0 => "null".to_string(),
        // 只有时间戳，没有逗号分隔的字段。
        1 => format!("\"{day:02}日11时\""),
        // 字段数不足 6 段。
        2 => format!("\"{day:02}日11时,d00,晴,12℃\""),
        // 非法日号（32 日之后都不存在）。
        3 => format!(
            "\"{}日11时,d00,晴,12℃,西风,<3级,1\"",
            rng.range_inclusive(32, 99)
        ),
        // 空记录：只有时间戳，没有任何气象信息。
        4 => format!("\"{day:02}日11时,,,,,\""),
        // 完整记录。
        _ => {
            let hour = HOURS[rng.below(HOURS.len() as u64) as usize];
            let weather = WEATHERS[rng.below(WEATHERS.len() as u64) as usize];
            let wind = WINDS[rng.below(WINDS.len() as u64) as usize];
            let temperature = rng.range_inclusive(1, 30);
            format!("\"{day:02}日{hour:02}时,d00,{weather},{temperature}℃,{wind},<3级,1\"")
        }
    }
}

/// 生成一份把随机条目散布到 `1d` / `23d` / `7d` 的完整页面。
fn random_hourly_doc(rng: &mut Rng) -> String {
    let mut fields: Vec<String> = Vec::new();

    // `1d`：扁平数组。
    let mut entries: Vec<String> = Vec::new();
    for _ in 0..rng.below(9) {
        let day = 1 + rng.below(40) as u32;
        entries.push(random_entry(rng, day));
    }
    fields.push(format!("\"1d\":{}", json_array(&entries)));

    // `23d` / `7d`：数组的数组（每天一个内层数组）。
    for key in ["23d", "7d"] {
        let mut days: Vec<String> = Vec::new();
        for _ in 0..rng.below(4) {
            let mut day_entries: Vec<String> = Vec::new();
            for _ in 0..rng.below(9) {
                let day = 1 + rng.below(40) as u32;
                day_entries.push(random_entry(rng, day));
            }
            days.push(json_array(&day_entries));
        }
        fields.push(format!("\"{key}\":{}", json_array(&days)));
    }

    format!(
        "<html><body><input type=\"hidden\" id=\"fc_3h_internal_update_time\" value=\"2026-11-06 08:00\"/><script>var hour3data={{{}}};</script></body></html>",
        fields.join(",")
    )
}

/// 解析一份页面并取出「时间 + 气温 + 天气」的点集合（按时间排序，便于比较）。
fn point_set(entries: &[String]) -> Vec<(String, Option<i32>, Option<String>)> {
    let html = format!(
        "<html><body><input type=\"hidden\" id=\"fc_3h_internal_update_time\" value=\"2026-11-06 08:00\"/><script>var hour3data={{\"1d\":{}}};</script></body></html>",
        json_array(entries)
    );
    let payload = parse_hourly_page(&html).expect("生成的记录都应可解析");
    let mut points: Vec<(String, Option<i32>, Option<String>)> = payload
        .points
        .iter()
        .map(|point| {
            (
                point.time.format("%Y-%m-%d %H:%M").to_string(),
                point.temperature_c,
                point.weather.clone(),
            )
        })
        .collect();
    points.sort();
    points
}

// ---------------------------------------------------------------------------
// 性质 1：逐三小时的排序 / 去重 / 上限 / 非空壳 / 确定性
// ---------------------------------------------------------------------------

#[test]
fn property_hourly_points_are_sorted_bounded_and_never_empty_shells() {
    // 固定种子：失败可复现。
    let mut rng = Rng::new(0x51ED_2701_00C0_FFEE);
    let mut succeeded = 0u32;

    for _case in 0..300 {
        let html = random_hourly_doc(&mut rng);
        let payload = match parse_hourly_page(&html) {
            Ok(payload) => payload,
            Err(err) => {
                // 随机输入只允许以「结构上没有可用数据」的 parse_error 失败，
                // 绝不能 panic、也不能出现别的错误类别。
                assert_eq!(
                    err.code(),
                    "parse_error",
                    "随机输入只允许以 parse_error 失败"
                );
                continue;
            }
        };
        succeeded += 1;

        let points = &payload.points;
        assert!(!points.is_empty(), "解析成功时至少有一个时点");
        assert!(
            points.len() <= MAX_HOURLY_POINTS,
            "点数不得超过 MAX_HOURLY_POINTS：{}",
            points.len()
        );

        // (a) 时间严格升序 → 同时蕴含 (b) 无重复时点。
        for window in points.windows(2) {
            assert!(
                window[0].time < window[1].time,
                "时间必须严格升序且无重复：{:?} / {:?}",
                window[0].time,
                window[1].time
            );
        }

        // (d) 每个点至少有一项气象信息：不允许「只有时间」的空壳时点。
        for point in points {
            assert!(
                point.weather.is_some()
                    || point.temperature_c.is_some()
                    || point.wind_direction.is_some()
                    || point.wind_scale.is_some(),
                "不允许出现只有时间的空壳时点：{point:?}"
            );
        }

        // (e) 同输入两次解析结果完全一致（确定性）。
        let again = parse_hourly_page(&html).expect("同一输入必须同样成功");
        assert_eq!(again.points.len(), points.len(), "两次解析点数必须一致");
        for (first, second) in points.iter().zip(again.points.iter()) {
            assert_eq!(first.time, second.time);
            assert_eq!(first.temperature_c, second.temperature_c);
            assert_eq!(first.weather, second.weather);
            assert_eq!(first.wind_direction, second.wind_direction);
            assert_eq!(first.wind_scale, second.wind_scale);
        }
    }

    assert!(
        succeeded > 50,
        "随机样本里应有足够多的成功解析，实际 {succeeded}"
    );
}

// ---------------------------------------------------------------------------
// 性质 2：同一组条目改变字段内部顺序，输出时点集合不变
// ---------------------------------------------------------------------------

#[test]
fn property_hourly_field_order_does_not_change_the_point_set() {
    let mut rng = Rng::new(0x00C0_FFEE_1234_5678);
    let mut cases = 0u32;

    for _case in 0..200 {
        // 日号互不相同 ⇒ 时点互不相同，因此这里**只**测顺序无关性：
        // 重复时间戳该由谁胜出（空记录 vs 有效记录）本身可能与顺序有关，
        // 那属于另一条语义，避免混进来。
        let count = rng.range_inclusive(1, 20) as u32;
        let mut entries: Vec<String> = Vec::new();
        for index in 0..count {
            let day = index + 1;
            let hour = HOURS[rng.below(HOURS.len() as u64) as usize];
            let weather = WEATHERS[rng.below(WEATHERS.len() as u64) as usize];
            let wind = WINDS[rng.below(WINDS.len() as u64) as usize];
            let temperature = rng.range_inclusive(1, 30);
            entries.push(format!(
                "\"{day:02}日{hour:02}时,d00,{weather},{temperature}℃,{wind},<3级,1\""
            ));
        }

        let forward = point_set(&entries);
        rng.shuffle(&mut entries);
        let shuffled = point_set(&entries);

        assert_eq!(
            forward, shuffled,
            "同一组条目换顺序后，时点集合（时间 + 温度 + 天气）必须完全相同"
        );
        cases += 1;
    }

    assert!(cases >= 200, "样本数不足：{cases}");
}

// ---------------------------------------------------------------------------
// 性质 3：7 日预报的日期推断
// ---------------------------------------------------------------------------

#[test]
fn property_forecast_dates_stay_within_two_months_and_seven_days() {
    let mut rng = Rng::new(0x0DDB_A117_0000_0777);
    let epoch = NaiveDate::from_ymd_opt(2024, 1, 1).expect("合法基准日期");
    // 2024-01-01 ..= 2030-12-31 共 2557 天。
    let span_days = 2557u64;
    let mut resolved = 0u32;

    for _case in 0..800 {
        let offset = rng.below(span_days) as i64;
        let anchor = epoch
            .checked_add_signed(Duration::days(offset))
            .expect("仍在可表示范围内");
        // 含 0 与 32..=40 这些必然无效的日号。
        let day_of_month = rng.below(41) as u32;

        let Some(date) = resolve_forecast_date(anchor, day_of_month) else {
            continue;
        };
        resolved += 1;

        assert!(date >= anchor, "日期不得早于锚点：{anchor} → {date}");
        assert!(
            (date - anchor).num_days() <= 7,
            "日期不得晚于锚点 7 天：{anchor} → {date}"
        );
        assert_eq!(
            date.day(),
            day_of_month,
            "解析出的日期必须与标题里的日号一致：{anchor} + {day_of_month}日 → {date}"
        );

        // 月份只能是锚点当月或下一个月（下个月要正确处理跨年）。
        let (next_year, next_month) = if anchor.month() == 12 {
            (anchor.year() + 1, 1)
        } else {
            (anchor.year(), anchor.month() + 1)
        };
        let same_month = date.year() == anchor.year() && date.month() == anchor.month();
        let following_month = date.year() == next_year && date.month() == next_month;
        assert!(
            same_month || following_month,
            "月份只能是锚点当月或下一个月：{anchor} + {day_of_month}日 → {date}"
        );
    }

    assert!(resolved > 100, "随机样本里应有足够多的成功解析：{resolved}");
}

// ---------------------------------------------------------------------------
// 性质 4：缺失字段绝不被填成 0
// ---------------------------------------------------------------------------

/// 每个字段的四种随机形态：缺失 / 文本哨兵 / 超量程 / 合法值。
const FIELD_CASES: [(&str, &str, &str, &str); 4] = [
    // (键, 合法值, 文本哨兵, 超量程值)
    ("temp", "12.5", "暂无", "999"),
    ("SD", "55", "N/A", "250"),
    ("qy", "1005", "-", "1"),
    ("njd", "12", "null", "-5"),
];

#[test]
fn property_missing_fields_are_never_filled_with_zero() {
    let mut rng = Rng::new(0x1BAD_5EED_0F0F_0001);
    let mut cases = 0u32;

    for _case in 0..400 {
        // `weather` 恒定有效，保证「至少命中一个气象字段」的门槛不会掩盖真正要测的东西。
        let mut body = String::from(r#"var dataSK={"weather":"晴""#);
        let mut expectations: Vec<(&str, Option<f64>)> = Vec::new();

        for (key, valid, sentinel, out_of_range) in FIELD_CASES {
            let literal = match rng.below(4) {
                // 字段整体缺失。
                0 => None,
                // 文本哨兵。
                1 => Some(sentinel),
                // 超出物理量程。
                2 => Some(out_of_range),
                // 合法值。
                _ => Some(valid),
            };
            if let Some(literal) = literal {
                body.push_str(&format!(",\"{key}\":\"{literal}\""));
            }
            let expected = if literal == Some(valid) {
                Some(valid.parse::<f64>().expect("字面量是合法数字"))
            } else {
                None
            };
            expectations.push((key, expected));
        }
        body.push('}');

        let payload = parse_current_js(&body).expect("weather 恒定有效，必须解析成功");
        for (key, expected) in expectations {
            let actual = match key {
                "temp" => payload.temperature_c,
                "SD" => payload.humidity_percent,
                "qy" => payload.pressure_hpa,
                "njd" => payload.visibility_km,
                other => panic!("未知字段 {other}"),
            };
            assert_eq!(
                actual, expected,
                "字段 {key} 的期望值与实际值不符（输入：{body}）"
            );
            assert_ne!(
                actual,
                Some(0.0),
                "字段 {key} 绝不能被填成 0（输入：{body}）"
            );
        }
        cases += 1;
    }

    assert!(cases >= 400, "样本数不足：{cases}");
}
