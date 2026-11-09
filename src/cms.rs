//! 苹果 CMS 资源站解析（对齐 cms-parser.ts）：搜索列表、详情、HTML 降级、
//! 成人过滤、标题归一化与相关性过滤。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use crate::{config::Config, net::FetchOptions, util::{as_array, non_empty, s, s_owned}};

pub fn cms_options(cfg: &Config) -> FetchOptions {
    FetchOptions::default()
        .with_timeout(cfg.request_timeout.as_millis() as u64)
        .header("user-agent", &cfg.user_agent)
        .json_accept()
}

/// 搜索响应 → 统一结果列表
pub fn parse_search_list(data: &Value, source: &Value) -> Result<Vec<Value>, String> {
    if !data.is_object() {
        return Err("API返回的数据格式无效".into());
    }
    let list = data.get("list");
    if let Some(l) = list {
        if !l.is_null() && !l.is_array() {
            return Err("API返回的数据格式无效".into());
        }
    }
    let key = s(source, "key").unwrap_or("");
    let name = s(source, "name").unwrap_or("");
    let url = s(source, "url").unwrap_or("");
    Ok(as_array(data, "list")
        .iter()
        .map(|vod| {
            let mut item = json!({
                "sourceKey": key,
                "sourceName": name,
                "vodId": s_owned(vod, "vod_id"),
                "name": s_owned(vod, "vod_name"),
                "sourceUrl": url,
            });
            let obj = item.as_object_mut().unwrap();
            for (src, dst) in [
                ("vod_pic", "pic"),
                ("type_name", "typeName"),
                ("vod_year", "year"),
                ("vod_area", "area"),
                ("vod_remarks", "remarks"),
            ] {
                if let Some(v) = s(vod, src) {
                    obj.insert(dst.into(), Value::String(v.to_string()));
                }
            }
            if let Some(adult) = source.get("isAdult").and_then(Value::as_bool) {
                obj.insert("isAdult".into(), Value::Bool(adult));
            }
            item
        })
        .collect())
}

/// vod_play_url: `源1$$$源2`，集内 `集名$URL1#集名$URL2`。
/// 优先取含 m3u8 直链的线路，避免第一条是不可播的网页中转页。
pub fn extract_episodes_from_play_url(play_url: &str) -> Vec<String> {
    if play_url.is_empty() {
        return vec![];
    }
    let groups: Vec<Vec<String>> = play_url
        .split("$$$")
        .map(|group| {
            group
                .split('#')
                .filter_map(|ep| {
                    let mut parts = ep.split('$');
                    parts.next();
                    parts.next().map(str::to_string)
                })
                .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
                .collect()
        })
        .filter(|eps: &Vec<String>| !eps.is_empty())
        .collect();
    groups
        .iter()
        .find(|eps| eps.iter().any(|u| u.contains(".m3u8")))
        .or_else(|| groups.first())
        .cloned()
        .unwrap_or_default()
}

fn m3u8_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\$https?://[^"'\s]+?\.m3u8"#).unwrap())
}

pub fn extract_m3u8_from_text(text: &str) -> Vec<String> {
    m3u8_pattern()
        .find_iter(text)
        .map(|m| m.as_str().trim_start_matches('$').to_string())
        .collect()
}

pub fn parse_detail(data: &Value, source: &Value) -> Result<Value, String> {
    let list = as_array(data, "list");
    if list.is_empty() {
        return Err("获取到的详情内容无效".into());
    }
    let vod = &list[0];
    let mut episodes = extract_episodes_from_play_url(&s_owned(vod, "vod_play_url"));
    if episodes.is_empty() {
        episodes = extract_m3u8_from_text(&s_owned(vod, "vod_content"));
    }
    Ok(json!({
        "episodes": episodes,
        "videoInfo": video_info(vod, source),
    }))
}

fn video_info(vod: &Value, source: &Value) -> Value {
    let mut info = serde_json::Map::new();
    for (src, dst) in [
        ("vod_name", "title"),
        ("vod_pic", "cover"),
        ("vod_content", "desc"),
        ("type_name", "typeName"),
        ("vod_year", "year"),
        ("vod_area", "area"),
        ("vod_director", "director"),
        ("vod_actor", "actor"),
        ("vod_remarks", "remarks"),
    ] {
        if let Some(v) = non_empty(vod, src) {
            info.insert(dst.into(), Value::String(v));
        }
    }
    for (src, dst) in [("key", "sourceKey"), ("name", "sourceName"), ("url", "sourceUrl")] {
        if let Some(v) = s(source, src) {
            info.insert(dst.into(), Value::String(v.to_string()));
        }
    }
    Value::Object(info)
}

