//! 城市库生成器。
//!
//! 主数据源是中国天气网自己的城市选择器数据：
//! `https://j.i8tq.com/weather2020/search/city.js`，结构为
//! `{ 省: { 市: { 区: { AREAID, NAMECN } } } }`，一次请求即可拿到全国
//! 34 个省级行政区、地级市与区县的官方 AREAID，比逐层爬 `city3jdata` 更完整
//! （例如深圳的南山、福田等在 `city3jdata/station` 里缺失，但这里齐全）。
//!
//! 若该文件不可用，回退到官方层级接口：
//! `city3jdata/china.html` → `provshi/{省}.html` → `station/{省市}.html`。
//!
//! 这是 `weather-mcp-cn --refresh-city-index` 的实现，也是 `data/cities.json`
//! 的唯一来源——项目里没有任何手写城市表。

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use tracing::{info, warn};

use crate::city::{CitiesFile, CityEntry};
use crate::error::{Result, WeatherError};
use crate::http::HttpClient;
use crate::models::{CityLevel, cst_offset};

/// 中国天气网城市选择器数据。
pub const CITY_JS: &str = "https://j.i8tq.com/weather2020/search/city.js";
/// 城市编码层级接口根地址（回退源）。
pub const CITY_API: &str = "https://www.weather.com.cn/data/city3jdata";
/// Referer，某些静态资源域名会校验。
const REFERER: &str = "https://www.weather.com.cn/";

/// 抓取全量城市库（主源失败时自动回退）。
pub async fn build_city_index(http: &HttpClient) -> Result<CitiesFile> {
    match build_from_city_js(http).await {
        Ok(file) => Ok(file),
        Err(err) => {
            warn!(error = %err, "城市选择器数据不可用，回退到 city3jdata 层级接口");
            build_from_city3jdata(http).await
        }
    }
}

/// 抓取全量城市库并写入 JSON 文件。
pub async fn write_city_index(http: &HttpClient, path: &Path) -> Result<CitiesFile> {
    let file = build_city_index(http).await?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|err| {
            WeatherError::Config(format!("创建目录 {} 失败：{err}", parent.display()))
        })?;
    }
    let json = serde_json::to_string_pretty(&file)
        .map_err(|err| WeatherError::Parse(format!("序列化城市库失败：{err}")))?;
    std::fs::write(path, json)
        .map_err(|err| WeatherError::Config(format!("写入 {} 失败：{err}", path.display())))?;
    info!(path = %path.display(), total = file.total, "城市库已写入");
    Ok(file)
}

// ---------------------------------------------------------------------------
// 主数据源：城市选择器 city.js
// ---------------------------------------------------------------------------

async fn build_from_city_js(http: &HttpClient) -> Result<CitiesFile> {
    let body = http.get_text(CITY_JS, Some(REFERER)).await?;
    let start = body
        .find('{')
        .ok_or_else(|| WeatherError::Parse("城市选择器数据不是预期的 JSON 结构".into()))?;
    let json = body[start..].trim().trim_end_matches(';').trim();
    let value: Value = serde_json::from_str(json)
        .map_err(|err| WeatherError::Parse(format!("城市选择器数据 JSON 解析失败：{err}")))?;
    let provinces = value
        .as_object()
        .ok_or_else(|| WeatherError::Parse("城市选择器数据顶层不是对象".into()))?;

    let mut entries: BTreeMap<String, CityEntry> = BTreeMap::new();
    for (province_name, cities) in provinces {
        let Some(cities) = cities.as_object() else {
            continue;
        };
        for (city_name, districts) in cities {
            let Some(districts) = districts.as_object() else {
                continue;
            };
            let mut list: Vec<(String, String)> = districts
                .iter()
                .filter_map(|(key, node)| {
                    let code = node.get("AREAID")?.as_str()?.trim().to_string();
                    if code.is_empty() {
                        return None;
                    }
                    let name = node
                        .get("NAMECN")
                        .and_then(Value::as_str)
                        .unwrap_or(key)
                        .trim()
                        .to_string();
                    Some((code, name))
                })
                .collect();
            list.sort_by(|a, b| a.0.cmp(&b.0));
            if list.is_empty() {
                continue;
            }

            let is_municipality = province_name == city_name;
            let seat = pick_seat(&list, city_name, is_municipality);

            for (code, name) in &list {
                let level = if *code == seat {
                    if is_municipality {
                        CityLevel::Municipality
                    } else {
                        CityLevel::City
                    }
                } else {
                    CityLevel::District
                };
                insert_entry(
                    &mut entries,
                    CityEntry {
                        code: code.clone(),
                        name: if level == CityLevel::Municipality {
                            province_name.clone()
                        } else {
                            name.clone()
                        },
                        province: province_name.clone(),
                        city: city_name.clone(),
                        district: if level == CityLevel::District {
                            Some(name.clone())
                        } else {
                            None
                        },
                        level,
                    },
                );
            }
        }
    }

    finish(entries, CITY_JS)
}

