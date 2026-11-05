//! 城市名称 → 中国天气网城市代码的解析与检索。
//!
//! 数据来自中国天气网官方城市编码接口（可追溯、可更新）：
//!
//! * 省级：`https://www.weather.com.cn/data/city3jdata/china.html`
//! * 地级：`https://www.weather.com.cn/data/city3jdata/provshi/{省码}.html`
//! * 区县：`https://www.weather.com.cn/data/city3jdata/station/{省市码}.html`
//!
//! 通过 `weather-mcp-cn --refresh-city-index` 可重新抓取并生成 `data/cities.json`；
//! 该文件在编译期用 `include_str!` 打进二进制，所以发布物是单个可执行文件。

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::error::{Result, WeatherError};
use crate::models::{CityInfo, CityLevel};

/// 编译期内置的城市库。
const EMBEDDED_CITIES: &str = include_str!("../data/cities.json");

/// 直辖市的省级名称。
const MUNICIPALITIES: &[&str] = &["北京", "上海", "天津", "重庆"];
/// 自治区的省级名称。
const AUTONOMOUS_REGIONS: &[(&str, &str)] = &[
    ("内蒙古", "内蒙古自治区"),
    ("广西", "广西壮族自治区"),
    ("西藏", "西藏自治区"),
    ("宁夏", "宁夏回族自治区"),
    ("新疆", "新疆维吾尔自治区"),
];
/// 特别行政区。
const SARS: &[(&str, &str)] = &[("香港", "香港特别行政区"), ("澳门", "澳门特别行政区")];

/// 地级行政区中并非「市」的常见简称 → 正确后缀。
///
/// 中国天气网的 `provshi` 接口只给简称（例如「阿坝」代表阿坝藏族羌族自治州），
/// 这里补上正确后缀，避免拼出「阿坝市」这类不存在的名字。
const PREFECTURE_SUFFIX: &[(&str, &str)] = &[
    ("阿坝", "州"),
    ("甘孜", "州"),
    ("凉山", "州"),
    ("黔东南", "州"),
    ("黔南", "州"),
    ("黔西南", "州"),
    ("恩施", "州"),
    ("湘西", "州"),
    ("延边", "州"),
    ("大理", "州"),
    ("德宏", "州"),
    ("迪庆", "州"),
    ("怒江", "州"),
    ("红河", "州"),
    ("文山", "州"),
    ("西双版纳", "州"),
    ("楚雄", "州"),
    ("临夏", "州"),
    ("甘南", "州"),
    ("海西", "州"),
    ("海北", "州"),
    ("黄南", "州"),
    ("果洛", "州"),
    ("玉树", "州"),
    ("伊犁", "州"),
    ("昌吉", "州"),
    ("博尔塔拉", "州"),
    ("巴音郭楞", "州"),
    ("克孜勒苏", "州"),
    ("锡林郭勒", "盟"),
    ("阿拉善", "盟"),
    ("兴安", "盟"),
    ("大兴安岭", "地区"),
    ("阿里", "地区"),
    ("那曲", "市"),
    ("和田", "地区"),
    ("喀什", "地区"),
    ("阿克苏", "地区"),
    ("塔城", "地区"),
    ("阿勒泰", "地区"),
];

/// 已经是完整省级名称的后缀。
const PROVINCE_COMPLETE: &[&str] = &["特别行政区", "自治区", "省", "市"];
/// 已经是完整地级名称的后缀。
const PREFECTURE_COMPLETE: &[&str] = &["特别行政区", "自治州", "地区", "盟", "市"];
/// 已经是完整区县名称的后缀。
const COUNTY_COMPLETE: &[&str] = &["自治县", "区", "县", "市", "旗"];

/// 一条城市记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CityEntry {
    /// 中国天气网 9 位城市代码。
    pub code: String,
    /// 数据源给出的简称。
    pub name: String,
    /// 省级行政区简称。
    pub province: String,
    /// 地级行政区简称；直辖市与省级相同。
    pub city: String,
    /// 区县简称。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    /// 行政区划层级。
    pub level: CityLevel,
}