/// 部分源需要爬详情页 HTML 提取 m3u8
pub fn parse_detail_page_html(html: &str, source: &Value) -> Value {
    let generic = Regex::new(r#"\$(https?://[^"'\s]+?\.m3u8)"#).unwrap();
    let dated = Regex::new(r#"\$(https?://[^"'\s]+?/\d{8}/\d+_[a-f0-9]+/index\.m3u8)"#).unwrap();
    let mut matches: Vec<String> = generic.captures_iter(html).filter_map(|c| c.get(1).map(|m| m.as_str().to_string())).collect();
    if matches.is_empty() {
        matches = dated.captures_iter(html).filter_map(|c| c.get(1).map(|m| m.as_str().to_string())).collect();
    }
    let mut seen = std::collections::HashSet::new();
    let episodes: Vec<String> = matches
        .into_iter()
        .filter(|u| seen.insert(u.clone()))
        .map(|u| match u.find('(') {
            Some(i) if i > 0 => u[..i].to_string(),
            _ => u,
        })
        .collect();

    let title = Regex::new(r#"<h1[^>]*>([^<]+)</h1>"#)
        .unwrap()
        .captures(html)
        .and_then(|c| c.get(1).map(|m| m.as_str().trim().to_string()));
    let desc = Regex::new(r#"(?s)<div[^>]*class=["']sketch["'][^>]*>(.*?)</div>"#)
        .unwrap()
        .captures(html)
        .map(|c| Regex::new(r"<[^>]+>").unwrap().replace_all(&c[1], " ").trim().to_string());

    let mut info = serde_json::Map::new();
    if let Some(t) = title {
        info.insert("title".into(), Value::String(t));
    }
    if let Some(d) = desc {
        info.insert("desc".into(), Value::String(d));
    }
    for (src, dst) in [("key", "sourceKey"), ("name", "sourceName"), ("url", "sourceUrl")] {
        if let Some(v) = s(source, src) {
            info.insert(dst.into(), Value::String(v.to_string()));
        }
    }
    json!({ "episodes": episodes, "videoInfo": Value::Object(info) })
}

const ADULT_KEYWORDS: &[&str] = &[
    "伦理片", "福利", "倫理片", "福利片", "日本伦理", "日本福利", "日本福利片", "日本伦理片",
    "擦边短剧", "擦边", "韩国伦理", "里番动漫", "门事件", "萝莉少女", "制服诱惑", "国产传媒",
    "cosplay", "黑丝诱惑", "无码", "日本无码", "有码", "日本有码", "SWAG", "网红主播",
    "色情片", "同性片", "福利视频", "无码高清", "有码高清", "91视频", "自拍偷拍", "mini传媒",
];

pub fn is_adult_content(type_name: Option<&str>) -> bool {
    type_name.map(|t| ADULT_KEYWORDS.iter().any(|k| t.contains(k))).unwrap_or(false)
}

/// 去空白、标点与符号，转小写（对应 JS `/[\s\p{P}\p{S}]+/gu`）
pub fn normalize_title(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace() && !is_punct_or_symbol(*c))
        .collect()
}

fn is_punct_or_symbol(c: char) -> bool {
    // Unicode 标点/符号主要区间；ASCII 标点单独处理
    if c.is_ascii() {
        return c.is_ascii_punctuation();
    }
    matches!(c as u32,
        0x00A1..=0x00BF | 0x2000..=0x206F | 0x3000..=0x303F | 0xFE10..=0xFE1F
        | 0xFE30..=0xFE4F | 0xFF01..=0xFF0F | 0xFF1A..=0xFF20 | 0xFF3B..=0xFF40
        | 0xFF5B..=0xFF65 | 0x2100..=0x2BFF
    )
}

/// 关键词相关性：归一化后完整命中；多词查询要求每个词都命中。
/// 数字边界：查询以数字开头/结尾时，避免「第2季」误匹配「12」。
pub fn is_relevant(name: &str, wd: &str) -> bool {
    let n_query = normalize_title(wd);
    if n_query.is_empty() {
        return true;
    }
    let n_name = normalize_title(name);
    if n_name.is_empty() {
        return false;
    }
    if includes_keyword(&n_name, &n_query) {
        return true;
    }
    let tokens: Vec<String> = wd.split_whitespace().map(normalize_title).filter(|t| !t.is_empty()).collect();
    tokens.len() > 1 && tokens.iter().all(|t| includes_keyword(&n_name, t))
}

fn includes_keyword(title: &str, keyword: &str) -> bool {
    let chars: Vec<char> = title.chars().collect();
    let kw: Vec<char> = keyword.chars().collect();
    if kw.is_empty() || kw.len() > chars.len() {
        return false;
    }
    let first_is_digit = kw[0].is_ascii_digit();
    let last_is_digit = kw[kw.len() - 1].is_ascii_digit();
    for start in 0..=chars.len() - kw.len() {
        if chars[start..start + kw.len()] != kw[..] {
            continue;
        }
        let before = if start > 0 { chars[start - 1] } else { ' ' };
        let after = if start + kw.len() < chars.len() { chars[start + kw.len()] } else { ' ' };
        if first_is_digit && before.is_ascii_digit() {
            continue;
        }
        if last_is_digit && after.is_ascii_digit() {
            continue;
        }
        return true;
    }
    false
}

/// 聚合：按 sourceKey_vodId 去重 → 成人过滤 → 相关性过滤 → 精确命中优先排序。
/// 注意：原版用 `localeCompare('zh-Hans-CN')` 中文排序，Rust 侧用 Unicode 码点排序，
/// 对纯 ASCII 名一致；中文名顺序可能有细微差异（不影响功能）。
pub fn aggregate(items: Vec<Value>, wd: &str, filter_adult: bool) -> Value {
    let mut seen = std::collections::HashSet::new();
    let exact = normalize_title(wd);
    let mut list: Vec<Value> = items
        .into_iter()
        .filter(|item| {
            let key = format!(
                "{}_{}",
                s(item, "sourceKey").unwrap_or(""),
                s(item, "vodId").unwrap_or("")
            );
            seen.insert(key)
        })
        .filter(|item| {
            !filter_adult || !is_adult_content(item.get("typeName").and_then(Value::as_str))
        })
        .filter(|item| is_relevant(s(item, "name").unwrap_or(""), wd))
        .collect();
    list.sort_by(|a, b| {
        let an = s(a, "name").unwrap_or("");
        let bn = s(b, "name").unwrap_or("");
        let a_exact = normalize_title(an) == exact;
        let b_exact = normalize_title(bn) == exact;
        b_exact.cmp(&a_exact).then_with(|| an.cmp(bn)).then_with(|| {
            s(a, "sourceName").unwrap_or("").cmp(s(b, "sourceName").unwrap_or(""))
        })
    });
    Value::Array(list)
}
