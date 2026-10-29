pub mod probe;

use std::sync::{Arc, Mutex};

use rig_agent::prelude::*;
use rig_agent::tool::DynamicTool;
use rig_core::completion::{GenerationOptions, Reasoning};
use rig_core::message::ToolName;
use rig_core::providers::ollama::OllamaConfig;
use rig_core::tool::ToolOutput;

use super::{ja, SourceKind, UrlEntry, UrlPlan};
use crate::config::Config;
use crate::unit::UnitKey;

pub use probe::{probe_page, PageProbe, ProbeStatus};

/// エージェントが登録した URL の集合。重複は加算しない。
///
/// **`fetch_page` で調べた `month_hits` も覚えておく。** `register_url` は
/// これを見て「その月の日付を含まないページ」を拒否する。
/// 実測: エージェントは yearwiz（年号換算ページ）を「2007年の月別ニュースを
/// 含む」という自己申告付きで登録した。**自己申告を信じてはいけない。**
#[derive(Default)]
struct Registry {
    entries: Vec<UrlEntry>,
    probed: std::collections::HashMap<String, u32>,
}

impl Registry {
    /// **URL だけで重複を判定してはいけない。**
    /// ja の `できごと` と `予定・予測` は同じ URL で `section` が違う。
    /// URL だけで弾くと 2 件目が消え、`ja_year_article` が 1 件になる
    /// （実測: S11 の検証でこれが起きた）。
    /// `merge_sources` と同じ `(URL, section)` の組で見る。
    fn push(&mut self, entry: UrlEntry) -> usize {
        let same = |e: &UrlEntry| {
            super::normalize_url(&e.url) == super::normalize_url(&entry.url)
                && e.section == entry.section
        };
        if self.entries.iter().any(same) {
            return self.entries.len();
        }
        self.entries.push(entry);
        self.entries.len()
    }

    fn record_probe(&mut self, url: &str, month_hits: u32) {
        self.probed.insert(url.to_owned(), month_hits);
    }

    /// 登録可否。調べていない URL は**拒否**する（先に `fetch_page` を要求）。
    /// 既に登録済みの URL は再登録しても件数が増えないだけなので許す。
    fn admission(&self, url: &str) -> Result<(), &'static str> {
        // 既に登録済みなら再登録は無害（件数が増えないだけ）。
        // ただし section 付きの既知候補と同じ URL を section なしで登録すると
        // 別エントリとして増えるので、ここでは URL 一致で「登録済み」とみなす。
        if self.entries.iter().any(|e| e.url == url) {
            return Ok(());
        }
        match self.probed.get(url) {
            Some(&hits) if hits > 0 => Ok(()),
            Some(_) => Err("no_month_hits"),
            None => Err("not_probed"),
        }
    }

    fn urls(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.url.clone()).collect()
    }
}

fn kind_from_url(url: &str) -> (SourceKind, u8) {
    let lower = url.to_lowercase();
    if lower.contains("jiken.jp") {
        return (SourceKind::JikenTimeline, 40);
    }
    if lower.contains("response.jp") {
        return (SourceKind::ResponseDay, 60);
    }
    if lower.contains("wikinews") {
        return (SourceKind::WikinewsMonth, 70);
    }
    if lower.contains("nippon.com") {
        return (SourceKind::NipponMonth, 30);
    }
    if lower.contains("portal:current_events") {
        return (SourceKind::EnPortal, 20);
    }
    if lower.contains("wikipedia.org") {
        return (SourceKind::SearchHit, 90);
    }
    (SourceKind::SearchHit, 90)
}

