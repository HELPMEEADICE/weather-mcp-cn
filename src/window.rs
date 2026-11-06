//! 逐 3 小时预报的时间窗口裁剪。
//!
//! 上游只给 3 小时粒度，返回的时点是**区间起点**：一个 `11:00` 的时点代表
//! `[11:00, 14:00)` 这段时间的预报。所以时间窗必须按区间来判定，而不是按时间点，
//! 否则「现在是 11:30，11:00 这个时点算过去还是现在」永远说不清。
//!
//! # 时点 → 区间
//!
//! 每个保留的时点 `t` 映射成半开区间 `[t, t + 3h)`。请求窗口同样按半开区间处理：
//! `[now, now + requested_hours)`。窗口内的**真实覆盖**是两者的交集并集：
//!
//! ```text
//! segments = merge( { [max(t, now), min(t + 3h, deadline)) | t 保留且 min > max } )
//! ```
//!
//! 区间先按起点排序，再合并重叠或首尾相接的相邻区间。这样重叠时点不会被重复计数，
//! 中间的空洞也不会被算成覆盖。
//!
//! | 分类 | 条件 | 处理 |
//! | --- | --- | --- |
//! | 过去 | `t + 3h <= now` | 丢弃（区间已经整体过去） |
//! | 当前附近 | `t <= now < t + 3h` | 保留（正在生效的预报） |
//! | 未来 | `now < t` 且 `t <= now + hours` | 保留 |
//! | 超出窗口 | `t > now + hours` | 丢弃 |
//!
//! 恰好等于截止时刻的时点会被保留在 [`WindowSelection::points`] 里（它仍然是有用的
//! 边界信息，也是既有输出的一部分），但它的区间 `[deadline, deadline + 3h)` 与窗口的
//! 交集为空，因此**不贡献任何覆盖时长**。
//!
//! # 两个覆盖时长
//!
//! ```text
//! covered_hours    = floor( 所有交集区间长度之和 / 1h )   // 窗口内累计有多少小时有数据
//! contiguous_hours = floor( 从 now 起连续不断的交集长度 / 1h )  // 从此刻起能连续回答多久
//! ```
//!
//! 两者都 <= `requested_hours`，也都不超过上游数据本身。它们回答的是不同问题：
//!
//! * 上游给了 `10:00`、`17:00` 两个时点、请求 12 小时 → `covered_hours = 6`、
//!   `contiguous_hours = 3`（缺口之后的数据不能用来回答「接下来连续 6 小时怎么样」）；
//! * 上游只有 `19:00` 一个时点、请求 12 小时 → `covered_hours = 3`、
//!   `contiguous_hours = 0`（从 10:00 起没有任何连续数据）。
//!
//! 旧实现只取「最后一个时点 + 3h」与截止时刻的较小值，于是上例会被报成 12 小时 ——
//! 把没有数据的时段算成了已覆盖。现在改为按真实交集计算，**不可能被夸大**。
//!
//! 本模块只做筛选与度量，不插值、不补点、不改变上游粒度。

use chrono::{DateTime, Duration as ChronoDuration, FixedOffset};

use crate::models::HourlyForecast;

/// 上游预报的固定粒度（小时）。
pub const RESOLUTION_HOURS: i64 = 3;

/// 一次剪裁允许的最大窗口（小时）。
///
/// 只用于挡住离谱参数导致的日期溢出，不代表产品上限 —— MCP 层的 `hours` 被夹在
/// `1..=24`。取 400 天，远大于任何真实需求，又离 `DateTime` 的上界足够远。
pub const MAX_WINDOW_HOURS: i64 = 24 * 400;

/// 一次窗口裁剪的结果。
#[derive(Debug, Clone)]
pub struct WindowSelection {
    /// 落在窗口内的时点（保持上游 3 小时粒度，按时间升序、彼此不重复）。
    pub points: Vec<HourlyForecast>,
    /// 裁剪基准时刻（本次调用的「现在」）。
    pub reference_time: DateTime<FixedOffset>,
    /// 请求的截止时刻，等于 `reference_time + requested_hours`。
    pub deadline: DateTime<FixedOffset>,
    /// 最后一段**真实交集**的结束时刻，因此不会超过 [`WindowSelection::deadline`]。
    pub coverage_end: DateTime<FixedOffset>,
    /// 窗口内累计被覆盖的整小时数（允许中间有缺口）。
    pub covered_hours: u32,
    /// 从 [`WindowSelection::reference_time`] 起连续被覆盖的整小时数（遇到第一个缺口即止）。
    pub contiguous_hours: u32,
}

