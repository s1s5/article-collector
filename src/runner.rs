use std::path::PathBuf;

use crate::config::Config;
use crate::discover::{self, Section, SourceKind, UrlEntry, UrlPlan};
use crate::ledger::{self, LedgerRow};
use crate::map::{self, LlmConfig, MapError, NewsItem};
use crate::sanitize;
use crate::slice;
use crate::store;
use crate::unit::{Lang, UnitKey};

pub const STAGE_A: &str = "a";
pub const STAGE_B: &str = "b";
pub const STAGE_C: &str = "c";
pub const STAGE_D: &str = "d";

#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub items: usize,
    pub chars: usize,
    pub finish_reasons: Vec<String>,
    pub prompt_sha256: String,
    pub attempts: u8,
    pub chunks: usize,
    pub expected_min: usize,
    pub docs_written: usize,
    pub sources_failed: usize,
    pub duration_ms: u64,
}

/// urls.json のパス。
fn urls_path(cfg: &Config, key: &UnitKey) -> PathBuf {
    store::unit_dir(cfg, key).join("urls.json")
}

/// フェーズ A: 候補 URL を決めて `urls.json` を書く（既存があれば読むだけ）。
pub async fn run_stage_a(
    cfg: &Config,
    key: &UnitKey,
    force: bool,
) -> Result<UrlPlan, discover::DiscoverError> {
    let path = urls_path(cfg, key);
    if !force {
        if let Some(plan) = store::read_json::<UrlPlan>(&path) {
            return Ok(plan);
        }
    }
    let plan = discover::discover(key, cfg).await?;
    let bytes = serde_json::to_vec_pretty(&plan).map_err(|e| discover::DiscoverError::Io(e.to_string()))?;
    store::write_atomic(&path, &bytes).map_err(|e| discover::DiscoverError::Io(e.to_string()))?;
    Ok(plan)
}

/// 件数不足での再入。検索クエリを足して `urls.json` を**上書き**する。
///
/// **呼び出し側が unit あたり 1 回だけ呼ぶこと。** ここでは上限を持たない
/// （再入の制御は `main.rs` の `run_unit` にある）。
pub async fn run_stage_a_expand(
    cfg: &Config,
    key: &UnitKey,
) -> Result<UrlPlan, discover::DiscoverError> {
    let plan = discover::discover_with(key, cfg, discover::DiscoverPass::Expand).await?;
    let path = urls_path(cfg, key);
    let bytes = serde_json::to_vec_pretty(&plan).map_err(|e| discover::DiscoverError::Io(e.to_string()))?;
    store::write_atomic(&path, &bytes).map_err(|e| discover::DiscoverError::Io(e.to_string()))?;
    Ok(plan)
}

/// 1 つの取得候補から、月セクションを切り出す。
///
/// ja は親見出しの指定が必須。`## できごと` と `## 誕生` に同じ `### N月` があり、
/// 親を見ないと誕生記事を拾う（実測で 12 個ずつ存在）。
fn slice_for(key: &UnitKey, source: &UrlEntry, markdown: &str) -> Option<slice::MonthSection> {
    // 決定的ソース（Wikipedia の年記事）は月見出しの構造が既知なので専用のスライサー。
    // 検索由来は構造がバラバラ（報道サイト・事件DB・政府白書）なので汎用の日付行スキャン。
    match (key.lang, &source.section) {
        (Lang::Ja, Some(Section::JaKigoto)) => {
            slice::slice_month_under(markdown, key.month, "できごと")
        }
        (Lang::Ja, Some(Section::JaYotei)) => {
            slice::slice_month_under(markdown, key.month, "予定・予測")
        }
        (Lang::En, Some(Section::EnEvents)) => slice::slice_en_month(markdown, key.month),
        // Portal は月ページ全体がその月なので、日付行でブロック化する。
        (Lang::En, Some(Section::Whole)) => slice::slice_portal(markdown, key.year, key.month),
        // ここから下は検索由来。section が None のものが該当する。
        _ => {
            // Portal の URL は section を持たないまま検索から来ることもあるので、
            // まず Portal 形式を試す（先頭が `April5,2026(2026-04-05)` の形）。
            if source.url.contains("Portal:Current_events") {
                if let Some(section) = slice::slice_portal(markdown, key.year, key.month) {
                    return Some(section);
                }
            }
            slice::slice_generic(markdown, key.year, key.month)
                // 汎用で取れないページでも Wikipedia の構造を持つ場合があるのでフォールバック。
                .or_else(|| match key.lang {
                    Lang::Ja => slice::slice_month(markdown, key.month),
                    Lang::En => slice::slice_en_month(markdown, key.month),
                })
        }
    }
}