impl CityEntry {
    /// 省级完整名称，例如 `广东省`。
    pub fn province_display(&self) -> String {
        province_display(&self.province)
    }

    /// 地级完整名称，例如 `广州市`。
    pub fn city_display(&self) -> String {
        match self.level {
            CityLevel::Municipality => self.province_display(),
            _ => prefecture_display(&self.city),
        }
    }

    /// 区县完整名称，例如 `天河区`。
    pub fn district_display(&self) -> Option<String> {
        self.district.as_deref().map(county_display)
    }

    /// 完整可读名称，例如 `广东省广州市天河区`。
    pub fn display_name(&self) -> String {
        display_name(self)
    }
}

/// 省级名称补全后缀。
pub fn province_display(name: &str) -> String {
    let name = name.trim();
    if PROVINCE_COMPLETE
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return name.to_string();
    }
    if MUNICIPALITIES.contains(&name) {
        return format!("{name}市");
    }
    for (short, full) in AUTONOMOUS_REGIONS {
        if *short == name {
            return (*full).to_string();
        }
    }
    for (short, full) in SARS {
        if *short == name {
            return (*full).to_string();
        }
    }
    if name == "台湾" {
        return "台湾省".to_string();
    }
    format!("{name}省")
}

/// 地级名称补全后缀。
///
/// 注意不能把「州」当作完整后缀 —— `广州`、`苏州`、`杭州` 都是地级市，只有
/// 明确出现在 [`PREFECTURE_SUFFIX`] 里的简称才补「州」。
pub fn prefecture_display(name: &str) -> String {
    let name = name.trim();
    if PREFECTURE_COMPLETE
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        return name.to_string();
    }
    for (short, suffix) in PREFECTURE_SUFFIX {
        if *short == name {
            return format!("{name}{suffix}");
        }
    }
    format!("{name}市")
}

/// 区县名称补全后缀。
pub fn county_display(name: &str) -> String {
    let name = name.trim();
    if COUNTY_COMPLETE.iter().any(|suffix| name.ends_with(suffix)) {
        return name.to_string();
    }
    format!("{name}区")
}

fn display_name(entry: &CityEntry) -> String {
    match entry.level {
        CityLevel::Municipality => entry.province_display(),
        CityLevel::City => format!("{}{}", entry.province_display(), entry.city_display()),
        CityLevel::District => format!(
            "{}{}{}",
            entry.province_display(),
            entry.city_display(),
            entry
                .district
                .as_deref()
                .map(county_display)
                .unwrap_or_default()
        ),
    }
}

/// 命中方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// 直接输入的 9 位城市代码。
    Code,
    /// 名称完全一致。
    Exact,
    /// 按「省 → 市 → 区」逐层剥离命中。
    Hierarchy,
    /// 模糊匹配。
    Fuzzy,
}

impl MatchKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Exact => "exact",
            Self::Hierarchy => "hierarchy",
            Self::Fuzzy => "fuzzy",
        }
    }
}

/// 城市解析结果。
#[derive(Debug, Clone)]
pub struct ResolvedCity {
    /// 城市代码。
    pub code: String,
    /// 命中的条目；直接使用代码且代码不在本地库中时为 `None`。
    pub entry: Option<CityEntry>,
    /// 命中方式。
    pub matched_by: MatchKind,
    /// 参考候选（便于调用方展示与二次确认）。
    pub candidates: Vec<CityEntry>,
    /// 解析说明（例如退回到上级行政区的原因）。
    pub note: Option<String>,
}