/// ターンごとのプロンプト。**同一文面を繰り返さない。**
///
/// 実測: 同じプロンプトを再送すると、モデルは同じクエリを投げ直した。
pub fn prompt_for_turn(
    turn: u8,
    registered: &[String],
    known: &[String],
    target: usize,
) -> String {
    let have = registered.len();
    let mut out = String::new();
    out.push_str(&format!(
        "あなたは {target} 件以上の URL を集める担当です。現在 {have} 件登録済み。\n"
    ));

    if !known.is_empty() {
        out.push_str("\n必ず登録すべき既知の候補（まだ未登録なら register_url で登録）:\n");
        for url in known {
            if !registered.contains(url) {
                out.push_str(&format!("- {url}\n"));
            }
        }
    }

    if !registered.is_empty() {
        out.push_str("\n登録済み（これ以外を探すこと。同じ URL を再登録しない）:\n");
        for url in registered.iter().take(20) {
            out.push_str(&format!("- {url}\n"));
        }
    }

    out.push_str("\n");
    match turn {
        0 => out.push_str(
            "手順:\n\
             1. 上の既知候補をすべて register_url する\n\
             2. search_searxng で追加を探す\n\
             3. 見つけた URL は fetch_page で中身を確認し、month_hits が 1 以上なら登録する\n\
             month_hits が 0 のページは登録してはいけない。\n",
        ),
        1 => out.push_str(&format!(
            "まだ足りません。**別のクエリ**で search_searxng を呼び、未登録のドメインを探してください。\n\
             残り {target} 件以上。\n",
            target = target.saturating_sub(have)
        )),
        2 => out.push_str(
            "月次アーカイブ（`/article/YYYY/MM/`）、事件データベース、業界紙のバックナンバーを狙ってください。\n",
        ),
        3 => out.push_str(
            "英語クエリも使えます。例: `{Month} {YYYY} news archive`。\n",
        ),
        _ => out.push_str("これで打ち切ります。finish を呼んでください。\n"),
    }
    out
}

