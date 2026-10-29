use std::path::Path;

use crate::config::Config;
use crate::dedup::normalize::normalize_key;
use crate::map::NewsItem;
use crate::unit::UnitKey;

/// `news.json` の検査結果。
#[derive(Debug, serde::Serialize)]
pub struct VerifyReport {
    pub unit: String,
    pub ok: bool,
    pub count: usize,
    pub null_date_ratio: f32,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub enum VerifyError {
    /// `news.json` が無い / パースできない。
    Unreadable(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(m) => write!(f, "unreadable: {m}"),
        }
    }
}

impl std::error::Error for VerifyError {}

const MIN_NULL_DATE_HEADROOM: usize = 10;

/// 保存済みの成果物を検査する。**ネットワークも LLM も触らない。**
///
/// 検査項目:
/// 1. `news.json` が存在しパースできる
/// 2. `schema_version == 1`、`count == items.len()`
/// 3. `items[].date` が対象年月に収まる（`YYYY-MM` 前置一致）
/// 4. `date: null` の比率 < 10%
/// 5. タイトルの完全重複が無い（`normalize_key` で判定）
/// 6. `docs/*.json` に `finish_reasons` の `"length"` が無い
pub fn verify_unit(cfg: &Config, u: &UnitKey) -> Result<VerifyReport, VerifyError> {
    let dir = crate::store::unit_dir(cfg, u);
    let news_path = dir.join("news.json");

    let raw = std::fs::read_to_string(&news_path)
        .map_err(|e| VerifyError::Unreadable(format!("{}: {e}", news_path.display())))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| VerifyError::Unreadable(format!("{}: {e}", news_path.display())))?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // --- 2. schema_version / count ---
    match value.get("schema_version").and_then(|v| v.as_u64()) {
        Some(1) => {}
        Some(other) => errors.push(format!("schema_version is {other}, expected 1")),
        None => errors.push("schema_version is missing".to_owned()),
    }

    let items: Vec<NewsItem> = value
        .get("items")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    if value.get("items").is_none() {
        errors.push("items is missing".to_owned());
    }

    let declared = value.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    if declared != items.len() {
        errors.push(format!(
            "count {declared} != items.len() {}",
            items.len()
        ));
    }

    // --- 3. date が対象年月に収まる ---
    let prefix = format!("{:04}-{:02}", u.year, u.month);
    let out_of_range = items
        .iter()
        .filter_map(|i| i.date.as_deref())
        .filter(|d| !d.starts_with(&prefix))
        .count();
    if out_of_range > 0 {
        errors.push(format!("{out_of_range} item(s) have a date outside {prefix}"));
    }

    // --- 4. null 比率 ---
    let nulls = items.iter().filter(|i| i.date.is_none()).count();
    let ratio = if items.is_empty() {
        0.0
    } else {
        nulls as f32 / items.len() as f32
    };
    if !items.is_empty() && nulls * MIN_NULL_DATE_HEADROOM >= items.len() {
        errors.push(format!(
            "date null ratio {ratio:.2} >= 0.10 ({nulls}/{})",
            items.len()
        ));
    }

    // --- 5. 重複（date, title の組）---
    //
    // **title だけで判定してはいけない。** `INTERNET Watchアクセスランキング` は
    // 週次ランキングなので毎週同じ題で出る（実測 2007-08 に 3 件）。
    // `China tests anti-satellite missile` も 1/11 と 1/18 の別項目。
    // dedup が `(date, title)` で統合している以上、verify も同じ粒度で見る。
    let mut seen = std::collections::HashSet::new();
    let mut dups = 0usize;
    for item in &items {
        let key = (item.date.clone(), normalize_key(&item.title));
        if !seen.insert(key) {
            dups += 1;
        }
    }
    if dups > 0 {
        errors.push(format!("{dups} duplicate (date, title) pair(s)"));
    }

    // --- 6. docs の truncation ---
    let docs_dir = crate::store::docs_dir(cfg, u);
    if let Ok(entries) = std::fs::read_dir(&docs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(doc): Option<serde_json::Value> = crate::store::read_json(&path) else {
                warnings.push(format!("unreadable doc: {}", path.display()));
                continue;
            };
            let truncated = doc
                .get("finish_reasons")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .any(|s| s.ends_with(":length"))
                })
                .unwrap_or(false);
            if truncated {
                errors.push(format!(
                    "truncated output in {}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
                ));
            }
        }
    }

    // 情報として残す（失敗扱いにしない）。
    if let Some(w) = value
        .get("coverage")
        .and_then(|c| c.get("warnings"))
        .and_then(|w| w.as_array())
    {
        for entry in w {
            if let Some(text) = entry.as_str() {
                warnings.push(text.to_owned());
            }
        }
    }

    Ok(VerifyReport {
        unit: u.to_string(),
        ok: errors.is_empty(),
        count: items.len(),
        null_date_ratio: ratio,
        warnings,
        errors,
    })
}