impl ResolvedCity {
    /// 转换为对外输出的 [`CityInfo`]。
    pub fn to_info(&self) -> CityInfo {
        let mut info = match &self.entry {
            Some(entry) => entry_to_info(entry),
            None => CityInfo {
                code: self.code.clone(),
                name: self.code.clone(),
                province: None,
                city: None,
                district: None,
                display_name: self.code.clone(),
                level: None,
                resolution_note: None,
            },
        };
        info.resolution_note = self.note.clone();
        info
    }
}

/// 把条目转换成对外输出。
pub fn entry_to_info(entry: &CityEntry) -> CityInfo {
    CityInfo {
        code: entry.code.clone(),
        name: entry.name.clone(),
        province: Some(entry.province_display()),
        city: Some(entry.city_display()),
        district: entry.district_display(),
        display_name: entry.display_name(),
        level: Some(entry.level),
        resolution_note: None,
    }
}

/// 城市库文件结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CitiesFile {
    /// 生成时间。
    pub generated_at: String,
    /// 数据来源说明。
    pub source: String,
    /// 条目总数。
    pub total: usize,
    /// 条目列表。
    pub entries: Vec<CityEntry>,
}

/// 内存中的城市索引。
pub struct CityIndex {
    entries: Vec<CityEntry>,
    by_code: HashMap<String, usize>,
    generated_at: String,
    source: String,
}