/// エージェントでフェーズ A を回す。
///
/// **決定的候補は Rust 側で先に登録する。** 実測でエージェントが Wikipedia を
/// 落とすと下限 30 が崩れた。プロンプト依存を減らすため、既知候補は
/// モデルを介さずに `Registry` へ入れる。
pub async fn agent_discover(u: &UnitKey, cfg: &Config) -> Result<UrlPlan, super::DiscoverError> {
    let known_entries = match u.lang {
        crate::unit::Lang::Ja => ja::ja_sources(u),
        crate::unit::Lang::En => super::en::en_sources(u),
    };
    let known_urls: Vec<String> = known_entries.iter().map(|e| e.url.clone()).collect();

    // 既知候補を先に登録しておく。
    let registry = Arc::new(Mutex::new(Registry::default()));
    {
        let mut reg = registry.lock().expect("registry poisoned");
        for entry in &known_entries {
            reg.push(entry.clone());
        }
    }

    let target = cfg.discover.target_urls;
    let target = target.max(2);

    // ---- ツール ----
    let reg_for_register = registry.clone();
    let register = DynamicTool::new(
        ToolName::new("register_url").expect("valid name"),
        "Register one URL for fetching. Rejects pages without that month's dates.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["url", "reason"]
        }),
        move |args: serde_json::Value| {
            let reg = reg_for_register.clone();
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if url.is_empty() {
                    return Ok(ToolOutput::text(
                        serde_json::json!({"registered": 0, "rejected": "empty_url"}).to_string(),
                    ));
                }
                let (kind, priority) = kind_from_url(&url);
                let mut guard = reg.lock().expect("registry poisoned");
                if let Err(reason) = guard.admission(&url) {
                    let n = guard.entries.len();
                    return Ok(ToolOutput::text(
                        serde_json::json!({
                            "registered": n,
                            "rejected": reason,
                            "hint": "call fetch_page first; only register when month_hits >= 1"
                        })
                        .to_string(),
                    ));
                }
                let n = guard.push(UrlEntry {
                    url,
                    kind,
                    priority,
                    section: None,
                    query: Some("agent".to_owned()),
                });
                Ok(ToolOutput::text(
                    serde_json::json!({"registered": n}).to_string(),
                ))
            })
        },
    );

    let cfg_for_search = cfg.clone();
    let lang = u.lang;
    let search = DynamicTool::new(
        ToolName::new("search_searxng").expect("valid name"),
        "Search SearXNG for a query. Returns up to 10 {title,url,engine}.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            },
            "required": ["query"]
        }),
        move |args: serde_json::Value| {
            let cfg = cfg_for_search.clone();
            Box::pin(async move {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                match super::search::search(&cfg, &query, lang).await {
                    Ok(hits) => {
                        let items: Vec<serde_json::Value> = hits
                            .into_iter()
                            .take(10)
                            .map(|h| {
                                serde_json::json!({
                                    "title": h.title,
                                    "url": h.url,
                                    "engine": h.engine,
                                })
                            })
                            .collect();
                        Ok(ToolOutput::text(
                            serde_json::json!({"results": items}).to_string(),
                        ))
                    }
                    Err(e) => Ok(ToolOutput::text(
                        serde_json::json!({"error": e.to_string()}).to_string(),
                    )),
                }
            })
        },
    );

    let cfg_for_fetch = cfg.clone();
    let unit_for_fetch = *u;
    let reg_for_fetch = registry.clone();
    let fetch_tool = DynamicTool::new(
        ToolName::new("fetch_page").expect("valid name"),
        "Fetch a page and report whether it contains that month's dated events (month_hits).",
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"}
            },
            "required": ["url"]
        }),
        move |args: serde_json::Value| {
            let cfg = cfg_for_fetch.clone();
            // **`Fn` クロージャなので外側の値を move できない。** 呼ばれるたびに clone する。
            let reg = reg_for_fetch.clone();
            let unit = unit_for_fetch;
            Box::pin(async move {
                let url = args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let outcome = crate::fetch::fetch_page(
                    &cfg.fetch.crawl4ai_url,
                    &url,
                    cfg.fetch.interval_ms,
                )
                .await;
                let probe = match outcome {
                    Ok(crate::fetch::FetchOutcome::Ok(doc)) => {
                        probe_page(&unit, &doc.markdown)
                    }
                    Ok(crate::fetch::FetchOutcome::MissingPage { .. }) => PageProbe {
                        status: ProbeStatus::MissingPage,
                        chars: 0,
                        month_hits: 0,
                        head: String::new(),
                    },
                    Ok(crate::fetch::FetchOutcome::EmptyContent { .. }) => PageProbe {
                        status: ProbeStatus::EmptyContent,
                        chars: 0,
                        month_hits: 0,
                        head: String::new(),
                    },
                    Err(e) => {
                        return Ok(ToolOutput::text(
                            serde_json::json!({"error": e.to_string()}).to_string(),
                        ));
                    }
                };
                // 調べた事実を Registry に残す。register_url の可否判定に使う。
                {
                    let mut guard = reg.lock().expect("registry poisoned");
                    guard.record_probe(&url, probe.month_hits);
                }
                Ok(ToolOutput::text(
                    serde_json::to_string(&probe).unwrap_or_else(|_| "{}".to_owned()),
                ))
            })
        },
    );

    let finish = DynamicTool::new(
        ToolName::new("finish").expect("valid name"),
        "Finish collecting URLs.",
        serde_json::json!({
            "type": "object",
            "properties": {"reason": {"type": "string"}},
            "required": ["reason"]
        }),
        |args: serde_json::Value| {
            Box::pin(async move {
                let reason = args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                Ok(ToolOutput::text(
                    serde_json::json!({"done": true, "reason": reason}).to_string(),
                ))
            })
        },
    );

    let client = OllamaConfig::new()
        .with_base_url(&cfg.llm.base_url)
        .with_api_key("ollama")
        .client();
    let model = client.native_completion(&cfg.llm.model);

    let agent = AgentBuilder::new(model)
        .preamble(
            "You collect URLs that contain monthly news. \
             Always register the known candidates first. \
             Only register a URL after fetch_page reports month_hits >= 1. \
             Call finish when you are done.",
        )
        .options(GenerationOptions::default().reasoning(Reasoning::Off))
        .dynamic_tool(register)
        .dynamic_tool(search)
        .dynamic_tool(fetch_tool)
        .dynamic_tool(finish)
        .build();

    // ターンごとにプロンプトを差し替える。会話は引き継がれないので
    // 登録済み一覧を毎回埋め込む。
    let mut warnings: Vec<String> = Vec::new();
    for turn in 0..cfg.discover.max_turns {
        let registered = registry.lock().expect("registry poisoned").urls();
        if registered.len() >= target {
            break;
        }
        let text = prompt_for_turn(turn as u8, &registered, &known_urls, target);
        // **1 プロンプト内で「検索 → 取得 → 登録」を何度も往復させる。**
        // `max_turns` はモデル呼び出し回数の上限（初回を含む）。
        // 3 では検索 1 回・取得 1 回で尽き、毎ターン
        // `reached the max turns limit` になった（実測）。
        // 1 登録あたり 2〜3 呼び出し要るので、余裕を持たせる。
        let per_prompt_turns = (cfg.discover.max_turns * 2).clamp(6, 20);
        match agent.prompt(text).max_turns(per_prompt_turns).await {
            Ok(_) => {}
            Err(e) => {
                // `MaxTurns` は失敗ではない。そこまでの登録内容で確定する。
                warnings.push(format!("agent_turn_{turn}:{e}"));
            }
        }
    }

    let entries = registry.lock().expect("registry poisoned").entries.clone();

    // **静的経路と同じ統合をかける。**
    // エージェントが既知候補と同じ URL を `section: None` で登録すると、
    // 同じページを 2 回取得して LLM を 2 回呼ぶ事故になる
    // （実測: `ja.wikipedia.org/wiki/2007年の日本` が p10 / p20 / p90 の 3 本並んだ）。
    let deterministic: Vec<UrlEntry> = entries.iter().filter(|e| e.priority < 60).cloned().collect();
    let searched: Vec<UrlEntry> = entries.iter().filter(|e| e.priority >= 60).cloned().collect();
    let merged = super::merge_sources(deterministic, searched, cfg.discover.max_search_urls);

    let mut plan = UrlPlan {
        unit: u.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        sources: merged,
    };
    if plan.sources.len() < target {
        warnings.push(format!("below_target:{}<{target}", plan.sources.len()));
    }
    eprintln!("[{u}] agent warnings: {warnings:?}");

    plan.sources.sort_by_key(|s| s.priority);
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_does_not_double_count() {
        let mut reg = Registry::default();
        let entry = UrlEntry {
            url: "https://example.com/a".to_owned(),
            kind: SourceKind::SearchHit,
            priority: 90,
            section: None,
            query: None,
        };
        assert_eq!(reg.push(entry.clone()), 1);
        assert_eq!(reg.push(entry), 1, "同じ URL は加算しない");
    }

    #[test]
    fn prompt_lists_unregistered_known_candidates() {
        let text = prompt_for_turn(
            0,
            &["https://example.com/b".to_owned()],
            &[
                "https://example.com/a".to_owned(),
                "https://example.com/b".to_owned(),
            ],
            12,
        );
        assert!(text.contains("https://example.com/a"));
        assert!(!text.contains("必ず登録すべき既知の候補（まだ未登録なら register_url で登録）:\n- https://example.com/b"));
    }

    #[test]
    fn prompts_differ_per_turn() {
        let a = prompt_for_turn(0, &[], &[], 12);
        let b = prompt_for_turn(1, &[], &[], 12);
        let c = prompt_for_turn(2, &[], &[], 12);
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[test]
    fn kind_from_url_maps_known_hosts() {
        assert_eq!(kind_from_url("https://jiken.jp/x").0, SourceKind::JikenTimeline);
        assert_eq!(kind_from_url("https://ja.wikinews.org/x").0, SourceKind::WikinewsMonth);
        assert_eq!(kind_from_url("https://example.com/x").0, SourceKind::SearchHit);
    }

}