fn kind_label(kind: &SourceKind) -> &'static str {
    match kind {
        SourceKind::JaYearArticle => "ja_year_article",
        SourceKind::EnYearArticle => "en_year_article",
        SourceKind::EnPortal => "en_portal",
        SourceKind::NipponMonth => "nippon_month",
        SourceKind::JikenTimeline => "jiken_timeline",
        SourceKind::ResponseMonth => "response_month",
        SourceKind::ResponseDay => "response_day",
        SourceKind::WikinewsMonth => "wikinews_month",
        SourceKind::SearchHit => "search_hit",
    }
}

fn sha8(url: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    hasher.finalize()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// フェーズ B: 候補 URL を順に取得し、URL ごとに doc を書き、最後に news.json を組み立てる。
///
/// **1 本の失敗で全体を止めない。** 2007-01 の `予定・予測` は存在しないので、
/// そこで打ち切ると `できごと` の doc すら残らなくなる。
pub async fn run_stage_b(cfg: &Config, key: &UnitKey, plan: &UrlPlan) -> Result<Metrics, String> {
    let started = std::time::Instant::now();
    let llm = LlmConfig::from_cfg(&cfg.llm);
    let mut metrics = Metrics::default();
    let mut items: Vec<NewsItem> = Vec::new();

    for source in &plan.sources {
        let label = kind_label(&source.kind);
        eprintln!("[{key}] fetch {} ({label})", source.url);

        let doc = match crate::fetch::fetch_page(
            &cfg.fetch.crawl4ai_url,
            &source.url,
            cfg.fetch.interval_ms,
        )
        .await
        {
            Ok(crate::fetch::FetchOutcome::Ok(doc)) => doc,
            Ok(crate::fetch::FetchOutcome::MissingPage { url }) => {
                eprintln!("[{key}]   missing page ({label}): {url}");
                metrics.sources_failed += 1;
                continue;
            }
            Ok(crate::fetch::FetchOutcome::EmptyContent { url }) => {
                eprintln!("[{key}]   empty content ({label}): {url}");
                metrics.sources_failed += 1;
                continue;
            }
            Err(e) => {
                eprintln!("[{key}]   transport error: {e}");
                metrics.sources_failed += 1;
                continue;
            }
        };

        let Some(section) = slice_for(key, source, &doc.markdown) else {
            eprintln!("[{key}]   no section for this month in {label}");
            metrics.sources_failed += 1;
            continue;
        };
        let chars = sanitize::section_chars(&section);
        eprintln!(
            "[{key}]   slice path={:?} blocks={} sanitized={chars} chars",
            section.heading_path,
            section.blocks.len()
        );

        let result = match map::extract_section(&llm, &section, key.year, key.month, key.lang).await {
            Ok(r) => r,
            Err(MapError::Truncated { finish }) => {
                return Err(format!(
                    "output truncated (finish_reason={finish}); think must stay false"
                ))
            }
            Err(e) => {
                eprintln!("[{key}]   extract failed: {e}");
                metrics.sources_failed += 1;
                continue;
            }
        };

        let doc_path = store::docs_dir(cfg, key).join(format!(
            "{}-{}.json",
            sha8(&source.url),
            label
        ));
        let doc_body = serde_json::json!({
            "url": doc.url,
            "kind": label,
            "section": source.section,
            "heading_path": section.heading_path,
            "raw_chars": doc.chars,
            "fetched_at": doc.fetched_at,
            "chars": chars,
            "finish_reasons": result.finish_reasons,
            "attempts": result.attempts,
            "chunks": result.chunks,
            "expected_min": result.expected_min,
            "items": result.items,
        });
        store::write_atomic(
            &doc_path,
            serde_json::to_vec_pretty(&doc_body)
                .map_err(|e| e.to_string())?
                .as_slice(),
        )
        .map_err(|e| format!("write {}: {e}", doc_path.display()))?;

        metrics.docs_written += 1;
        metrics.chars = metrics.chars.max(chars);
        metrics.finish_reasons.extend(result.finish_reasons);
        metrics.prompt_sha256 = result.prompt_sha256;
        metrics.attempts = metrics.attempts.max(result.attempts);
        metrics.chunks += result.chunks;
        metrics.expected_min += result.expected_min;
        items.extend(result.items);
    }

    if metrics.docs_written == 0 {
        return Err("no document could be produced for this unit".to_owned());
    }

    // 重複除去（同じ date + 同じ title）。セクション違いで同じ事象が出ることがある。
    let before = items.len();
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert((item.date.clone(), item.title.clone())));
    if items.len() != before {
        eprintln!("[{key}] dedup exact {before} -> {}", items.len());
    }
    metrics.items = items.len();

    // 日付昇順、null は末尾。
    items.sort_by(|a, b| match (&a.date, &b.date) {
        (Some(x), Some(y)) => x.cmp(y).then_with(|| a.title.cmp(&b.title)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.title.cmp(&b.title),
    });

    let news_path = store::unit_dir(cfg, key).join("news.json");
    let news = serde_json::json!({
        "schema_version": 1,
        "unit": key.to_string(),
        "model": llm.model,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "count": items.len(),
        "items": items,
    });
    store::write_atomic(
        &news_path,
        serde_json::to_vec_pretty(&news)
            .map_err(|e| e.to_string())?
            .as_slice(),
    )
    .map_err(|e| format!("write {}: {e}", news_path.display()))?;

    metrics.duration_ms = started.elapsed().as_millis() as u64;
    eprintln!(
        "[{key}] wrote {} items from {} doc(s), {} source(s) failed, {} ms -> {}",
        metrics.items,
        metrics.docs_written,
        metrics.sources_failed,
        metrics.duration_ms,
        news_path.display()
    );
    Ok(metrics)
}

/// 台帳に結果を書く。
pub fn record(cfg: &Config, key: &UnitKey, stage: &str, metrics: &Metrics) -> Result<(), String> {
    let row = LedgerRow::new(key.to_string(), stage, "done")
        .with_model(&cfg.llm.model)
        .with_duration_ms(metrics.duration_ms)
        .with_counts(serde_json::json!({
            "items": metrics.items,
            "chars": metrics.chars,
            "docs": metrics.docs_written,
            "sources_failed": metrics.sources_failed,
            "finish_reasons": metrics.finish_reasons,
            "prompt_sha256": metrics.prompt_sha256,
            "attempts": metrics.attempts,
            "chunks": metrics.chunks,
            "expected_min": metrics.expected_min,
        }));
    ledger::append(&store::ledger_path(cfg), &row).map_err(|e| format!("append ledger: {e}"))
}

pub fn record_failure(cfg: &Config, key: &UnitKey, stage: &str, error: &str) {
    let row = LedgerRow::new(key.to_string(), stage, "failed").with_error(error);
    let _ = ledger::append(&store::ledger_path(cfg), &row);
}

/// フェーズ C など、`Metrics` を伴わない完了を台帳に書く。
pub fn record_stage_done(
    cfg: &Config,
    key: &UnitKey,
    stage: &str,
    counts: serde_json::Value,
) -> Result<(), String> {
    let row = LedgerRow::new(key.to_string(), stage, "done")
        .with_model(&cfg.llm.model)
        .with_counts(counts);
    ledger::append(&store::ledger_path(cfg), &row).map_err(|e| format!("append ledger: {e}"))
}

/// フェーズ C: `docs/*.json` を読み、3 段の重複除去をして `dedup.json` を書く。
///
/// **docs が 1 件も無ければエラー。** 先に `--stage b` を回す必要がある。
pub async fn run_stage_c(cfg: &Config, key: &UnitKey) -> Result<crate::dedup::DedupResult, String> {
    let dir = store::docs_dir(cfg, key);
    let mut docs: Vec<crate::dedup::DocResult> = Vec::new();

    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                if let Some(doc) = store::read_json::<crate::dedup::DocResult>(&path) {
                    docs.push(doc);
                }
            }
        }
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    }

    if docs.is_empty() {
        return Err(format!(
            "no docs under {}; run --stage b first",
            dir.display()
        ));
    }

    // 読み込み順を固定する（ディレクトリ列挙の順は保証されない）。
    docs.sort_by(|a, b| a.url.cmp(&b.url));

    let result = crate::dedup::dedup(&key.to_string(), &docs, cfg).await;

    let path = store::unit_dir(cfg, key).join("dedup.json");
    store::write_atomic(
        &path,
        serde_json::to_vec_pretty(&result)
            .map_err(|e| e.to_string())?
            .as_slice(),
    )
    .map_err(|e| format!("write {}: {e}", path.display()))?;

    eprintln!(
        "[{key}] dedup {} -> {} (merged {}, llm_calls {}) -> {}",
        result.stats.input,
        result.stats.output,
        result.stats.merged,
        result.stats.llm_calls,
        path.display()
    );
    Ok(result)
}

