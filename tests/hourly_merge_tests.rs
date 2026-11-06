//! 逐 3 小时预报 `1d` / `23d` / `7d` 三个来源合并的集成回归测试。
//!
//! 这些用例只碰 [`parse_hourly_page`]，全部离线：时间基准来自页面里的
//! `input#fc_3h_internal_update_time`，不读系统时钟、不发网络请求。
//!
//! 每条用例都对应一个**具体不变式**，断言一律打在**具体字段值**上
//! （气温 / 天气 / 风向 / 风力 / 图标），不用「返回了几个时点」来代替。
//!
//! 注意用例的两种性质（以基线 `17a4419` 的实测结果为准）：
//!
//! * **判别性回归测试** —— 旧实现会失败，用来证明缺陷已修；
//! * **语义护栏** —— 旧实现在这一点上恰好也满足，用来防止未来的实现改动把语义
//!   改坏（例如把合并策略换成「逐字段用低优先级候选补 `None`」）。
//!
//! 具体哪个是哪种，写在每个用例的文档注释第一行，不含「旧实现会失败」这类未经
//! 基线验证的说法。

use weather_mcp_cn::models::HourlyForecast;
use weather_mcp_cn::providers::weather_com_cn::{build_cst, parse_hourly_page};

/// 拼一个最小的 1 日页面。
///
/// 发布时间由页面自己给出，解析器据此推断「日号 + 时」的月/年，因此测试完全不依赖
/// 运行时的当前时间。
fn hourly_page(published: &str, hour3data: &str) -> String {
    format!(
        r#"<html><body>
<input type="hidden" id="fc_3h_internal_update_time" value="{published}"/>
<script>var hour3data={hour3data};</script>
</body></html>"#
    )
}

fn times_of(points: &[HourlyForecast]) -> Vec<String> {
    points
        .iter()
        .map(|point| point.time.format("%m-%d %H:%M").to_string())
        .collect()
}

/// 缺陷 1：高优先级（`1d`）记录**全空**、低优先级（`23d`）完整。
///
/// 旧实现先按来源优先级排序再去重，`1d` 的 `"06日11时,,,,,"`（只有时间戳）会把
/// `23d` 的完整记录整条挤掉，最后该时点要么变成空壳、要么整条消失。
/// 新期望：该时点由 `23d` 提供，且每个气象字段都来自那一条记录。
#[test]
fn empty_1d_record_falls_back_to_complete_23d_record() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,,,,,"],"23d":[["06日11时,d00,晴,12℃,西风,<3级,1"]]}"#,
    );
    let payload = parse_hourly_page(&html).expect("空记录 + 完整记录应可解析");
    assert_eq!(payload.points.len(), 1, "同一时刻只能出现一次");

    let point = &payload.points[0];
    assert_eq!(point.time, build_cst(2026, 11, 6, 11, 0).expect("合法时间"));
    assert_eq!(point.temperature_c, Some(12), "气温必须来自 23d 的记录");
    assert_eq!(point.weather.as_deref(), Some("晴"));
    assert_eq!(point.wind_direction.as_deref(), Some("西风"));
    assert_eq!(point.wind_scale.as_deref(), Some("<3级"));
    assert_eq!(point.icon_code.as_deref(), Some("d00"));
}

/// 语义护栏（**不是**判别性回归测试）：禁止跨源拼接字段。
///
/// 基线 `17a4419` 上本条用例实测是 `ok`：旧实现按 `sort_by(time, priority)` +
/// `dedup_by(time)` 归并，一旦选中 `1d` 的那条记录就整条用它，**从不**拿 `23d` 去补
/// `1d` 的 `None` 字段。也就是说，旧实现在这一点上恰好是对的，本条没有暴露任何历史缺陷。
///
/// 那它防止的**未来风险**是什么：断言锁死了「整条记录选择」这个语义。只要有人把归并
/// 改成「逐字段用低优先级候选补 `None`」（一个看起来更贴心、实则伪造观测的改法），
/// `temperature_c` / `wind_direction` / `wind_scale` 就会变成 `Some(99)` /
/// `Some("北风")` / `Some("9级")`，这条用例立刻失败。它是未来改动的护栏。
#[test]
fn partial_1d_record_never_borrows_fields_from_23d() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,d00,晴,,,"],
            "23d":[["06日11时,d00,晴,99℃,北风,9级,9"]]}"#,
    );
    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(payload.points.len(), 1);

    let point = &payload.points[0];
    assert_eq!(
        point.weather.as_deref(),
        Some("晴"),
        "天气取自被选中的 1d 记录"
    );
    assert_eq!(point.temperature_c, None, "不得从 23d 借气温");
    assert_eq!(point.wind_direction, None, "不得从 23d 借风向");
    assert_eq!(point.wind_scale, None, "不得从 23d 借风力");
}

