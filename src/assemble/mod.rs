pub mod rank;

use crate::config::Config;
use crate::dedup::DedupResult;
use crate::discover::UrlPlan;
use crate::map::NewsItem;

use rank::rank_key;

pub const SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Coverage {
    pub sources_used: u32,
    pub sources_failed: u32,
    pub warnings: Vec<String>,
    pub expanded: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MonthlyDigest {
    pub schema_version: u16,
    pub unit: String,
    pub model: String,
    pub generated_at: String,
    pub count: usize,
    pub items: Vec<NewsItem>,
    pub coverage: Coverage,
}

#[derive(Debug)]
pub enum AssembleOutcome {
    Done(Box<MonthlyDigest>),
    /// 下限に届かない。呼び出し側（runner）が 1 回だけ discover を再入する。
    NeedsExpand { count: usize, min: usize },
}

/// フェーズ D。**LLM を呼ばない純粋関数。**
///
/// - 並び: `date` 昇順（null は末尾）、同日内は `rank_key` 昇順
/// - `count > items.max` → 重要度上位 `max` 件に切る
/// - `count < items.min` → `NeedsExpand`（勝手に閾値を下げない）
///
/// 実測（2007-04 ja）: dedup 676 クラスタ → 200 件に切り詰め。
/// IT 系索引（priority 90）が大半を占めるため、年記事（priority 10）が
/// 先頭に来ることを `rank_key` で担保する。
pub fn assemble(
    unit: &str,
    model: &str,
    dedup: &DedupResult,
    plan: &UrlPlan,
    failed: u32,
    cfg: &Config,
) -> AssembleOutcome {
    assemble_with(unit, model, dedup, plan, failed, cfg, false)
}

/// `allow_below_min` が真なら、下限未満でも `Done` を返して `below_min` を記録する。
///
/// Expand（再入）の 1 回目で使う。**「30 件無い月」は実在しうる**ので、
/// 何度でも広げ直すのではなく、1 回広げて足りなければ成功として確定させる。
/// 実測: en 2000-04 は年記事に事件が 2 件しかない。
pub fn assemble_with(
    unit: &str,
    model: &str,
    dedup: &DedupResult,
    plan: &UrlPlan,
    failed: u32,
    cfg: &Config,
    allow_below_min: bool,
) -> AssembleOutcome {
    let min = cfg.items.min;
    let max = cfg.items.max;

    // クラスタを重要度順に並べる。安定ソートなので同点は入力順。
    let mut clusters = dedup.clusters.clone();
    clusters.sort_by_key(rank_key);

    if clusters.len() < min && !allow_below_min {
        return AssembleOutcome::NeedsExpand {
            count: clusters.len(),
            min,
        };
    }
    // **検索ヒット（priority 90）の占有率に上限をかける。**
    // 実測 2007: IT 系索引（internet.watch / k-tai.watch / itmedia）が
    // 「大きなニュース」の枠を圧迫した。priority 90 を ratio までに抑え、
    // 空いた枠は priority の低いソースで埋める。
    let ratio = cfg.items.search_hit_ratio;
    if ratio > 0.0 && ratio < 1.0 {
        let capacity = ((max as f32) * ratio) as usize;
        let mut search_hits = 0usize;
        clusters.retain(|c| {
            if c.best_priority >= 90 {
                search_hits += 1;
                search_hits <= capacity
            } else {
                true
            }
        });
        eprintln!(
            "       capped search_hits at {capacity} (was {search_hits})"
        );
    }

    if clusters.len() > max {
        clusters.truncate(max);
    }

    // **日付なしの比率を 10% 未満に抑える。**
    // 実測 2007-01 ja: 年記事（`ja.wikipedia.org/wiki/2007年`）を汎用スライサーで
    // 切ると日付のない行が 57 件混ざり、null 26% になった。
    // 日付のある項目で枠を埋められるなら、日付なしは捨てる。
    let null_count = clusters.iter().filter(|c| c.date.is_none()).count();
    let dated_count = clusters.len() - null_count;
    let mut clusters = clusters;
    // 日付ありだけで下限を満たせるなら、日付なしは落とす。
    // 最初 `max * 9/10` を要求したら、151/203 の月で発動しなかった（実測）。
    if null_count * 10 >= clusters.len() && dated_count >= min {
        clusters.retain(|c| c.date.is_some());
        eprintln!(
            "       dropped {null_count} undated cluster(s) (kept {dated_count} dated, min {min})"
        );
    }

    // 最終の並びは date 昇順（null 末尾）。同日内は重要度順を保つ。
    let mut items: Vec<NewsItem> = clusters
        .iter()
        .map(|c| NewsItem {
            date: c.date.clone(),
            title: c.title.clone(),
            summary: c.summary.clone(),
        })
        .collect();
    items.sort_by(|a, b| match (&a.date, &b.date) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    let nulls = items.iter().filter(|i| i.date.is_none()).count();
    let mut warnings = dedup.warnings.clone();
    if nulls * 10 >= items.len() && !items.is_empty() {
        warnings.push(format!(
            "high_null_date_ratio:{nulls}/{}",
            items.len()
        ));
    }
    if dedup.stats.llm_calls > 0 {
        warnings.push(format!("dedup_llm_calls:{}", dedup.stats.llm_calls));
    }
    if allow_below_min && clusters.len() < min {
        warnings.push(format!("below_min:{}<{min}", clusters.len()));
    }

    let sources_used = plan.sources.len() as u32;
    let digest = MonthlyDigest {
        schema_version: SCHEMA_VERSION,
        unit: unit.to_owned(),
        model: model.to_owned(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        count: items.len(),
        items,
        coverage: Coverage {
            sources_used,
            sources_failed: failed,
            warnings,
            expanded: false,
        },
    };
    AssembleOutcome::Done(Box::new(digest))
}

/// 採用した item の出所を `sources.json` 用に畳む。
///
/// `used_items` は「その URL を出所に持つクラスタ数」。重複 URL は 1 本に。
pub fn sources_report(unit: &str, dedup: &DedupResult, plan: &UrlPlan) -> serde_json::Value {
    let mut counts: Vec<(String, u32)> = Vec::new();
    for cluster in &dedup.clusters {
        for url in &cluster.sources {
            match counts.iter_mut().find(|(u, _)| u == url) {
                Some((_, n)) => *n += 1,
                None => counts.push((url.clone(), 1)),
            }
        }
    }
    // 決定的な順序: used_items 降順 → url 昇順。
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let kind_of = |url: &str| -> String {
        plan.sources
            .iter()
            .find(|s| s.url == url)
            .map(|s| format!("{:?}", s.kind).to_lowercase())
            .unwrap_or_else(|| "unknown".to_owned())
    };

    serde_json::json!({
        "unit": unit,
        "sources": counts
            .into_iter()
            .map(|(url, used)| serde_json::json!({
                "url": url,
                "kind": kind_of(&url),
                "used_items": used,
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup::{Cluster, DedupStats};
    use crate::discover::{Section, SourceKind, UrlEntry};

    fn cluster(priority: u8, date: Option<&str>, title: &str) -> Cluster {
        Cluster {
            date: date.map(|d| d.to_owned()),
            title: title.to_owned(),
            summary: "summary".to_owned(),
            sources: vec![format!("https://example.com/{title}")],
            members: 1,
            best_priority: priority,
        }
    }

    fn dedup_of(clusters: Vec<Cluster>) -> DedupResult {
        let output = clusters.len();
        DedupResult {
            unit: "2007-04/ja".to_owned(),
            clusters,
            stats: DedupStats {
                input: output,
                merged: 0,
                output,
                llm_calls: 0,
            },
            warnings: vec![],
        }
    }

    fn plan() -> UrlPlan {
        UrlPlan {
            unit: "2007-04/ja".to_owned(),
            generated_at: "2026-10-29T00:00:00Z".to_owned(),
            sources: vec![UrlEntry {
                url: "https://example.com/x".to_owned(),
                kind: SourceKind::JaYearArticle,
                priority: 10,
                section: Some(Section::JaKigoto),
                query: None,
            }],
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn digest(outcome: AssembleOutcome) -> MonthlyDigest {
        match outcome {
            AssembleOutcome::Done(d) => *d,
            AssembleOutcome::NeedsExpand { count, min } => {
                panic!("expected Done, got NeedsExpand({count} < {min})")
            }
        }
    }

    #[test]
    fn caps_at_max() {
        // 検索ヒット（priority 90）は 3 割上限があるので、決定的ソースと混ぜる。
        let mut clusters: Vec<Cluster> = (0..150)
            .map(|i| cluster(10, Some("2007-04-01"), &format!("y{i:03}")))
            .collect();
        clusters.extend(
            (0..150).map(|i| cluster(90, Some("2007-04-01"), &format!("s{i:03}"))),
        );
        let d = digest(assemble(
            "2007-04/ja",
            "ornith-1.5:35b",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        ));
        assert_eq!(d.count, 200);
        assert_eq!(d.items.len(), 200);
    }

    #[test]
    fn needs_expand_below_min() {
        let clusters: Vec<Cluster> = (0..29)
            .map(|i| cluster(90, Some("2007-04-01"), &format!("t{i:02}")))
            .collect();
        let outcome = assemble(
            "2007-04/ja",
            "ornith-1.5:35b",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        );
        assert!(matches!(
            outcome,
            AssembleOutcome::NeedsExpand { count: 29, min: 30 }
        ));
    }

    #[test]
    fn sorts_dates_and_nulls_last() {
        // 下限 30 を満たしつつ、日付順と null 末尾を確かめる。
        let mut clusters: Vec<Cluster> = (1..=28)
            .map(|day| {
                let date = format!("2007-04-{day:02}");
                cluster(10, Some(&date), &format!("d{day:02}"))
            })
            .collect();
        // 逆順の日付と null を混ぜる。
        clusters.push(cluster(10, Some("2007-04-29"), "z-late"));
        clusters.push(cluster(10, None, "null-a"));
        clusters.push(cluster(10, None, "null-b"));

        let d = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        ));

        let dates: Vec<&str> = d
            .items
            .iter()
            .filter_map(|i| i.date.as_deref())
            .collect();
        let mut sorted = dates.clone();
        sorted.sort();
        assert_eq!(dates, sorted, "date must ascend");

        // null は必ず末尾。
        let first_null = d.items.iter().position(|i| i.date.is_none());
        if let Some(pos) = first_null {
            assert!(
                d.items[pos..].iter().all(|i| i.date.is_none()),
                "nulls must be contiguous at the end"
            );
        }
    }

    #[test]
    fn prefers_year_article_over_search_hit() {
        // 2007-04 の実測: IT 系索引（priority 90）が 555 件を占める。
        // 200 件に切るときに年記事（priority 10）が落ちてはいけない。
        let mut clusters = vec![cluster(10, Some("2007-04-01"), "year")];
        // 検索ヒットは 3 割上限で 60 件まで。他の決定的ソースで枠を埋める。
        clusters.extend(
            (0..300).map(|i| cluster(90, Some("2007-04-01"), &format!("s{i:03}"))),
        );
        clusters.extend(
            (0..200).map(|i| cluster(20, Some("2007-04-01"), &format!("p{i:03}"))),
        );
        let d = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        ));
        assert_eq!(d.count, 200);
        assert!(
            d.items.iter().any(|i| i.title == "year"),
            "年記事は必ず残る"
        );
    }

    #[test]
    fn caps_search_hits_at_ratio() {
        // 実測 2007-03 ja: 643 件のうち過半が IT 系索引（priority 90）だった。
        // 切り詰め（max=200）が効かない規模にして、上限だけを見る。
        let mut clusters: Vec<Cluster> = (0..100)
            .map(|i| cluster(10, Some("2007-04-01"), &format!("y{i:03}")))
            .collect();
        clusters.extend(
            (0..300).map(|i| cluster(90, Some("2007-04-01"), &format!("s{i:03}"))),
        );
        let d = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        ));
        let hits = d.items.iter().filter(|i| i.title.starts_with('s')).count();
        assert_eq!(hits, 60, "200 件の 3 割 = 60 件まで");
        assert_eq!(d.items.len(), 160, "年記事 100 + 検索ヒット 60");
    }

    #[test]
    fn is_deterministic_except_generated_at() {
        let clusters: Vec<Cluster> = (0..40)
            .map(|i| cluster(90, Some("2007-04-01"), &format!("t{i:02}")))
            .collect();
        let a = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(clusters.clone()),
            &plan(),
            0,
            &cfg(),
        ));
        let b = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(clusters),
            &plan(),
            0,
            &cfg(),
        ));
        assert_eq!(a.items.len(), b.items.len());
        for (x, y) in a.items.iter().zip(&b.items) {
            assert_eq!(x.date, y.date);
            assert_eq!(x.title, y.title);
            assert_eq!(x.summary, y.summary);
        }
    }

    #[test]
    fn schema_version_is_one() {
        let d = digest(assemble(
            "2007-04/ja",
            "m",
            &dedup_of(
                (0..30)
                    .map(|i| cluster(10, Some("2007-04-01"), &format!("a{i:02}")))
                    .collect(),
            ),
            &plan(),
            0,
            &cfg(),
        ));
        assert_eq!(d.schema_version, 1);
    }

    #[test]
    fn sources_report_counts_usage() {
        let dd = dedup_of(vec![
            cluster(10, Some("2007-04-01"), "a"),
            cluster(10, Some("2007-04-02"), "b"),
        ]);
        let report = sources_report("2007-04/ja", &dd, &plan());
        let sources = report["sources"].as_array().expect("array");
        assert_eq!(sources.len(), 2, "URL ごとに 1 行");
        assert!(sources.iter().all(|s| s["used_items"] == 1));
    }
}