impl CityIndex {
    /// 使用编译期内置的城市库。
    pub fn embedded() -> &'static CityIndex {
        static INDEX: OnceLock<CityIndex> = OnceLock::new();
        INDEX.get_or_init(|| {
            CityIndex::from_json(EMBEDDED_CITIES).unwrap_or_else(|err| {
                tracing::error!(error = %err, "内置城市库解析失败，使用空索引");
                CityIndex::empty()
            })
        })
    }

    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            by_code: HashMap::new(),
            generated_at: "unknown".to_string(),
            source: "unknown".to_string(),
        }
    }

    /// 从 JSON 文本构建索引。
    pub fn from_json(json: &str) -> Result<Self> {
        let file: CitiesFile = serde_json::from_str(json)
            .map_err(|err| WeatherError::Parse(format!("城市库 JSON 解析失败：{err}")))?;
        Ok(Self::from_entries(
            file.entries,
            file.generated_at,
            file.source,
        ))
    }

    /// 从文件构建索引。
    pub fn from_path(path: &Path) -> Result<Self> {
        let json = std::fs::read_to_string(path).map_err(|err| {
            WeatherError::Config(format!("读取城市库 {} 失败：{err}", path.display()))
        })?;
        Self::from_json(&json)
    }

    /// 从条目构建索引。
    pub fn from_entries(entries: Vec<CityEntry>, generated_at: String, source: String) -> Self {
        let mut by_code = HashMap::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            by_code.insert(entry.code.clone(), index);
        }
        Self {
            entries,
            by_code,
            generated_at,
            source,
        }
    }

    /// 城市库生成时间。
    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    /// 城市库来源。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// 条目数量。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全部条目。
    pub fn entries(&self) -> &[CityEntry] {
        &self.entries
    }

    /// 按代码精确查找。
    pub fn get(&self, code: &str) -> Option<&CityEntry> {
        self.by_code.get(code).map(|index| &self.entries[*index])
    }

    /// 解析城市名称或代码。
    pub fn resolve(&self, raw: &str) -> Result<ResolvedCity> {
        let query = normalize(raw);
        if query.is_empty() {
            return Err(WeatherError::CityNotFound(raw.to_string()));
        }

        if is_city_code(&query) {
            return Ok(ResolvedCity {
                code: query.clone(),
                entry: self.get(&query).cloned(),
                matched_by: MatchKind::Code,
                candidates: Vec::new(),
                note: None,
            });
        }

        // 纯数字但不是合法城市代码：直接指出格式问题，比"找不到城市"更有用。
        if query.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(WeatherError::InvalidCityCode(query));
        }

        if let Some(resolved) = self.resolve_by_hierarchy(&query)? {
            return Ok(resolved);
        }

        let scored = self.search_scored(&query, 10);
        if scored.is_empty() {
            return Err(WeatherError::CityNotFound(raw.to_string()));
        }

        let candidates: Vec<CityEntry> = scored.iter().map(|(entry, _)| entry.clone()).collect();
        let top_score = scored[0].1;
        let tie = scored.len() > 1 && scored[1].1 == top_score;
        if tie || top_score < 60 {
            return Err(WeatherError::AmbiguousCity {
                query: raw.to_string(),
                count: candidates.len(),
                candidates: candidates
                    .into_iter()
                    .take(8)
                    .map(|entry| entry_to_info(&entry))
                    .collect(),
            });
        }

        let (entry, score) = scored.into_iter().next().expect("非空");
        let matched_by = if score >= 94 {
            MatchKind::Exact
        } else {
            MatchKind::Fuzzy
        };
        Ok(ResolvedCity {
            code: entry.code.clone(),
            entry: Some(entry.clone()),
            matched_by,
            candidates: vec![entry],
            note: None,
        })
    }

    /// 模糊搜索城市。
    pub fn search(&self, raw: &str, limit: usize) -> Vec<CityEntry> {
        let query = normalize(raw);
        if query.is_empty() {
            return Vec::new();
        }
        self.search_scored(&query, limit)
            .into_iter()
            .map(|(entry, _)| entry)
            .collect()
    }

    fn search_scored(&self, query: &str, limit: usize) -> Vec<(CityEntry, i32)> {
        let mut scored: Vec<(CityEntry, i32)> = self
            .entries
            .iter()
            .filter_map(|entry| score_entry(entry, query).map(|score| (entry.clone(), score)))
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.code.cmp(&b.0.code)));
        scored.truncate(limit.max(1));
        scored
    }

    /// 按「省 → 市 → 区」逐层剥离。
    fn resolve_by_hierarchy(&self, query: &str) -> Result<Option<ResolvedCity>> {
        let (province, rest_after_province) = self.strip_province(query);

        if let Some(province) = province.as_deref() {
            // 1. 只给了省名：直辖市/特区可唯一定位，其余省名返回候选。
            if rest_after_province.is_empty() {
                let entries: Vec<CityEntry> = self
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.province == province && entry.level != CityLevel::District
                    })
                    .cloned()
                    .collect();
                if entries.len() == 1 {
                    return Ok(Some(Self::hierarchy_result(
                        entries.into_iter().next().expect("长度为一"),
                        None,
                    )));
                }
                return Err(WeatherError::AmbiguousCity {
                    query: query.to_string(),
                    count: entries.len(),
                    candidates: entries
                        .into_iter()
                        .take(10)
                        .map(|entry| entry_to_info(&entry))
                        .collect(),
                });
            }

            // 2. 直辖市 / 特别行政区：省级即市级，剩余部分直接当区县处理。
            if let Some(municipality) = self
                .entries
                .iter()
                .find(|entry| entry.province == province && entry.level == CityLevel::Municipality)
                .cloned()
            {
                let (district_hit, rest_after_district) =
                    self.strip_district(rest_after_province, &municipality);
                if let Some((district_entry, ties)) = district_hit
                    && ties.is_empty()
                    && rest_after_district.is_empty()
                {
                    return Ok(Some(Self::hierarchy_result(district_entry, None)));
                }
                let note = format!(
                    "「{rest_after_province}」在中国天气网城市库中没有独立站点，已使用所属行政区「{}」的数据",
                    municipality.city_display()
                );
                return Ok(Some(Self::hierarchy_result(municipality, Some(note))));
            }
        }

        // 3. 匹配地级行政区（无省前缀时在全国范围内匹配）。
        let (city_hit, rest_after_city) = self.strip_city(rest_after_province, province.as_deref());
        let Some((city_entry, city_ties)) = city_hit else {
            return Ok(None);
        };
        if !city_ties.is_empty() {
            let mut candidates = vec![city_entry.clone()];
            candidates.extend(city_ties);
            return Err(WeatherError::AmbiguousCity {
                query: query.to_string(),
                count: candidates.len(),
                candidates: candidates.iter().map(entry_to_info).collect(),
            });
        }

        if rest_after_city.is_empty() {
            return Ok(Some(Self::hierarchy_result(city_entry, None)));
        }

        let (district_hit, rest_after_district) = self.strip_district(rest_after_city, &city_entry);
        if let Some((district_entry, ties)) = district_hit
            && ties.is_empty()
            && rest_after_district.is_empty()
        {
            return Ok(Some(Self::hierarchy_result(district_entry, None)));
        }

        // 区县在中国天气网城市库里没有独立站点：如实退回到所属地级市并说明原因。
        let note = format!(
            "「{rest_after_city}」在中国天气网城市库中没有独立站点，已使用所属行政区「{}」的数据",
            city_entry.city_display()
        );
        Ok(Some(Self::hierarchy_result(city_entry, Some(note))))
    }

    fn hierarchy_result(entry: CityEntry, note: Option<String>) -> ResolvedCity {
        ResolvedCity {
            code: entry.code.clone(),
            entry: Some(entry.clone()),
            matched_by: MatchKind::Hierarchy,
            candidates: vec![entry],
            note,
        }
    }

    fn strip_province<'a>(&self, query: &'a str) -> (Option<String>, &'a str) {
        let mut best: Option<(usize, String)> = None;
        for entry in &self.entries {
            for form in [entry.province.clone(), entry.province_display()] {
                if form.is_empty() || !query.starts_with(&form) {
                    continue;
                }
                let len = form.len();
                let better = match &best {
                    Some((best_len, _)) => len > *best_len,
                    None => true,
                };
                if better {
                    best = Some((len, entry.province.clone()));
                }
            }
        }
        match best {
            Some((len, province)) => (Some(province), &query[len..]),
            None => (None, query),
        }
    }

    fn strip_city<'a>(
        &self,
        query: &'a str,
        province: Option<&str>,
    ) -> (Option<(CityEntry, Vec<CityEntry>)>, &'a str) {
        let mut matches: Vec<(usize, CityEntry)> = Vec::new();
        for entry in &self.entries {
            if entry.level == CityLevel::District {
                continue;
            }
            if let Some(province) = province
                && entry.province != province
            {
                continue;
            }
            let display = entry.city_display();
            let mut best_len: Option<usize> = None;
            for form in [entry.name.as_str(), display.as_str()] {
                if !form.is_empty() && query.starts_with(form) {
                    best_len = Some(best_len.map_or(form.len(), |len: usize| len.max(form.len())));
                }
            }
            if let Some(len) = best_len {
                matches.push((len, entry.clone()));
            }
        }
        if matches.is_empty() {
            return (None, query);
        }
        let max_len = matches.iter().map(|(len, _)| *len).max().unwrap_or(0);
        matches.retain(|(len, _)| *len == max_len);
        let rest = &query[max_len..];
        let mut iter = matches.into_iter();
        let first = iter.next().expect("非空").1;
        let ties: Vec<CityEntry> = iter
            .map(|(_, entry)| entry)
            .filter(|entry| entry.code != first.code)
            .collect();
        let ties = if ties.is_empty() {
            Vec::new()
        } else {
            ties.into_iter()
                .filter(|entry| entry.level != CityLevel::Municipality)
                .collect()
        };
        (Some((first, ties)), rest)
    }

    fn strip_district<'a>(
        &self,
        query: &'a str,
        city: &CityEntry,
    ) -> (Option<(CityEntry, Vec<CityEntry>)>, &'a str) {
        if query.is_empty() {
            return (None, query);
        }
        let mut matches: Vec<(usize, CityEntry)> = Vec::new();
        for entry in &self.entries {
            if entry.level != CityLevel::District {
                continue;
            }
            if entry.province != city.province || entry.city != city.city {
                continue;
            }
            if let Some(district) = entry.district.as_deref() {
                let display = county_display(district);
                let mut best_len: Option<usize> = None;
                for form in [district, display.as_str()] {
                    if !form.is_empty() && query.starts_with(form) {
                        best_len =
                            Some(best_len.map_or(form.len(), |len: usize| len.max(form.len())));
                    }
                }
                if let Some(len) = best_len {
                    matches.push((len, entry.clone()));
                }
            }
        }
        if matches.is_empty() {
            return (None, query);
        }
        let max_len = matches.iter().map(|(len, _)| *len).max().unwrap_or(0);
        matches.retain(|(len, _)| *len == max_len);
        let rest = &query[max_len..];
        let mut iter = matches.into_iter();
        let first = iter.next().expect("非空").1;
        let ties: Vec<CityEntry> = iter.map(|(_, entry)| entry).collect();
        (Some((first, ties)), rest)
    }
}