/// 台帳や成果物を書き換えないことを保証するために、読み取り専用の入口だけを公開する。
#[allow(dead_code)]
fn _never_writes(_p: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Lang;

    fn cfg(tag: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("ac-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Config::default();
        c.store.data_dir = dir.to_string_lossy().into_owned();
        c
    }

    fn key() -> UnitKey {
        UnitKey {
            year: 2007,
            month: 4,
            lang: Lang::Ja,
        }
    }

    fn write_news(cfg: &Config, body: serde_json::Value) {
        let dir = crate::store::unit_dir(cfg, &key());
        crate::store::write_atomic(
            &dir.join("news.json"),
            serde_json::to_vec(&body).unwrap().as_slice(),
        )
        .expect("write");
    }

    fn good_items(n: usize) -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| {
                serde_json::json!({
                    "date": format!("2007-04-{:02}", i % 30 + 1),
                    "title": format!("t{i:03}"),
                    "summary": "s",
                })
            })
            .collect()
    }

    #[test]
    fn accepts_well_formed_news() {
        let c = cfg("ok");
        let items = good_items(40);
        write_news(
            &c,
            serde_json::json!({
                "schema_version": 1,
                "count": items.len(),
                "items": items,
            }),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(r.ok, "errors: {:?}", r.errors);
        assert_eq!(r.count, 40);
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn missing_items_is_an_error() {
        let c = cfg("noitems");
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": 40}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("items is missing")));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn count_mismatch_is_an_error() {
        let c = cfg("count");
        let items = good_items(40);
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": 999, "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("count 999")));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn wrong_schema_version_is_an_error() {
        let c = cfg("schema");
        let items = good_items(40);
        write_news(
            &c,
            serde_json::json!({"schema_version": 2, "count": items.len(), "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn date_outside_unit_is_an_error() {
        let c = cfg("date");
        let mut items = good_items(40);
        items[0]["date"] = serde_json::json!("2007-05-01");
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": items.len(), "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("outside 2007-04")));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn high_null_ratio_is_an_error() {
        let c = cfg("null");
        let mut items = good_items(40);
        for item in items.iter_mut().take(10) {
            item["date"] = serde_json::Value::Null;
        }
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": items.len(), "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok, "10/40 = 25% must fail");
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn duplicate_titles_are_an_error() {
        let c = cfg("dup");
        let mut items = good_items(40);
        // 同じ日付・同じタイトルは重複。
        items[1]["date"] = items[0]["date"].clone();
        items[1]["title"] = items[0]["title"].clone();
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": items.len(), "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("duplicate (date, title)")));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn same_title_on_different_days_is_allowed() {
        // 週次ランキングのように、同じ題が毎週出るのは正常。
        let c = cfg("weekly");
        let mut items = good_items(40);
        items[1]["title"] = items[0]["title"].clone();
        items[1]["date"] = serde_json::json!("2007-04-15");
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": items.len(), "items": items}),
        );
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(r.ok, "errors: {:?}", r.errors);
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn truncated_doc_is_an_error() {
        let c = cfg("trunc");
        let items = good_items(40);
        write_news(
            &c,
            serde_json::json!({"schema_version": 1, "count": items.len(), "items": items}),
        );
        let doc_path = crate::store::docs_dir(&c, &key()).join("x-ja_year_article.json");
        crate::store::write_atomic(
            &doc_path,
            serde_json::json!({"finish_reasons": ["r0:length"]})
                .to_string()
                .as_bytes(),
        )
        .expect("write doc");
        let r = verify_unit(&c, &key()).expect("verify");
        assert!(!r.ok);
        assert!(r.errors.iter().any(|e| e.contains("truncated")));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }

    #[test]
    fn missing_news_is_unreadable() {
        let c = cfg("missing");
        assert!(matches!(
            verify_unit(&c, &key()),
            Err(VerifyError::Unreadable(_))
        ));
        let _ = std::fs::remove_dir_all(&c.store.data_dir);
    }
}
