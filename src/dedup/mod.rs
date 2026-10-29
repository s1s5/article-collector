pub mod normalize;

use crate::config::Config;
use crate::map::NewsItem;

use normalize::{jaccard_bigrams, normalize_key};

/// `docs/*.json` 1 件ぶん。dedup が必要とする最小のフィールドだけ読む。
#[derive(Clone, Debug, serde::Deserialize)]
pub struct DocResult {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub items: Vec<NewsItem>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Cluster {
    pub date: Option<String>,
    pub title: String,
    pub summary: String,
    pub sources: Vec<String>,
    pub members: u16,
    pub best_priority: u8,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DedupStats {
    pub input: usize,
    pub merged: usize,
    pub output: usize,
    pub llm_calls: usize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DedupResult {
    pub unit: String,
    pub clusters: Vec<Cluster>,
    pub stats: DedupStats,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `kind` から優先度を引く（§8.5 の表）。
pub fn priority_of_kind(kind: &str) -> u8 {
    match kind {
        "ja_year_article" | "en_year_article" => 10,
        "en_portal" => 20,
        "nippon_month" => 30,
        "jiken_timeline" => 40,
        "response_month" | "response_day" => 60,
        "wikinews_month" => 70,
        _ => 90,
    }
}

/// 1 項目と、その出所。
#[derive(Clone, Debug)]
struct Entry {
    item: NewsItem,
    url: String,
    priority: u8,
}

/// 段 2 の推移的閉包に使う union-find。
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            let root = self.find(self.parent[x]);
            self.parent[x] = root;
        }
        self.parent[x]
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// フェーズ C。3 段の重複除去。
///
/// 段 1: `(date, normalize_key(title))` の完全一致
/// 段 2: 同一 date 内で `jaccard_bigrams >= cfg.dedup.jaccard`（推移的閉包）
/// 段 3: 段 2 で残り、date が ±2 日以内の組だけ LLM に裁定させる
///
/// 実測（2007-04 ja）: 入力 1,048 件 → 1,031 件。同じ事象が年記事と検索由来の
/// 両方に出るため、段 1 だけで 17 件落ちた。
/// **エラーを返さない。** LLM の裁定が失敗しても warning に残して続行する
/// （重複除去は「できるだけ」であって、失敗して unit 全体を落とす価値は無い）。
pub async fn dedup(unit: &str, docs: &[DocResult], cfg: &Config) -> DedupResult {
    let mut warnings = Vec::new();

    // 0 件の doc は集計に入れない（記録は残す）。
    for doc in docs {
        if doc.items.is_empty() {
            warnings.push(format!("empty_doc:{}", doc.url));
        }
    }

    let entries: Vec<Entry> = docs
        .iter()
        .filter(|d| !d.items.is_empty())
        .flat_map(|doc| {
            let priority = priority_of_kind(&doc.kind);
            doc.items.iter().map(move |item| Entry {
                item: item.clone(),
                url: doc.url.clone(),
                priority,
            })
        })
        .collect();

    let input = entries.len();
    if input == 0 {
        return DedupResult {
            unit: unit.to_owned(),
            clusters: Vec::new(),
            stats: DedupStats::default(),
            warnings,
        };
    }

    // --- 段 1: 完全一致 ---
    let mut uf = UnionFind::new(input);
    let mut by_key: std::collections::HashMap<(Option<String>, String), usize> =
        std::collections::HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let key = (
            entry.item.date.clone(),
            normalize_key(&entry.item.title),
        );
        match by_key.get(&key) {
            Some(&j) => uf.union(j, i),
            None => {
                by_key.insert(key, i);
            }
        }
    }

    // --- 段 2: 同一 date 内の Jaccard ---
    let jaccard = f64::from(cfg.dedup.jaccard) as f32;
    for i in 0..input {
        for j in (i + 1)..input {
            if entries[i].item.date != entries[j].item.date {
                continue;
            }
            if uf.find(i) == uf.find(j) {
                continue;
            }
            let sim = jaccard_bigrams(&entries[i].item.title, &entries[j].item.title);
            if sim >= jaccard {
                uf.union(i, j);
            }
        }
    }

    // --- 段 3: 残った組を LLM に裁定させる（date が ±2 日以内） ---
    let mut llm_calls = 0usize;
    let groups = group_roots(&mut uf, input);
    let mut root_of: Vec<usize> = vec![0; input];
    for (root, members) in &groups {
        for &m in members {
            root_of[m] = *root;
        }
    }

    // 代表（priority 最小）が異なるクラスタ同士で、日付が近い組を集める。
    //
    // **ただしタイトルに重なりがある組だけ。** 日付が近いだけの全組み合わせを
    // 取ると、687 件・同一日に 200 件が集中する月では候補が 8,000 組を超え、
    // バッチ上限に張り付いて大半が未裁定のまま捨てられた（実測）。
    // 段 2 の閾値（0.6）より低い 0.25 を下限にして「言い換えの可能性がある組」
    // だけを LLM に回す。これで段 3 は本当に曖昧な組だけになる。
    const PRE_JUDGE_SIMILARITY: f32 = 0.25;
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..input {
        for j in (i + 1)..input {
            if root_of[i] == root_of[j] {
                continue;
            }
            if !dates_within(&entries[i].item.date, &entries[j].item.date, 2) {
                continue;
            }
            if jaccard_bigrams(&entries[i].item.title, &entries[j].item.title)
                < PRE_JUDGE_SIMILARITY
            {
                continue;
            }
            candidates.push((i, j));
        }
    }

    if !candidates.is_empty() {
        // **1 回のプロンプトに全ペアを入れない。**
        // 実測 2007-04 ja: 候補ペアを全部入れたら 4,164,661 トークンを要求し、
        // `exceed_context_size_error`（400）で落ちた。
        // バッチに割り、1 バッチあたりの文字数とペア数の両方で抑える。
        const MAX_PAIRS_PER_BATCH: usize = 40;
        const MAX_CHARS_PER_BATCH: usize = 12_000;
        // 実測 2007-04 ja: 候補ペアが 40 バッチを超え、judge_batches_capped で
        // 多くのペアが未裁定のまま捨てられた。上限を上げる。
        const MAX_BATCHES: usize = 200;

        // バッチに割る。文字数とペア数の両方で抑える。
        let mut pending: Vec<Vec<(usize, usize)>> = Vec::new();
        let mut current: Vec<(usize, usize)> = Vec::new();
        let mut current_chars = 0usize;

        for pair in &candidates {
            let size = entry_pair_chars(&entries, *pair);
            let would_overflow = !current.is_empty()
                && (current.len() >= MAX_PAIRS_PER_BATCH
                    || current_chars + size > MAX_CHARS_PER_BATCH);
            if would_overflow {
                pending.push(std::mem::take(&mut current));
                current_chars = 0;
                if pending.len() >= MAX_BATCHES {
                    break;
                }
            }
            current.push(*pair);
            current_chars += size;
        }
        if !current.is_empty() && pending.len() < MAX_BATCHES {
            pending.push(current);
        }
        if pending.len() >= MAX_BATCHES {
            warnings.push(format!("judge_batches_capped:{}", pending.len()));
        }
        llm_calls += pending.len();

        for batch in pending {
            match judge_same_event(cfg, &entries, &batch).await {
                Ok(verdicts) => {
                    for ((i, j), same) in batch.iter().zip(verdicts) {
                        if same {
                            let ri = uf.find(*i);
                            let rj = uf.find(*j);
                            uf.union(ri, rj);
                        }
                    }
                }
                Err(e) => {
                    warnings.push(format!("llm_judge_failed:{e}"));
                }
            }
        }
    }

    // --- クラスタを組み立てる ---
    let final_groups = group_roots(&mut uf, input);
    let mut clusters: Vec<Cluster> = Vec::with_capacity(final_groups.len());
    for (_, members) in final_groups {
        // 代表は priority 最小、同点なら入力順が先のもの。
        let best = members
            .iter()
            .copied()
            .min_by_key(|&i| (entries[i].priority, i))
            .expect("non-empty group");
        let mut sources: Vec<String> = Vec::new();
        for &m in &members {
            if !sources.contains(&entries[m].url) {
                sources.push(entries[m].url.clone());
            }
        }
        clusters.push(Cluster {
            date: entries[best].item.date.clone(),
            title: entries[best].item.title.clone(),
            summary: entries[best].item.summary.clone(),
            sources,
            members: members.len() as u16,
            best_priority: entries[best].priority,
        });
    }

    // 決定的な順序: date 昇順（null は末尾）、同日内は priority → title。
    clusters.sort_by(|a, b| match (&a.date, &b.date) {
        (Some(x), Some(y)) => x.cmp(y).then_with(|| {
            a.best_priority
                .cmp(&b.best_priority)
                .then_with(|| a.title.cmp(&b.title))
        }),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a
            .best_priority
            .cmp(&b.best_priority)
            .then_with(|| a.title.cmp(&b.title)),
    });

    let output = clusters.len();
    DedupResult {
        unit: unit.to_owned(),
        clusters,
        stats: DedupStats {
            input,
            merged: input.saturating_sub(output),
            output,
            llm_calls,
        },
        warnings,
    }
}

fn group_roots(uf: &mut UnionFind, n: usize) -> Vec<(usize, Vec<usize>)> {
    let mut map: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for i in 0..n {
        let root = uf.find(i);
        map.entry(root).or_default().push(i);
    }
    map.into_iter().collect()
}

/// `YYYY-MM-DD` の差が `days` 以内か。どちらかが null なら false。
fn dates_within(a: &Option<String>, b: &Option<String>, days: i64) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    let (Ok(a), Ok(b)) = (
        chrono::NaiveDate::parse_from_str(a, "%Y-%m-%d"),
        chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d"),
    ) else {
        return false;
    };
    (a - b).num_days().abs() <= days
}


/// 1 ペアがプロンプトで占めるおおよその文字数。
fn entry_pair_chars(entries: &[Entry], (i, j): (usize, usize)) -> usize {
    let a = &entries[i].item;
    let b = &entries[j].item;
    a.title.chars().count()
        + a.summary.chars().count()
        + b.title.chars().count()
        + b.summary.chars().count()
        + 40 // 日付・ラベル・改行のぶん
}


/// 裁定結果の配列を読む。**モデルは配列以外の形も返す。**
///
/// 実測: `{"0": false, "1": true, ...}` という数値キーのオブジェクトを
/// 返されたとき、`Vec<bool>` へのパースが失敗して全バッチが落ちた。
/// 両方を受ける。
fn parse_verdicts(text: &str) -> Result<Vec<bool>, String> {
    if let Ok(v) = serde_json::from_str::<Vec<bool>>(text) {
        return Ok(v);
    }
    if let Ok(map) = serde_json::from_str::<std::collections::BTreeMap<String, bool>>(text) {
        let mut keys: Vec<usize> = Vec::with_capacity(map.len());
        for k in map.keys() {
            keys.push(k.parse::<usize>().map_err(|_| format!("non-numeric key `{k}`"))?);
        }
        keys.sort_unstable();
        return Ok(keys.into_iter().map(|k| map[&k.to_string()]).collect());
    }
    Err("neither array nor numeric-key object".to_owned())
}

/// 同一事象かを LLM に聞く。**候補の組だけ**を渡す。
///
/// 実測: 全件を投げると `llm_calls == input` になり、コストが跳ねる。
/// 段 1・2 で大半が落ちるので、ここに来るのは残りだけ。
async fn judge_same_event(
    cfg: &Config,
    entries: &[Entry],
    pairs: &[(usize, usize)],
) -> Result<Vec<bool>, String> {
    use rig_core::completion::message::AssistantContent;
    use rig_core::completion::request::ToolDefinition;
    use rig_core::completion::{CompletionRequest, Reasoning};
    use rig_core::message::ToolName;

    let mut listing = String::new();
    for (n, (i, j)) in pairs.iter().enumerate() {
        let a = &entries[*i].item;
        let b = &entries[*j].item;
        listing.push_str(&format!(
            "{n}. A: [{}] {} / {}\n   B: [{}] {} / {}\n",
            a.date.as_deref().unwrap_or("null"),
            a.title,
            a.summary,
            b.date.as_deref().unwrap_or("null"),
            b.title,
            b.summary
        ));
    }

    // **テキストで JSON 配列を書かせない。** 実測で 40 バッチ中 39 が
    // 「expected value at line 1 column 1」で落ちた（説明文を返していた）。
    // 抽出と同じくツール呼び出しで受け取る。
    const JUDGE_TOOL: &str = "submit_verdicts";
    let tool = ToolDefinition::new(
        ToolName::new(JUDGE_TOOL).expect("valid tool name"),
        "Submit one boolean per pair, in order. true = same event.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "verdicts": {
                    "type": "array",
                    "items": { "type": "boolean" }
                }
            },
            "required": ["verdicts"]
        }),
    );

    let preamble = "\
You judge whether two news items describe the SAME event.
Same event = same actors, same action, same day (or near-same day).
Different wording of the same fact is the same event.
Submit one boolean per pair, in the given order, by calling `submit_verdicts` exactly once.
Do not write prose. Do not write JSON in the body.";

    let client = rig_core::providers::ollama::OllamaConfig::new()
        .with_base_url(&cfg.llm.base_url)
        .with_api_key("ollama")
        .client();
    let model = client.native_completion(&cfg.llm.model);

    let mut request = CompletionRequest::new(listing)
        .preamble(preamble)
        .temperature(0.0)
        .max_tokens(4096)
        .tool(tool)
        .additional_params(serde_json::json!({
            "keep_alive": cfg.llm.keep_alive,
            "options": { "num_ctx": cfg.llm.num_ctx }
        }));
    if !cfg.llm.think {
        request = request.reasoning(Reasoning::Off);
    }

    let resp = model.call(request).await.map_err(|e| e.to_string())?;

    // ツール呼び出しの引数から読む。無ければ本文の配列を試す。
    let from_tool = resp.choice.iter().find_map(|c| match c {
        AssistantContent::ToolCall(call) if call.function.name.as_str() == JUDGE_TOOL => {
            call.function
                .arguments
                .get("verdicts")
                .and_then(|v| serde_json::from_value::<Vec<bool>>(v.clone()).ok())
        }
        _ => None,
    });

    let verdicts = match from_tool {
        Some(v) => v,
        None => {
            let text = resp
                .choice
                .iter()
                .find_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            let body = text.trim();
            let slice = match (body.find('['), body.rfind(']')) {
                (Some(a), Some(b)) if b > a => &body[a..=b],
                _ => body,
            };
            parse_verdicts(slice).map_err(|e| {
                format!(
                    "judge parse: {e} (text: {})",
                    &body.chars().take(120).collect::<String>()
                )
            })?
        }
    };

    // **判定数の不一致を許容する。** 実測でモデルが 40 ペアに対し 39 件しか
    // 返すことがあった。不足分は false（統合しない）に倒す。重複除去は
    // 「できるだけ」であり、取りこぼしより誤統合の方が害が大きい。
    let mut verdicts = verdicts;
    if verdicts.len() > pairs.len() {
        verdicts.truncate(pairs.len());
    }
    while verdicts.len() < pairs.len() {
        verdicts.push(false);
    }
    Ok(verdicts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(date: &str, title: &str) -> NewsItem {
        NewsItem {
            date: Some(date.to_owned()),
            title: title.to_owned(),
            summary: "s".to_owned(),
        }
    }

    fn doc(url: &str, kind: &str, items: Vec<NewsItem>) -> DocResult {
        DocResult {
            url: url.to_owned(),
            kind: kind.to_owned(),
            items,
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    #[tokio::test]
    async fn merges_exact_duplicates_across_docs() {
        let docs = vec![
            doc(
                "https://a",
                "ja_year_article",
                vec![item("2007-04-01", "熊本城築城400年祭開幕")],
            ),
            doc(
                "https://b",
                "search_hit",
                vec![item("2007-04-01", "熊本城築城400年祭開幕")],
            ),
        ];
        let r = dedup("2007-04/ja", &docs, &cfg()).await;
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].members, 2);
        assert_eq!(r.clusters[0].best_priority, 10, "年記事を代表にする");
        assert_eq!(r.stats.merged, 1);
    }

    #[tokio::test]
    async fn merges_paraphrase_via_jaccard() {
        let docs = vec![doc(
            "https://a",
            "ja_year_article",
            vec![
                item("2007-04-01", "熊本城築城400年祭が開幕"),
                item("2007-04-01", "熊本城築城400年祭が開幕した"),
            ],
        )];
        let r = dedup("2007-04/ja", &docs, &cfg()).await;
        assert_eq!(r.clusters.len(), 1);
        assert_eq!(r.clusters[0].members, 2);
    }

    #[tokio::test]
    async fn keeps_distinct_events_on_same_day() {
        let docs = vec![doc(
            "https://a",
            "ja_year_article",
            vec![
                item("2007-04-01", "熊本城築城400年祭が開幕"),
                item("2007-04-01", "全く別の交通事故が起きた"),
            ],
        )];
        let r = dedup("2007-04/ja", &docs, &cfg()).await;
        assert_eq!(r.clusters.len(), 2);
    }

    #[tokio::test]
    async fn keeps_distinct_dates_apart() {
        let docs = vec![doc(
            "https://a",
            "ja_year_article",
            vec![
                item("2007-04-01", "同じ題名の出来事"),
                item("2007-04-20", "同じ題名の出来事"),
            ],
        )];
        let r = dedup("2007-04/ja", &docs, &cfg()).await;
        assert_eq!(r.clusters.len(), 2);
    }

    #[tokio::test]
    async fn warns_about_empty_docs() {
        let docs = vec![doc("https://empty", "search_hit", vec![])];
        let r = dedup("2007-04/ja", &docs, &cfg()).await;
        assert!(r.clusters.is_empty());
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn priority_table_matches_design() {
        assert_eq!(priority_of_kind("ja_year_article"), 10);
        assert_eq!(priority_of_kind("en_portal"), 20);
        assert_eq!(priority_of_kind("jiken_timeline"), 40);
        assert_eq!(priority_of_kind("search_hit"), 90);
        assert_eq!(priority_of_kind("unknown"), 90);
    }

    #[test]
    fn dates_within_handles_nulls_and_ranges() {
        let d = |s: &str| Some(s.to_owned());
        assert!(dates_within(&d("2007-04-01"), &d("2007-04-03"), 2));
        assert!(!dates_within(&d("2007-04-01"), &d("2007-04-04"), 2));
        assert!(!dates_within(&None, &d("2007-04-01"), 2));
    }
}