/// 选出代表「本级行政区」的站点代码。
///
/// 优先与行政区同名的站点（例如 广州 → 广州），其次是结尾为 `01` 的站点
/// （例如 乌兰察布 → 集宁 101080401），最后退化为最小代码。
fn pick_seat(list: &[(String, String)], city_name: &str, is_municipality: bool) -> String {
    if is_municipality && let Some((code, _)) = list.iter().find(|(_, name)| name == city_name) {
        return code.clone();
    }
    if let Some((code, _)) = list.iter().find(|(code, _)| code.ends_with("01")) {
        return code.clone();
    }
    if let Some((code, _)) = list.iter().find(|(_, name)| name == city_name) {
        return code.clone();
    }
    list[0].0.clone()
}

// ---------------------------------------------------------------------------
// 回退数据源：city3jdata 层级接口
// ---------------------------------------------------------------------------

async fn build_from_city3jdata(http: &HttpClient) -> Result<CitiesFile> {
    let provinces = fetch_map(http, &format!("{CITY_API}/china.html")).await?;
    info!(count = provinces.len(), "已获取省级行政区列表");

    let mut entries: BTreeMap<String, CityEntry> = BTreeMap::new();
    let mut requests = 1usize;

    for (province_code, province_name) in &provinces {
        let cities = fetch_map(http, &format!("{CITY_API}/provshi/{province_code}.html")).await?;
        requests += 1;

        for (city_key, city_name) in cities {
            let station_prefix = format!("{province_code}{city_key}");
            let stations =
                fetch_map(http, &format!("{CITY_API}/station/{station_prefix}.html")).await?;
            requests += 1;
            if stations.is_empty() {
                continue;
            }

            let is_municipality = cities_len_hint(province_code, &city_name, &city_key);
            let build_code = |station_key: &str| -> String {
                if station_key.len() == 9 && station_key.starts_with("101") {
                    station_key.to_string()
                } else if is_municipality {
                    format!("{province_code}{station_key}00")
                } else {
                    format!("{station_prefix}{station_key}")
                }
            };

            let seat_key = if stations.iter().any(|(key, _)| key == "01") {
                "01".to_string()
            } else if stations.iter().any(|(key, _)| key.len() == 9) {
                stations[0].0.clone()
            } else {
                warn!(
                    province = province_name.as_str(),
                    city = city_name.as_str(),
                    "该地级市没有可识别的本级站点，跳过本级条目"
                );
                String::new()
            };

            if !seat_key.is_empty() {
                let seat_code = build_code(&seat_key);
                insert_entry(
                    &mut entries,
                    CityEntry {
                        code: seat_code.clone(),
                        name: city_name.clone(),
                        province: province_name.clone(),
                        city: city_name.clone(),
                        district: None,
                        level: if is_municipality {
                            CityLevel::Municipality
                        } else {
                            CityLevel::City
                        },
                    },
                );
            }

            for (station_key, station_name) in stations {
                if station_key == seat_key {
                    continue;
                }
                insert_entry(
                    &mut entries,
                    CityEntry {
                        code: build_code(&station_key),
                        name: station_name.clone(),
                        province: province_name.clone(),
                        city: city_name.clone(),
                        district: Some(station_name),
                        level: CityLevel::District,
                    },
                );
            }
        }
    }

    info!(requests, "city3jdata 抓取完成");
    finish(entries, CITY_API)
}

/// `city3jdata` 里直辖市/特别行政区的省与市同名，且 `provshi` 只有一个 `00` 键。
fn cities_len_hint(province_code: &str, city_name: &str, city_key: &str) -> bool {
    let _ = province_code;
    city_key == "00"
        || matches!(
            city_name,
            "北京" | "上海" | "天津" | "重庆" | "香港" | "澳门"
        )
}

fn finish(entries: BTreeMap<String, CityEntry>, source: &str) -> Result<CitiesFile> {
    let entries: Vec<CityEntry> = entries.into_values().collect();
    info!(total = entries.len(), source, "城市库抓取完成");
    if entries.is_empty() {
        return Err(WeatherError::Upstream(
            "城市库抓取结果为空，请检查网络或接口是否已变更".into(),
        ));
    }
    Ok(CitiesFile {
        generated_at: chrono::Utc::now().with_timezone(&cst_offset()).to_rfc3339(),
        source: source.to_string(),
        total: entries.len(),
        entries,
    })
}

fn insert_entry(entries: &mut BTreeMap<String, CityEntry>, entry: CityEntry) {
    match entries.get(&entry.code) {
        Some(existing) if rank(existing.level) <= rank(entry.level) => {}
        _ => {
            entries.insert(entry.code.clone(), entry);
        }
    }
}

fn rank(level: CityLevel) -> u8 {
    match level {
        CityLevel::Municipality => 0,
        CityLevel::City => 1,
        CityLevel::District => 2,
    }
}

async fn fetch_map(http: &HttpClient, url: &str) -> Result<Vec<(String, String)>> {
    let body = http.get_text(url, Some(REFERER)).await?;
    let value: Value = serde_json::from_str(body.trim())
        .map_err(|err| WeatherError::Parse(format!("城市编码接口 {url} 返回的不是 JSON：{err}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| WeatherError::Parse(format!("城市编码接口 {url} 返回结构异常")))?;
    let mut pairs: Vec<(String, String)> = object
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|text| (key.clone(), text.trim().to_string()))
        })
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(pairs)
}