/// 请求的时间窗完全没有被任何有效时点覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowMiss {
    /// 裁剪基准时刻。
    pub reference_time: DateTime<FixedOffset>,
    /// 请求的截止时刻。
    pub deadline: DateTime<FixedOffset>,
    /// 上游数据里最早的时点。
    pub available_from: Option<DateTime<FixedOffset>>,
    /// 上游数据里最后一个时点所在区间的结束时刻。
    pub available_until: Option<DateTime<FixedOffset>>,
}

impl WindowMiss {
    /// 面向调用方的说明，包含「请求窗口」与「上游可用区间」，便于 Agent 自行改写查询。
    pub fn message(&self, requested_hours: u32) -> String {
        let available = match (self.available_from, self.available_until) {
            (Some(from), Some(until)) => format!(
                "上游当前可用区间为 {} ~ {}",
                from.format("%Y-%m-%d %H:%M"),
                until.format("%Y-%m-%d %H:%M")
            ),
            (Some(from), None) => {
                format!("上游最早的时点是 {}", from.format("%Y-%m-%d %H:%M"))
            }
            _ => "上游没有返回任何可解析的预报时点".to_string(),
        };
        format!(
            "中国天气网没有覆盖未来 {requested_hours} 小时的逐 3 小时预报：\
             请求窗口 {} ~ {} 内没有任何有效时点；{available}",
            self.reference_time.format("%Y-%m-%d %H:%M"),
            self.deadline.format("%Y-%m-%d %H:%M"),
        )
    }
}

/// 时间轴上的一段半开区间 `[start, end)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: DateTime<FixedOffset>,
    end: DateTime<FixedOffset>,
}

/// 时点所在区间的结束时刻。
///
/// 用带检查的加法：上游理论上不会给出接近 `DateTime` 上界的时点，但解析层不保证
/// 这一点，这里不允许因为一个畸形时点就 panic。
fn bucket_end(time: DateTime<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    time.checked_add_signed(ChronoDuration::hours(RESOLUTION_HOURS))
}

/// 把时点映射成裁剪到 `[now, deadline)` 内的区间，排序后合并重叠与首尾相接的区间。
fn merged_coverage(
    points: &[HourlyForecast],
    now: DateTime<FixedOffset>,
    deadline: DateTime<FixedOffset>,
) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    for point in points {
        // 交集下界：时点起点与 now 的较晚者；上界：区间终点与 deadline 的较早者。
        let start = point.time.max(now);
        // 无法表示区间终点时（理论上不可能），按「一直覆盖到窗口结束」处理。
        let end = bucket_end(point.time).map_or(deadline, |end| end.min(deadline));
        if end > start {
            segments.push(Segment { start, end });
        }
    }

    segments.sort_by_key(|segment| segment.start);
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match merged.last_mut() {
            // `<=` 而不是 `<`：`[11,14)` 与 `[14,17)` 首尾相接，合并后才是连续的 6 小时。
            Some(last) if segment.start <= last.end => {
                if segment.end > last.end {
                    last.end = segment.end;
                }
            }
            _ => merged.push(segment),
        }
    }
    merged
}

/// 从 `now` 起连续不断的覆盖秒数（遇到第一个缺口即停）。
fn contiguous_seconds(segments: &[Segment], now: DateTime<FixedOffset>) -> i64 {
    let mut cursor = now;
    let mut covered = 0i64;
    for segment in segments {
        if segment.start > cursor {
            break;
        }
        covered = (segment.end - now).num_seconds().max(0);
        cursor = segment.end;
    }
    covered
}

/// 构造「窗口无数据」诊断。
fn miss(
    points: &[HourlyForecast],
    now: DateTime<FixedOffset>,
    deadline: DateTime<FixedOffset>,
) -> WindowMiss {
    WindowMiss {
        reference_time: now,
        deadline,
        available_from: points.iter().map(|point| point.time).min(),
        available_until: points
            .iter()
            .map(|point| point.time)
            .max()
            .and_then(bucket_end),
    }
}

