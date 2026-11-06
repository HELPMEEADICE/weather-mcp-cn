//! 逐 3 小时预报「真实覆盖时长」的回归测试。
//!
//! 核心命题：`covered_hours` 必须是「请求窗口」与「上游数据区间并集」的真实交集长度，
//! 而不是「从 now 到最后一个时点结束」的长度。旧实现只看最后一个时点，一旦上游数据
//! 有意外的开始时间或中间缺口，就会把没有数据的时段算成已覆盖。
//!
//! 这些用例完全确定：`now` 由测试注入，不读系统时钟、不访问网络。

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, TimeZone};
use weather_mcp_cn::models::{HourlyForecast, cst_offset};
use weather_mcp_cn::window::{self, RESOLUTION_HOURS};

fn cst(year: i32, month: u32, day: u32, hour: u32) -> DateTime<FixedOffset> {
    cst_minute(year, month, day, hour, 0)
}

fn cst_minute(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<FixedOffset> {
    cst_offset()
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .expect("合法时间")
}

fn point_at(time: DateTime<FixedOffset>, temperature: i32) -> HourlyForecast {
    HourlyForecast {
        time,
        weather: Some("晴".into()),
        temperature_c: Some(temperature),
        wind_direction: None,
        wind_scale: None,
        icon_code: None,
    }
}

/// 从 `start` 起每 3 小时一个点，共 `count` 个。
fn series(start: DateTime<FixedOffset>, count: usize) -> Vec<HourlyForecast> {
    (0..count)
        .map(|index| {
            point_at(
                start + ChronoDuration::hours(RESOLUTION_HOURS * index as i64),
                index as i32,
            )
        })
        .collect()
}

/// 用户报告的场景：现在是 10:00，请求未来 12 小时，上游只给了 19:00 一个时点。
///
/// 19:00 的三小时区间 `[19:00, 22:00)` 全部落在窗口 `[10:00, 22:00)` 内，真实覆盖
/// 只有 3 小时。旧实现取「最后一个时点 + 3h」与截止时刻的较小值，得到 22:00，
/// 于是报告 covered_hours = 12 —— 比真实数据多出 9 小时。
#[test]
fn only_the_last_three_hours_does_not_claim_twelve() {
    let now = cst(2026, 11, 6, 10);
    let points = series(cst(2026, 11, 6, 19), 1);

    let selection = window::select(&points, now, 12).expect("19:00 仍覆盖窗口尾部");
    assert_eq!(selection.points.len(), 1);
    assert_eq!(
        selection.covered_hours, 3,
        "窗口内只有 19:00~22:00 这 3 小时有数据，不能报告 12"
    );
    assert_eq!(
        selection.coverage_end,
        cst(2026, 11, 6, 22),
        "最后一小段数据的结束时刻仍然是 22:00"
    );
}

/// 中间有缺口：`[11:00, 14:00)` 与 `[17:00, 20:00)` 共 6 小时，中间 14:00~17:00 没有数据。
#[test]
fn gap_between_buckets_is_not_counted_as_coverage() {
    let now = cst(2026, 11, 6, 10);
    let points = vec![
        point_at(cst(2026, 11, 6, 11), 1),
        point_at(cst(2026, 11, 6, 17), 2),
    ];

    let selection = window::select(&points, now, 12).expect("部分覆盖仍应成功");
    assert_eq!(selection.points.len(), 2);
    assert_eq!(
        selection.covered_hours, 6,
        "3 小时 + 3 小时，缺口不能算成覆盖"
    );
    assert_eq!(selection.contiguous_hours, 0, "从 10:00 起就没有数据");
    assert_eq!(selection.coverage_end, cst(2026, 11, 6, 20));
}

/// 从当前时刻起先连续覆盖一段、之后才出现缺口。
#[test]
fn contiguous_hours_stop_at_the_first_gap() {
    let now = cst(2026, 11, 6, 10);
    let points = vec![
        point_at(cst(2026, 11, 6, 10), 1),
        point_at(cst(2026, 11, 6, 17), 2),
    ];

    let selection = window::select(&points, now, 12).expect("部分覆盖仍应成功");
    assert_eq!(selection.covered_hours, 6, "累计覆盖 [10,13) + [17,20)");
    assert_eq!(
        selection.contiguous_hours, 3,
        "只能连续覆盖到 13:00，缺口之后的不算连续"
    );
}

/// 时点恰好等于请求窗口结束时刻：它描述的时段整体在窗口之外，不能算覆盖。
#[test]
fn bucket_exactly_at_the_deadline_covers_nothing() {
    let now = cst(2026, 11, 6, 10);
    let points = series(cst(2026, 11, 6, 11), 1); // 11:00 == now + 1h

    let miss = window::select(&points, now, 1).expect_err("11:00 的区间整体在窗口之外，必须报错");
    assert_eq!(miss.reference_time, now);
    assert_eq!(miss.deadline, cst(2026, 11, 6, 11));
    assert_eq!(miss.available_from, Some(cst(2026, 11, 6, 11)));
    let message = miss.message(1);
    assert!(message.contains("没有覆盖未来 1 小时"), "{message}");
}

/// 部分小时覆盖向下取整，不做四舍五入、也不补足 1 小时。
#[test]
fn partial_hour_coverage_is_floored() {
    // now = 10:20，数据覆盖 [10:00, 13:00) 与 [17:00, 20:00)。
    // 与窗口 [10:20, 22:20) 的交集是 2h40m + 3h = 5h40m → 5 小时。
    let now = cst_minute(2026, 11, 6, 10, 20);
    let points = vec![
        point_at(cst(2026, 11, 6, 10), 1),
        point_at(cst(2026, 11, 6, 17), 2),
    ];

    let selection = window::select(&points, now, 12).expect("部分覆盖仍应成功");
    assert_eq!(selection.covered_hours, 5, "5h40m 向下取整为 5 小时");
    assert_eq!(
        selection.contiguous_hours, 2,
        "从 10:20 到 13:00 是 2h40m，向下取整 2 小时"
    );
}

/// 重叠 / 重复 / 乱序的时点不能重复计数，也不能把区间合并算错。
#[test]
fn overlapping_and_duplicate_points_are_merged() {
    let now = cst(2026, 11, 6, 10);
    let points = vec![
        point_at(cst(2026, 11, 6, 14), 3),
        point_at(cst(2026, 11, 6, 11), 1),
        point_at(cst(2026, 11, 6, 11), 1), // 完全重复
        point_at(cst(2026, 11, 6, 12), 2), // 与 [11,14) 重叠
    ];

    let selection = window::select(&points, now, 12).expect("应有覆盖");
    assert_eq!(selection.points.len(), 3, "重复时点只保留一个");
    assert_eq!(
        selection.covered_hours, 6,
        "[11,14) ∪ [12,15) ∪ [14,17) = [11,17) 共 6 小时"
    );
    assert_eq!(selection.contiguous_hours, 0, "10:00~11:00 没有数据");
}

/// 不规则时点（上游异常返回 30/15 分钟偏移）也必须按区间求交集，而不是按点数 ×3。
#[test]
fn irregular_points_use_real_intervals() {
    let now = cst(2026, 11, 6, 10);
    let points = vec![
        point_at(cst_minute(2026, 11, 6, 10, 30), 1), // [10:30, 13:30)
        point_at(cst_minute(2026, 11, 6, 13, 30), 2), // [13:30, 16:30)
        point_at(cst_minute(2026, 11, 6, 20, 15), 3), // [20:15, 23:15) → 截到 22:00
    ];

    let selection = window::select(&points, now, 12).expect("应有覆盖");
    // [10:30,16:30) = 6h；[20:15,22:00) = 1h45m；合计 7h45m → 7。
    assert_eq!(selection.covered_hours, 7);
    assert_eq!(selection.contiguous_hours, 0, "10:00~10:30 没有数据");
    assert_eq!(selection.coverage_end, cst(2026, 11, 6, 22), "截止时刻夹住");
}

/// 跨年 + 缺口：UTC+8 下没有夏令时，跨年只影响日期显示，不影响时长计算。
#[test]
fn across_new_year_with_a_gap() {
    let now = cst_minute(2026, 12, 31, 22, 30);
    let points = vec![
        point_at(cst(2026, 12, 31, 22), 1), // [22:00, 01:00)
        point_at(cst(2027, 1, 1, 4), 2),    // [04:00, 07:00)，01:00~04:00 缺口
    ];

    let selection = window::select(&points, now, 9).expect("应有覆盖");
    assert_eq!(selection.points.len(), 2);
    // 窗口 [22:30, 07:30)：交集 = [22:30,01:00) 2h30m + [04:00,07:00) 3h = 5h30m。
    assert_eq!(selection.covered_hours, 5);
    assert_eq!(selection.contiguous_hours, 2, "22:30 → 01:00 是 2h30m");
    assert_eq!(selection.coverage_end, cst(2027, 1, 1, 7));
}

/// 闰年 2 月 29 日之后同样按真实时长计算。
#[test]
fn across_leap_day_with_a_gap() {
    let now = cst(2028, 2, 28, 23);
    let points = vec![
        point_at(cst(2028, 2, 28, 23), 1), // [23:00, 02:00)
        point_at(cst(2028, 2, 29, 5), 2),  // 02:00~05:00 缺口，[05:00, 08:00)
    ];

    let selection = window::select(&points, now, 9).expect("应有覆盖");
    assert_eq!(selection.covered_hours, 6);
    assert_eq!(selection.contiguous_hours, 3);
}

/// 全部数据都在窗口之外时，必须给出结构化诊断而不是「成功但没数据」。
#[test]
fn data_entirely_after_the_window_is_a_structured_miss() {
    let now = cst(2026, 11, 6, 10);
    let points = series(cst(2026, 11, 6, 20), 3); // 20:00 / 23:00 / 次日 02:00

    let miss = window::select(&points, now, 6).expect_err("窗口内没有任何数据");
    assert_eq!(miss.available_from, Some(cst(2026, 11, 6, 20)));
    assert_eq!(miss.available_until, Some(cst(2026, 11, 7, 5)));
    let message = miss.message(6);
    assert!(message.contains("上游当前可用区间"), "{message}");
}