/// 是否为合法形态的城市代码（101 开头的 9 位数字）。
pub fn is_city_code(value: &str) -> bool {
    value.len() == 9 && value.starts_with("101") && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// 归一化输入：去掉所有空白（含全角空格），并把全角数字/字母转半角。
pub fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            ' ' | '\t' | '\r' | '\n' | '\u{3000}' => continue,
            '\u{ff10}'..='\u{ff19}' => {
                let digit = (ch as u32 - 0xff10) as u8;
                out.push((b'0' + digit) as char);
            }
            '\u{ff21}'..='\u{ff3a}' => {
                let idx = (ch as u32 - 0xff21) as u8;
                out.push((b'A' + idx) as char);
            }
            '\u{ff41}'..='\u{ff5a}' => {
                let idx = (ch as u32 - 0xff41) as u8;
                out.push((b'a' + idx) as char);
            }
            _ => out.push(ch),
        }
    }
    out
}

fn score_entry(entry: &CityEntry, query: &str) -> Option<i32> {
    let name = entry.name.as_str();
    let full = entry.display_name();
    let compact = compact_name(entry);

    let mut best = if name == query {
        100
    } else if full == query {
        96
    } else if compact == query {
        94
    } else if name.starts_with(query) {
        80
    } else if full.starts_with(query) {
        78
    } else if compact.starts_with(query) {
        76
    } else if name.contains(query) {
        70
    } else if compact.contains(query) {
        66
    } else if is_subsequence(query, &compact) {
        45
    } else {
        return None;
    };

    best += match entry.level {
        CityLevel::Municipality => 3,
        CityLevel::City => 2,
        CityLevel::District => 1,
    };
    if query.ends_with('区') || query.ends_with('县') || query.ends_with('旗') {
        if entry.level == CityLevel::District {
            best += 12;
        }
    } else if query.ends_with('市') && entry.level != CityLevel::District {
        best += 8;
    }
    Some(best)
}