/// 语义护栏（**不是**判别性回归测试）：来源优先级不得被低优先级内容覆盖。
///
/// 基线 `17a4419` 上本条用例实测是 `ok`：旧实现 `sort_by(time, priority)` 之后
/// `dedup_by(time)` 保留每组第一个，时间相同时按优先级升序排列，`1d`（priority 0）
/// 永远排在 `23d`（priority 1）之前 —— 因此 `23d` **从未**覆盖过 `1d`。原注释里
/// 「后写入的 `23d` / `7d` 有可能覆盖 `1d`」与旧代码不符，已按基线实测更正。
///
/// 那它防止的**未来风险**是什么：任何把归并改成「后到的记录胜出」「同刻取最后一个
/// 非空值」或者打乱排序键顺序的实现，都会让 `-30` / `暴雨` / `北风` / `9级` 漏进结果，
/// 下面这几条 `assert_ne!` 会立刻抓到。保留它是为了让这种改写无法悄悄发生。
#[test]
fn conflicting_23d_content_cannot_override_1d() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,d00,晴,12℃,西风,<3级,1"],
            "23d":[["06日11时,n09,暴雨,-30℃,北风,9级,9"]]}"#,
    );
    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(payload.points.len(), 1);

    let point = &payload.points[0];
    assert_eq!(point.temperature_c, Some(12), "`1d` 优先于 `23d`");
    assert_eq!(point.weather.as_deref(), Some("晴"));
    assert_eq!(point.wind_direction.as_deref(), Some("西风"));
    assert_eq!(point.wind_scale.as_deref(), Some("<3级"));
    assert_eq!(point.icon_code.as_deref(), Some("d00"));

    // 冲突来源的值一个都不许出现。
    assert_ne!(point.temperature_c, Some(-30), "不能取 23d 的气温");
    assert_ne!(point.weather.as_deref(), Some("暴雨"));
    assert_ne!(point.wind_direction.as_deref(), Some("北风"));
    assert_ne!(point.wind_scale.as_deref(), Some("9级"));
}

/// 缺陷 4：三个来源同刻都没有气象数据。
///
/// 旧实现会留下一个「只有时间戳、字段全空」的时点，调用方据此以为「有预报」。
/// 新期望：该时点整条丢弃（时间序列里留一个诚实的空洞）；整页只剩这类记录时
/// 必须报 `parse_error`，而不是返回空壳数据。
#[test]
fn points_without_any_measurement_are_dropped_and_all_empty_page_is_parse_error() {
    // 该时点在三个来源里都只有时间戳，另一个时点有真实数据。
    let mixed = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,,,,,","06日14时,d00,晴,12℃,西风,<3级,1"],
            "23d":[["06日11时,,,,,"]],
            "7d":[["06日11时,n01,,,,"]]}"#,
    );
    let payload = parse_hourly_page(&mixed).expect("有效时点仍然存在");
    assert_eq!(
        times_of(&payload.points),
        vec!["11-06 14:00"],
        "空壳时点整条丢弃"
    );
    assert_eq!(payload.points[0].temperature_c, Some(12));
    assert_eq!(payload.points[0].weather.as_deref(), Some("晴"));

    // 整页只剩空记录。
    let all_empty = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,,,,,","06日14时,d00,,,,"],
            "23d":[["06日11时,,,,,"]],
            "7d":[["06日11时,n01,,,,"]]}"#,
    );
    let err = parse_hourly_page(&all_empty).expect_err("没有任何气象信息必须报错");
    assert_eq!(err.code(), "parse_error");
}

/// 缺陷 5：同一字段内部出现重复时间戳。
///
/// 旧实现按 `dedup_by` 直接保留第一个，于是「空记录排在前」时有效记录被丢掉。
/// 新期望：只剩一条，且是**有效**的那一条。
#[test]
fn duplicate_timestamps_inside_one_field_keep_the_valid_copy() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,,,,,","06日11时,d00,晴,12℃,西风,<3级,1"]}"#,
    );
    let payload = parse_hourly_page(&html).expect("应可解析");
    assert_eq!(payload.points.len(), 1, "同一时刻只能出现一次");

    let point = &payload.points[0];
    assert_eq!(point.temperature_c, Some(12));
    assert_eq!(point.weather.as_deref(), Some("晴"));
    assert_eq!(point.wind_direction.as_deref(), Some("西风"));
    assert_eq!(point.wind_scale.as_deref(), Some("<3级"));
}