/// フェーズ D: `dedup.json` を読み、重要度順に切って `news.json` / `sources.json` を書く。
///
/// **LLM を呼ばない。** ここが無料で何度でも回せることの価値。
/// `count < items.min` なら `NeedsExpand` を返し、呼び出し側（runner）が 1 回だけ再入する。
pub async fn run_stage_d(
    cfg: &Config,
    key: &UnitKey,
    plan: &crate::discover::UrlPlan,
    failed: u32,
) -> Result<crate::assemble::AssembleOutcome, String> {
    run_stage_d_with(cfg, key, plan, failed, false).await
}

/// `allow_below_min` を指定できる `run_stage_d`。
///
/// Expand の再入後は `true` で呼ぶ。**1 回広げて足りなければ成功として確定**する
/// （「30 件無い月」は実在しうる。無限に広げ直さない）。
pub async fn run_stage_d_with(
    cfg: &Config,
    key: &UnitKey,
    plan: &crate::discover::UrlPlan,
    failed: u32,
    allow_below_min: bool,
) -> Result<crate::assemble::AssembleOutcome, String> {
    let dedup_path = store::unit_dir(cfg, key).join("dedup.json");
    let dedup = store::read_json::<crate::dedup::DedupResult>(&dedup_path).ok_or_else(|| {
        format!(
            "no dedup.json at {}; run --stage c first",
            dedup_path.display()
        )
    })?;

    // 通常経路は下限未満を `NeedsExpand` として返し、Expand 経路だけが
    // `below_min` を記録して確定する。2 つの入口を明示的に使い分ける。
    let outcome = if allow_below_min {
        crate::assemble::assemble_with(
            &key.to_string(),
            &cfg.llm.model,
            &dedup,
            plan,
            failed,
            cfg,
            true,
        )
    } else {
        crate::assemble::assemble(
            &key.to_string(),
            &cfg.llm.model,
            &dedup,
            plan,
            failed,
            cfg,
        )
    };

    match &outcome {
        crate::assemble::AssembleOutcome::NeedsExpand { count, min } => {
            eprintln!("[{key}] assemble: {count} < min {min}; needs expand");
        }
        crate::assemble::AssembleOutcome::Done(digest) => {
            let dir = store::unit_dir(cfg, key);
            let news_path = dir.join("news.json");
            store::write_atomic(
                &news_path,
                serde_json::to_vec_pretty(digest)
                    .map_err(|e| e.to_string())?
                    .as_slice(),
            )
            .map_err(|e| format!("write {}: {e}", news_path.display()))?;

            let sources_path = dir.join("sources.json");
            let report = crate::assemble::sources_report(&key.to_string(), &dedup, plan);
            store::write_atomic(
                &sources_path,
                serde_json::to_vec_pretty(&report)
                    .map_err(|e| e.to_string())?
                    .as_slice(),
            )
            .map_err(|e| format!("write {}: {e}", sources_path.display()))?;

            eprintln!(
                "[{key}] assemble {} -> {} items (dropped {}) -> {}",
                dedup.stats.output,
                digest.count,
                dedup.stats.output.saturating_sub(digest.count),
                news_path.display()
            );
        }
    }
    Ok(outcome)
}


