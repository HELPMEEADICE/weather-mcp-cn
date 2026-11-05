//! 城市名称解析测试，全部基于内置的官方城市库（`data/cities.json`）。
//!
//! 城市库由 `weather-mcp-cn --refresh-city-index` 从中国天气网城市选择器数据生成，
//! 因此这里的断言同时也验证了「生成器 + 索引 + 解析算法」这条完整链路。

use weather_mcp_cn::city::{CityIndex, MatchKind, is_city_code};
use weather_mcp_cn::error::WeatherError;

fn index() -> CityIndex {
    CityIndex::from_json(weather_mcp_cn::EMBEDDED_CITIES_JSON).expect("内置城市库必须可解析")
}

#[test]
fn embedded_index_is_usable() {
    let index = index();
    assert!(
        index.len() > 3000,
        "城市库应覆盖全国区县，实际 {} 条",
        index.len()
    );
    assert!(index.generated_at().contains('T'));
    assert!(index.source().contains("weather.com.cn") || index.source().contains("i8tq.com"));
}

#[test]
fn resolves_major_cities_by_name() {
    let index = index();
    let cases = [
        ("北京", "101010100"),
        ("上海", "101020100"),
        ("广州", "101280101"),
        ("深圳", "101280601"),
        ("武汉", "101200101"),
        ("成都", "101270101"),
        ("杭州", "101210101"),
        ("南京", "101190101"),
        ("东莞", "101281601"),
        ("苏州", "101190401"),
    ];
    for (name, expected) in cases {
        let resolved = index.resolve(name).unwrap_or_else(|err| {
            panic!("城市 {name} 解析失败：{err}");
        });
        assert_eq!(resolved.code, expected, "城市 {name} 的代码不对");
    }
}

#[test]
fn resolves_district_with_city_prefix() {
    let index = index();
    let resolved = index
        .resolve("深圳市南山区")
        .expect("应能解析 深圳市南山区");
    assert_eq!(resolved.code, "101280604");
    assert_eq!(resolved.matched_by, MatchKind::Hierarchy);
    assert!(resolved.note.is_none());
    let info = resolved.to_info();
    assert_eq!(info.province.as_deref(), Some("广东省"));
    assert_eq!(info.city.as_deref(), Some("深圳市"));
    assert_eq!(info.district.as_deref(), Some("南山区"));
    assert_eq!(info.display_name, "广东省深圳市南山区");
}

#[test]
fn resolves_city_with_province_prefix() {
    let index = index();
    let resolved = index
        .resolve("湖北省武汉市")
        .expect("应能解析 湖北省武汉市");
    assert_eq!(resolved.code, "101200101");
    assert_eq!(resolved.to_info().display_name, "湖北省武汉市");
}

#[test]
fn resolves_three_level_query() {
    let index = index();
    let resolved = index
        .resolve("广东省广州市天河区")
        .expect("应能解析 广东省广州市天河区");
    assert_eq!(resolved.code, "101280109", "天河区代码");
    assert_eq!(resolved.to_info().display_name, "广东省广州市天河区");
}

#[test]
fn resolves_municipality_district() {
    let index = index();
    assert_eq!(index.resolve("北京市海淀区").unwrap().code, "101010200");
    assert_eq!(index.resolve("海淀").unwrap().code, "101010200");
    assert_eq!(index.resolve("北京市").unwrap().code, "101010100");
}

#[test]
fn accepts_raw_city_code() {
    let index = index();
    assert!(is_city_code("101010100"));
    let resolved = index.resolve("101010100").unwrap();
    assert_eq!(resolved.code, "101010100");
    assert_eq!(resolved.matched_by, MatchKind::Code);
    assert_eq!(resolved.to_info().name, "北京");

    // 合法格式但本地库里没有的代码：仍然放行，由上游校验。
    let unknown = index.resolve("101999999").unwrap();
    assert_eq!(unknown.matched_by, MatchKind::Code);
    assert!(unknown.entry.is_none());
}

#[test]
fn rejects_invalid_city_code_format() {
    let index = index();
    for bad in ["1010101", "1010101000", "1010101a0", "201010100"] {
        assert!(!is_city_code(bad), "{bad} 不应被判为城市代码");
    }
    let err = index.resolve("1010101").unwrap_err();
    assert_eq!(
        err.code(),
        "invalid_city_code",
        "纯数字但长度不对应报格式错误"
    );
    let err = index.resolve("201010100").unwrap_err();
    assert_eq!(err.code(), "invalid_city_code", "非 101 开头应报格式错误");
}

#[test]
fn reports_not_found_for_unknown_city() {
    let index = index();
    let err = index.resolve("不存在的城市名称").unwrap_err();
    assert_eq!(err.code(), "city_not_found");
}

#[test]
fn reports_ambiguity_for_duplicate_names() {
    let index = index();
    let err = index.resolve("鼓楼").unwrap_err();
    assert_eq!(err.code(), "ambiguous_city");
    match err {
        WeatherError::AmbiguousCity {
            candidates, count, ..
        } => {
            assert!(count >= 2);
            let codes: Vec<&str> = candidates.iter().map(|c| c.code.as_str()).collect();
            assert!(codes.contains(&"101190111"), "应包含南京鼓楼：{codes:?}");
            assert!(codes.contains(&"101180808"), "应包含开封鼓楼：{codes:?}");
        }
        other => panic!("期望歧义错误，实际：{other}"),
    }
}

#[test]
fn search_returns_ranked_candidates() {
    let index = index();
    let hits = index.search("南山", 5);
    assert!(!hits.is_empty());
    let codes: Vec<&str> = hits.iter().map(|entry| entry.code.as_str()).collect();
    assert!(codes.contains(&"101280604"), "应包含深圳南山：{codes:?}");

    let hits = index.search("天河", 5);
    assert_eq!(hits[0].code, "101280109");
    assert_eq!(hits[0].display_name(), "广东省广州市天河区");
}

#[test]
fn district_without_station_falls_back_to_parent_city() {
    let index = index();
    // 数据源里没有的街道级地名：如实退回到地级市并给出说明，而不是失败或瞎猜。
    let resolved = index.resolve("深圳市粤海街道").expect("应退回深圳");
    assert_eq!(resolved.code, "101280601");
    let note = resolved.note.unwrap_or_default();
    assert!(note.contains("没有独立站点"), "说明文案：{note}");
}