/// 语义护栏（**不是**判别性回归测试）：上游混入格式错误 / 无效时间 / 未知结构。
///
/// 基线 `17a4419` 上本条用例实测是 `ok`：旧实现在这几点上与新实现行为一致 ——
/// 逐三小时路径的坏日号走 `resolve_hour_date()`，它对 `day=99` / `hour=32` 的
/// 「基准月 / 下月 / 上月」三个候选**全部构造失败**（`from_ymd_opt` / `and_hms_opt`
/// 返回 `None`）→ 直接返回 `None` → 条目被丢弃；非字符串条目（`null` / `true` /
/// 对象）与字段不足的行（`"06日14时,d00"`）同样进不来。本条**没有**暴露任何历史缺陷。
///
/// 更正一处旧注释的错：原文写的「坏日号还会被**向前搜 14 个月**救回来」不属于这条
/// 路径 —— 那是 **7 日预报** `next_date_with_day()` 的旧缺陷机制，逐三小时解析从来没有
/// 涉及它。
///
/// 那它防止的**未来风险**是什么：断言锁住「非法条目必须消失、且不许污染同页有效时点」。
/// 只要有人删掉 `parse_hour_point` 的 `fields.len() < 6` 守卫，或让 `resolve_hour_date`
/// 在三个候选全失败时「兜底」回退成当月同日 / 基准时刻，`"99日11时"`、`"06日32时"`
/// 就会变成凭空造出来的时点，这里立刻失败。
#[test]
fn malformed_and_unknown_entries_are_discarded() {
    let html = hourly_page(
        "2026-11-06 07:30",
        r#"{"1d":["06日11时,d00,晴,12℃,西风,<3级,1","这不是一个时点","06日14时,d00","99日11时,d00,晴,1℃,西风,<3级,1","06日32时,d00,晴,1℃,西风,<3级,1",null,true,{"x":1},{"y":[1]}],
            "23d":"不是数组","7d":[[null,true,"06日17时,d00,晴,9℃,西风,<3级,1"]]}"#,
    );
    let payload = parse_hourly_page(&html).expect("有效时点仍然存在");
    assert_eq!(
        times_of(&payload.points),
        vec!["11-06 11:00", "11-06 17:00"],
        "非法条目被丢弃，有效时点按时间升序保留"
    );

    assert_eq!(payload.points[0].temperature_c, Some(12));
    assert_eq!(payload.points[0].weather.as_deref(), Some("晴"));
    assert_eq!(payload.points[0].wind_direction.as_deref(), Some("西风"));

    let from_7d = &payload.points[1];
    assert_eq!(
        from_7d.time,
        build_cst(2026, 11, 6, 17, 0).expect("合法时间")
    );
    assert_eq!(from_7d.temperature_c, Some(9));
    assert_eq!(from_7d.weather.as_deref(), Some("晴"));
    assert_eq!(from_7d.icon_code.as_deref(), Some("d00"));
}

/// 缺陷 7（回归保护）：真实页面不得因为「丢弃空记录」而掉点。
///
/// 真实样本里本来就没有空记录，因此结果必须仍是 32 个时点、3 小时恒定间隔、
/// 每个时点的气温与天气都不为空 —— 否则说明过滤逻辑误伤了真实数据。
#[test]
fn real_hourly_fixture_still_parses_into_complete_points() {
    const FULL_PAGE: &str = include_str!("fixtures/hourly_1d_101010100_20261106.html");

    let payload = parse_hourly_page(FULL_PAGE).expect("真实页面应可解析");
    assert_eq!(payload.points.len(), 32, "1d + 23d + 7d 合并后是 32 个时点");

    assert_eq!(
        payload.points[0].time.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-06 11:00",
        "首点"
    );
    assert_eq!(
        payload.points[31].time.format("%Y-%m-%d %H:%M").to_string(),
        "2026-11-10 08:00",
        "末点"
    );
    for pair in payload.points.windows(2) {
        assert_eq!(
            (pair[1].time - pair[0].time).num_hours(),
            3,
            "逐三小时数据不允许被插值或补点"
        );
    }
    for point in &payload.points {
        assert!(
            point.temperature_c.is_some(),
            "空记录混进了结果：{}",
            point.time
        );
        assert!(point.weather.is_some(), "空记录混进了结果：{}", point.time);
    }

    // 抽查首点：值必须来自真实记录，且不能被别的来源改写。
    let first = &payload.points[0];
    assert_eq!(first.temperature_c, Some(12));
    assert_eq!(first.weather.as_deref(), Some("晴"));
    assert_eq!(first.icon_code.as_deref(), Some("d00"));
    assert_eq!(first.wind_direction.as_deref(), Some("西风"));
    assert_eq!(first.wind_scale.as_deref(), Some("<3级"));
}