/// 組み立て済みのダイジェストを `news.json` に書き直す。
///
/// Expand の再入後に `coverage.expanded` と `below_min` を足すために使う。
/// `sources.json` は `run_stage_d` が既に書いているので触らない。
pub fn write_digest(
    cfg: &Config,
    key: &UnitKey,
    digest: &crate::assemble::MonthlyDigest,
) -> Result<(), String> {
    let news_path = store::unit_dir(cfg, key).join("news.json");
    store::write_atomic(
        &news_path,
        serde_json::to_vec_pretty(digest)
            .map_err(|e| e.to_string())?
            .as_slice(),
    )
    .map_err(|e| format!("write {}: {e}", news_path.display()))
}

/// スキップ判定: 台帳が done で、`news.json` がパースでき、schema_version が現行。
pub fn is_finished(cfg: &Config, key: &UnitKey, stage: &str) -> bool {
    if !ledger::is_done(&store::ledger_path(cfg), &key.to_string(), stage) {
        return false;
    }
    let news_path = store::unit_dir(cfg, key).join("news.json");
    let parsed: Option<serde_json::Value> = store::read_json(&news_path);
    matches!(
        parsed.and_then(|v| v.get("schema_version").and_then(|s| s.as_u64())),
        Some(1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_labels_are_snake_case() {
        assert_eq!(kind_label(&SourceKind::JaYearArticle), "ja_year_article");
        assert_eq!(kind_label(&SourceKind::EnYearArticle), "en_year_article");
        assert_eq!(kind_label(&SourceKind::SearchHit), "search_hit");
    }

    #[test]
    fn sha8_is_stable_and_short() {
        let a = sha8("https://example.com/x");
        assert_eq!(a, sha8("https://example.com/x"));
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn ja_kigoto_and_yotei_pick_different_parents() {
        let doc = "\
## できごと
### 1月
  * [1月1日](x \"1月1日\")
    * 出来事。
## 予定・予測
### 1月
  * [1月2日](x \"1月2日\")
    * 予定。
## 誕生
### 1月
  * [1月9日](x \"1月9日\")
    * 生誕。
";
        let key = UnitKey {
            year: 2007,
            month: 1,
            lang: Lang::Ja,
        };
        let kigoto = UrlEntry {
            url: "u".into(),
            kind: SourceKind::JaYearArticle,
            priority: 10,
            section: Some(Section::JaKigoto),
            query: None,
        };
        let yotei = UrlEntry {
            section: Some(Section::JaYotei),
            ..kigoto.clone()
        };
        let a = slice_for(&key, &kigoto, doc).expect("kigoto");
        assert_eq!(a.heading_path[0], "できごと");
        let b = slice_for(&key, &yotei, doc).expect("yotei");
        assert_eq!(b.heading_path[0], "予定・予測");
        // 誕生は選ばれない
        assert!(!a.blocks.iter().flat_map(|x| &x.lines).any(|l| l.contains("生誕")));
    }
}