fn compact_name(entry: &CityEntry) -> String {
    let mut out = entry.province.clone();
    if entry.city != entry.province {
        out.push_str(&entry.city);
    }
    if let Some(district) = entry.district.as_deref()
        && district != entry.city
    {
        out.push_str(district);
    }
    out
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    for target in needle.chars() {
        loop {
            match chars.next() {
                Some(ch) if ch == target => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> CityIndex {
        let entries = vec![
            CityEntry {
                code: "101010100".into(),
                name: "北京".into(),
                province: "北京".into(),
                city: "北京".into(),
                district: None,
                level: CityLevel::Municipality,
            },
            CityEntry {
                code: "101010200".into(),
                name: "海淀".into(),
                province: "北京".into(),
                city: "北京".into(),
                district: Some("海淀".into()),
                level: CityLevel::District,
            },
            CityEntry {
                code: "101280101".into(),
                name: "广州".into(),
                province: "广东".into(),
                city: "广州".into(),
                district: None,
                level: CityLevel::City,
            },
            CityEntry {
                code: "101280106".into(),
                name: "天河".into(),
                province: "广东".into(),
                city: "广州".into(),
                district: Some("天河".into()),
                level: CityLevel::District,
            },
            CityEntry {
                code: "101280601".into(),
                name: "深圳".into(),
                province: "广东".into(),
                city: "深圳".into(),
                district: None,
                level: CityLevel::City,
            },
            CityEntry {
                code: "101280604".into(),
                name: "南山".into(),
                province: "广东".into(),
                city: "深圳".into(),
                district: Some("南山".into()),
                level: CityLevel::District,
            },
            CityEntry {
                code: "101200101".into(),
                name: "武汉".into(),
                province: "湖北".into(),
                city: "武汉".into(),
                district: None,
                level: CityLevel::City,
            },
        ];
        CityIndex::from_entries(entries, "test".into(), "test".into())
    }

    #[test]
    fn normalizes_whitespace_and_fullwidth() {
        assert_eq!(normalize(" 北 京\u{3000}"), "北京");
        assert_eq!(normalize("１０１０１０１００"), "101010100");
    }

    #[test]
    fn recognizes_city_codes() {
        assert!(is_city_code("101010100"));
        assert!(!is_city_code("10101010"));
        assert!(!is_city_code("201010100"));
        assert!(!is_city_code("10101010a"));
    }

    #[test]
    fn resolves_municipality() {
        let index = sample_index();
        let resolved = index.resolve("北京").unwrap();
        assert_eq!(resolved.code, "101010100");
    }

    #[test]
    fn resolves_district_by_hierarchy() {
        let index = sample_index();
        let resolved = index.resolve("深圳市南山区").unwrap();
        assert_eq!(resolved.code, "101280604");
        assert_eq!(resolved.matched_by, MatchKind::Hierarchy);
    }

    #[test]
    fn resolves_province_prefixed_city() {
        let index = sample_index();
        let resolved = index.resolve("湖北省武汉市").unwrap();
        assert_eq!(resolved.code, "101200101");
    }

    #[test]
    fn resolves_three_level_query() {
        let index = sample_index();
        let resolved = index.resolve("广东省广州市天河区").unwrap();
        assert_eq!(resolved.code, "101280106");
    }

    #[test]
    fn resolves_plain_city_name() {
        let index = sample_index();
        assert_eq!(index.resolve("广州").unwrap().code, "101280101");
        assert_eq!(index.resolve("海淀").unwrap().code, "101010200");
    }

    #[test]
    fn resolves_raw_code() {
        let index = sample_index();
        let resolved = index.resolve("101010100").unwrap();
        assert_eq!(resolved.code, "101010100");
        assert_eq!(resolved.matched_by, MatchKind::Code);
    }

    #[test]
    fn unknown_city_reports_not_found() {
        let index = sample_index();
        let err = index.resolve("不存在的城市名").unwrap_err();
        assert!(matches!(err, WeatherError::CityNotFound(_)));
    }

    #[test]
    fn province_display_matches_official_names() {
        assert_eq!(province_display("广东"), "广东省");
        assert_eq!(province_display("北京"), "北京市");
        assert_eq!(province_display("内蒙古"), "内蒙古自治区");
        assert_eq!(province_display("香港"), "香港特别行政区");
        assert_eq!(prefecture_display("阿坝"), "阿坝州");
        assert_eq!(prefecture_display("大兴安岭"), "大兴安岭地区");
        assert_eq!(prefecture_display("兴安盟"), "兴安盟");
        assert_eq!(prefecture_display("广州"), "广州市");
        assert_eq!(prefecture_display("苏州"), "苏州市");
        assert_eq!(prefecture_display("乌鲁木齐"), "乌鲁木齐市");
        assert_eq!(county_display("福田"), "福田区");
        assert_eq!(county_display("阳朔县"), "阳朔县");
        assert_eq!(county_display("土左旗"), "土左旗");
    }
}