/// 裁剪出 `[now, now + requested_hours)` 内真正有数据的预报时点。
///
/// 没有任何有效覆盖时返回 [`WindowMiss`]，由调用方转换成工具错误 —— 绝不返回空数组
/// 让调用方误以为「查询成功但没有数据」。
pub fn select(
    points: &[HourlyForecast],
    now: DateTime<FixedOffset>,
    requested_hours: i64,
) -> Result<WindowSelection, WindowMiss> {
    let hours = requested_hours.clamp(0, MAX_WINDOW_HOURS);
    let deadline = now + ChronoDuration::hours(hours);

    // 空窗口：既没有时点也没有覆盖。MCP 层把 hours 夹在 1..=24，这条分支只是让
    // 纯函数在 hours = 0 时的语义明确下来（而不是报「窗口无数据」）。
    if hours == 0 {
        return Ok(WindowSelection {
            points: Vec::new(),
            reference_time: now,
            deadline,
            coverage_end: now,
            covered_hours: 0,
            contiguous_hours: 0,
        });
    }

    let mut selected: Vec<HourlyForecast> = Vec::new();
    for point in points {
        // 区间整体落在过去：丢掉。
        if bucket_end(point.time).is_some_and(|end| end <= now) {
            continue;
        }
        // 起点超出截止时刻：丢掉。恰好等于截止时刻的边界时点保留（见模块文档：
        // 它不贡献覆盖，但仍是有用的边界信息）。
        if point.time > deadline {
            continue;
        }
        selected.push(point.clone());
    }

    // 上游数据本应有序，这里仍然排序 + 去重：窗口语义依赖顺序，不能依赖上游的自觉。
    selected.sort_by_key(|point| point.time);
    selected.dedup_by_key(|point| point.time);

    if selected.is_empty() {
        return Err(miss(points, now, deadline));
    }

    let segments = merged_coverage(&selected, now, deadline);
    let Some(last) = segments.last().copied() else {
        // 时点存在，但没有任何区间真正落进窗口 —— 例如唯一的时点恰好等于截止时刻。
        // 「有时点」不等于「有覆盖」，必须如实报错。
        return Err(miss(points, now, deadline));
    };

    let total_seconds: i64 = segments
        .iter()
        .map(|segment| (segment.end - segment.start).num_seconds().max(0))
        .sum();
    let contiguous = contiguous_seconds(&segments, now);

    Ok(WindowSelection {
        points: selected,
        reference_time: now,
        deadline,
        coverage_end: last.end,
        // 交集长度按定义不会超过窗口，这里的 clamp 只是把不变量写死在类型边界上。
        covered_hours: (total_seconds / 3600).clamp(0, hours) as u32,
        contiguous_hours: (contiguous / 3600).clamp(0, hours) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::cst_offset;
    use chrono::TimeZone;

    fn cst(year: i32, month: u32, day: u32, hour: u32) -> DateTime<FixedOffset> {
        cst_minute(year, month, day, hour, 0)
    }

    fn cst_minute(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
    ) -> DateTime<FixedOffset> {
        cst_offset()
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("合法时间")
    }

    fn point_at(time: DateTime<FixedOffset>) -> HourlyForecast {
        HourlyForecast {
            time,
            weather: Some("晴".into()),
            temperature_c: Some(1),
            wind_direction: None,
            wind_scale: None,
            icon_code: None,
        }
    }

    /// 从 `start` 起每 3 小时一个点，共 `count` 个。
    fn series(start: DateTime<FixedOffset>, count: usize) -> Vec<HourlyForecast> {
        (0..count)
            .map(|index| point_at(start + ChronoDuration::hours(RESOLUTION_HOURS * index as i64)))
            .collect()
    }

    #[test]
    fn drops_buckets_that_are_entirely_in_the_past() {
        // 现在的 01:00 正好是 22:00 那个区间的结束时刻：它已经整体过去，必须丢弃。
        let points = series(cst(2026, 11, 5, 22), 4);
        let selection = select(&points, cst(2026, 11, 6, 1), 12).expect("应有覆盖");
        let times: Vec<String> = selection
            .points
            .iter()
            .map(|point| point.time.format("%d %H:%M").to_string())
            .collect();
        assert_eq!(
            times,
            vec!["06 01:00", "06 04:00", "06 07:00"],
            "22:00 的区间已在 01:00 结束，不能算未来预报"
        );
        assert_eq!(selection.coverage_end, cst(2026, 11, 6, 10));
        assert_eq!(selection.covered_hours, 9, "01:00 → 10:00");
        assert_eq!(selection.contiguous_hours, 9, "这三个区间首尾相接");
    }

    #[test]
    fn keeps_the_covering_bucket_as_current() {
        // 现在 23:30，23:00 的区间 [23:00, 02:00) 仍覆盖未来，必须保留。
        let points = series(cst(2026, 11, 5, 23), 6);
        let selection = select(&points, cst_minute(2026, 11, 5, 23, 30), 1).expect("应有覆盖");
        assert_eq!(selection.points.len(), 1);
        assert_eq!(selection.points[0].time, cst(2026, 11, 5, 23));
        assert_eq!(
            selection.covered_hours, 1,
            "窗口只到 00:30，覆盖时长必须按截止时刻截断"
        );
    }

    #[test]
    fn hours_one_three_twelve_twenty_four() {
        let now = cst_minute(2026, 11, 5, 23, 30);
        let points = series(cst(2026, 11, 5, 23), 9); // 23:00 … 次日 23:00

        let one = select(&points, now, 1).unwrap();
        assert_eq!(one.points.len(), 1);
        assert_eq!(one.covered_hours, 1);

        let three = select(&points, now, 3).unwrap();
        assert_eq!(three.points.len(), 2, "23:00 + 02:00");
        assert_eq!(three.covered_hours, 3);

        let twelve = select(&points, now, 12).unwrap();
        assert_eq!(twelve.points.len(), 5, "23、02、05、08、11");
        assert_eq!(twelve.covered_hours, 12);
        assert_eq!(twelve.coverage_end, now + ChronoDuration::hours(12));

        let twenty_four = select(&points, now, 24).unwrap();
        assert_eq!(twenty_four.points.len(), 9);
        assert_eq!(twenty_four.covered_hours, 24);
        assert_eq!(twenty_four.coverage_end, now + ChronoDuration::hours(24));
    }

    /// 上游数据只到 05:00 时，24 小时窗口只能如实报告更小的覆盖时长。
    #[test]
    fn covered_hours_never_exceeds_available_data() {
        let now = cst_minute(2026, 11, 5, 23, 30);
        let points = series(cst(2026, 11, 5, 23), 3); // 23:00 / 02:00 / 05:00
        let selection = select(&points, now, 24).unwrap();
        assert_eq!(selection.points.len(), 3);
        // 05:00 的区间到 08:00，从 23:30 起是 8.5 小时 → 向下取整 8。
        assert_eq!(selection.covered_hours, 8);
        assert!(
            selection.covered_hours < 24,
            "不能凭空夸大覆盖时长：请求 24 小时，实际只有 {}",
            selection.covered_hours
        );
    }

    /// 只有最后一个时点落在窗口里：累计覆盖就是那一个区间的长度，不是整个窗口。
    #[test]
    fn single_trailing_bucket_does_not_cover_the_whole_window() {
        let now = cst(2026, 11, 6, 10);
        let points = series(cst(2026, 11, 6, 19), 1);
        let selection = select(&points, now, 12).unwrap();
        assert_eq!(selection.covered_hours, 3);
        assert_eq!(
            selection.contiguous_hours, 0,
            "10:00 到 19:00 之间没有数据，连续覆盖率必须是 0"
        );
        assert_eq!(selection.coverage_end, cst(2026, 11, 6, 22));
    }

    /// 缺口把窗口切成两段：累计 6 小时，连续只有 3 小时。
    #[test]
    fn gap_splits_coverage_into_two_segments() {
        let now = cst(2026, 11, 6, 10);
        let points = vec![
            point_at(cst(2026, 11, 6, 10)),
            point_at(cst(2026, 11, 6, 17)),
        ];
        let selection = select(&points, now, 12).unwrap();
        assert_eq!(selection.covered_hours, 6);
        assert_eq!(selection.contiguous_hours, 3);
    }

    /// 时点恰好等于截止时刻：保留在 points 里（既有输出兼容），但不贡献覆盖。
    #[test]
    fn bucket_starting_at_the_deadline_covers_nothing() {
        let now = cst(2026, 11, 6, 10);
        let points = series(cst(2026, 11, 6, 10), 2); // 10:00 / 13:00
        let deadline_point = series(cst(2026, 11, 6, 13), 1);

        // 请求 3 小时 → 截止 13:00，13:00 那个时点只作为边界保留，不计覆盖。
        let selection = select(&points, now, 3).unwrap();
        assert_eq!(selection.points.len(), 2);
        assert_eq!(selection.covered_hours, 3, "只有 [10:00,13:00) 这 3 小时");
        assert_eq!(selection.coverage_end, cst(2026, 11, 6, 13));

        // 请求 3 小时但上游只给了 13:00 一个时点 → 窗口内没有任何覆盖，必须报错。
        assert!(select(&deadline_point, now, 3).is_err());
    }

    /// 重叠与重复的时点不能重复计数。
    #[test]
    fn overlapping_points_are_merged() {
        let now = cst(2026, 11, 6, 10);
        let points = vec![
            point_at(cst(2026, 11, 6, 11)),
            point_at(cst(2026, 11, 6, 12)),
            point_at(cst(2026, 11, 6, 12)),
            point_at(cst(2026, 11, 6, 14)),
        ];
        let selection = select(&points, now, 12).unwrap();
        assert_eq!(selection.covered_hours, 6, "[11,15) ∪ [14,17) = [11,17)");
        assert_eq!(selection.contiguous_hours, 0);
    }

    #[test]
    fn window_is_anchored_on_now_not_on_publish_time() {
        // 同一批时点：两种「现在」得到的窗口必须不同，与发布时间无关。
        let points = series(cst(2026, 11, 6, 11), 8);
        let early = select(&points, cst_minute(2026, 11, 6, 9, 15), 12).unwrap();
        let late = select(&points, cst(2026, 11, 6, 17), 12).unwrap();

        assert_eq!(
            early.points.first().unwrap().time,
            cst(2026, 11, 6, 11),
            "09:15 的窗口从 11:00 那个时点开始"
        );
        assert_eq!(early.points.len(), 4, "11 / 14 / 17 / 20");

        assert_eq!(
            late.points.first().unwrap().time,
            cst(2026, 11, 6, 17),
            "17:00 的窗口从 17:00 那个时点开始"
        );
        assert_eq!(late.points.len(), 5, "17 / 20 / 23 / 02 / 05");
    }

    #[test]
    fn reports_miss_when_every_bucket_is_in_the_past() {
        let points = series(cst(2026, 11, 5, 8), 4); // 08:00 … 17:00
        let miss = select(&points, cst(2026, 11, 5, 23), 12).expect_err("窗口无数据应报错");
        assert_eq!(miss.available_from, Some(cst(2026, 11, 5, 8)));
        assert_eq!(miss.available_until, Some(cst(2026, 11, 5, 20)));
        let message = miss.message(12);
        assert!(message.contains("没有覆盖未来 12 小时"), "{message}");
        assert!(message.contains("2026-11-05 23:00"), "{message}");
    }

    #[test]
    fn reports_miss_when_data_starts_after_the_window() {
        // 现在 10:00，请求 1 小时，但上游最早的时点是 23:00：窗口没被覆盖。
        let points = series(cst(2026, 11, 5, 23), 4);
        let miss = select(&points, cst(2026, 11, 5, 10), 1).expect_err("窗口无数据应报错");
        assert_eq!(miss.available_from, Some(cst(2026, 11, 5, 23)));
    }

    #[test]
    fn reports_miss_for_empty_input() {
        let miss = select(&[], cst(2026, 11, 5, 10), 12).expect_err("空输入应报错");
        assert_eq!(miss.available_from, None);
        assert_eq!(miss.available_until, None);
        assert!(miss.message(12).contains("没有返回任何可解析的预报时点"));
    }

    #[test]
    fn crosses_month_and_year_boundary() {
        // 2026-12-31 22:30 → 2027-01-01，跨年窗口。
        let now = cst_minute(2026, 12, 31, 22, 30);
        let points = series(cst(2026, 12, 31, 22), 4); // 12-31 22, 01-01 01, 04, 07
        let selection = select(&points, now, 9).unwrap();
        assert_eq!(selection.points.len(), 4);
        assert_eq!(selection.points[1].time, cst(2027, 1, 1, 1));
        assert_eq!(
            selection.coverage_end,
            cst_minute(2027, 1, 1, 7, 30),
            "覆盖截止时刻必须被请求窗口（07:30）夹住"
        );
        assert_eq!(selection.covered_hours, 9);
    }

    #[test]
    fn crosses_leap_day() {
        let points = series(cst(2028, 2, 28, 23), 4);
        let selection = select(&points, cst(2028, 2, 28, 23), 6).unwrap();
        assert_eq!(selection.points[1].time, cst(2028, 2, 29, 2));
        assert_eq!(selection.points[2].time, cst(2028, 2, 29, 5));
    }

    #[test]
    fn compares_instants_across_offsets() {
        // 同一个绝对时刻，用 UTC 偏移表示也必须被正确识别（比较是瞬时对齐的）。
        let utc = chrono::FixedOffset::east_opt(0).expect("UTC");
        let utc_point = HourlyForecast {
            time: utc.with_ymd_and_hms(2026, 11, 6, 0, 0, 0).single().unwrap(), // 北京 08:00
            ..point_at(cst(2026, 11, 6, 8))
        };
        let mut points = vec![utc_point];
        points.extend(series(cst(2026, 11, 6, 11), 2));

        // 现在 = 北京 09:00：UTC 表示的 08:00 区间 [08:00, 11:00) 仍然覆盖未来。
        let selection = select(&points, cst(2026, 11, 6, 9), 6).unwrap();
        assert_eq!(selection.points.len(), 3);
        assert_eq!(selection.points[0].time, cst(2026, 11, 6, 8));
        assert_eq!(
            selection.covered_hours, 6,
            "[08:00,11:00) 与窗口 [09:00,15:00)"
        );
    }

    #[test]
    fn preserves_three_hour_granularity_without_interpolation() {
        let now = cst_minute(2026, 11, 6, 9, 15);
        let points = series(cst(2026, 11, 6, 8), 8);
        let selection = select(&points, now, 24).unwrap();
        for pair in selection.points.windows(2) {
            assert_eq!(
                (pair[1].time - pair[0].time).num_hours(),
                RESOLUTION_HOURS,
                "裁剪不得改变上游粒度"
            );
        }
        // 保留的时点数必须与源数据在窗口内的数量完全一致（没有凭空补点）。
        let expected = points
            .iter()
            .filter(|point| {
                bucket_end(point.time).is_some_and(|end| end > now)
                    && point.time <= now + ChronoDuration::hours(24)
            })
            .count();
        assert_eq!(selection.points.len(), expected);
    }

    #[test]
    fn sorts_and_dedups_input() {
        let now = cst(2026, 11, 6, 9);
        let mut points = series(cst(2026, 11, 6, 11), 3);
        points.reverse();
        points.push(points[0].clone());
        let selection = select(&points, now, 12).unwrap();
        assert_eq!(selection.points.len(), 3);
        assert_eq!(selection.points[0].time, cst(2026, 11, 6, 11));
        assert_eq!(selection.points[2].time, cst(2026, 11, 6, 17));
    }

    #[test]
    fn zero_hours_covers_nothing() {
        let now = cst(2026, 11, 6, 9);
        let points = series(cst(2026, 11, 6, 9), 3);
        let selection = select(&points, now, 0).unwrap();
        assert_eq!(selection.covered_hours, 0);
        assert_eq!(selection.contiguous_hours, 0);
        assert_eq!(selection.coverage_end, now);
        assert!(selection.points.is_empty(), "空窗口不返回任何时点");
    }

    /// 极端参数不得溢出：窗口上限把 `requested_hours` 夹住。
    #[test]
    fn absurd_window_sizes_do_not_overflow() {
        let now = cst(2026, 11, 6, 9);
        let points = series(cst(2026, 11, 6, 11), 3);
        let selection = select(&points, now, i64::MAX).unwrap();
        assert_eq!(
            selection.deadline,
            now + ChronoDuration::hours(MAX_WINDOW_HOURS)
        );
        assert!(selection.covered_hours <= MAX_WINDOW_HOURS as u32);
        assert!(select(&points, now, -5).is_ok(), "负数按 0 处理");
    }
}
