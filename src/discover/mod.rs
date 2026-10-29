pub mod agent;
pub mod en;
pub mod ja;
pub mod search;

use crate::config::Config;
use crate::unit::UnitKey;

/// 月見出しをどのセクションから取るか。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    /// `## できごと`
    JaKigoto,
    /// `## 予定・予測`
    JaYotei,
    /// `## Events`（英語の年記事）
    EnEvents,
    /// セクション指定なし（ページ全体）
    Whole,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    JaYearArticle,
    EnYearArticle,
    EnPortal,
    NipponMonth,
    JikenTimeline,
    ResponseMonth,
    ResponseDay,
    WikinewsMonth,
    SearchHit,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UrlEntry {
    pub url: String,
    pub kind: SourceKind,
    pub priority: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<Section>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UrlPlan {
    pub unit: String,
    pub generated_at: String,
    pub sources: Vec<UrlEntry>,
}

#[derive(Debug)]
pub enum DiscoverError {
    /// そのユニットに候補が 1 つも無い。
    Nothing,
    Io(String),
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nothing => write!(f, "no source candidates for this unit"),
            Self::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// フェーズ A（決定的ソース **＋ 検索**）。
///
/// **検索は補完ではなく主経路。** 決定的ソースと検索を常に両方走らせ、
/// `normalize_url` で重複を落として `priority` 昇順に統合する。
/// 実測: ja 年記事の月セクションは 12〜17 件しか出口が無く、下限 30 には
/// 検索由来のソースが要る（§8.6）。
///
/// 検索が全滅してもエラーにしない。決定的ソースは残るため。
/// フェーズ A の何回目か。S11 の `DiscoverMode { Static, Agent }` とは別の型。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum DiscoverPass {
    Initial,
    /// 件数不足での再入。**unit あたり 1 回だけ**。
    Expand,
}

/// フェーズ A の実行方式。S9 の `DiscoverPass` とは**別の型**。
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum DiscoverMode {
    /// 決定的ソース＋固定クエリの検索。**既定**。LLM を使わない。
    #[default]
    Static,
    /// rig-agent がツール経由で URL を探す。LLM を使う。
    Agent,
}

impl DiscoverMode {
    /// 設定文字列を解釈する。未知の値は `Static`（安全側）。
    pub fn parse(raw: &str) -> Self {
        match raw {
            "agent" => Self::Agent,
            _ => Self::Static,
        }
    }
}

pub async fn discover(u: &UnitKey, cfg: &Config) -> Result<UrlPlan, DiscoverError> {
    discover_with(u, cfg, DiscoverPass::Initial).await
}

/// `pass` を指定してフェーズ A を回す。
///
/// `DiscoverPass::Expand` では検索クエリを 2 本追加する。実測 en 2000 の
/// ように年記事に事件が 2 件しか無い月があるため、1 回だけ広げ直せるようにする。
pub async fn discover_with(
    u: &UnitKey,
    cfg: &Config,
    pass: DiscoverPass,
) -> Result<UrlPlan, DiscoverError> {
    // 既定は static。agent のときだけエージェント経路に切り替える。
    if DiscoverMode::parse(&cfg.discover.mode) == DiscoverMode::Agent {
        return agent::agent_discover(u, cfg).await;
    }

    let deterministic = match u.lang {
        crate::unit::Lang::Ja => ja::ja_sources(u),
        crate::unit::Lang::En => en::en_sources(u),
    };

    // 系統 2: 検索を常時走らせる。Expand ではクエリを追加する。
    let searched = search::search_urls_with(u, cfg, pass).await;
    eprintln!(
        "[{u}] search yielded {} url(s) ({pass:?})",
        searched.len()
    );

    let sources = merge_sources(deterministic, searched, cfg.discover.max_search_urls);
    if sources.is_empty() {
        return Err(DiscoverError::Nothing);
    }

    Ok(UrlPlan {
        unit: u.to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        sources,
    })
}

/// 決定的ソースと検索結果を統合する。**ネットワークを触らない純粋関数。**
///
/// - `priority` 昇順（小さいほど優先）で安定に並べる
/// - 決定的ソースが既に指している URL は、検索由来のエントリを**丸ごと落とす**
///   （`section` が違っても同じページを 2 回叩いて LLM を 2 回呼ぶ事故になる。
///   実測 2007-04 で 101 件を 2 回生成した）
/// - 残りは `(URL, section)` で重複除去
fn merge_sources(
    deterministic: Vec<UrlEntry>,
    searched: Vec<UrlEntry>,
    max_search_urls: usize,
) -> Vec<UrlEntry> {
    let deterministic_urls: std::collections::HashSet<String> = deterministic
        .iter()
        .map(|s| normalize_url(&s.url))
        .collect();

    let mut all = deterministic;
    let mut search_added = 0usize;
    for entry in searched {
        // 決定的ソースが同じページを既に取るなら、検索由来は不要。
        if deterministic_urls.contains(&normalize_url(&entry.url)) {
            continue;
        }
        all.push(entry);
        search_added += 1;
    }
    let _ = search_added;

    all.sort_by_key(|s| s.priority);
    let mut seen = std::collections::HashSet::new();
    all.retain(|source| {
        let key = (normalize_url(&source.url), format!("{:?}", source.section));
        seen.insert(key)
    });

    // 検索由来は priority 順に上位 `max_search_urls` 本だけ残す。
    // 実測 2007-04: IT 系ニュース索引が 283 件・226 件を吐き、
    // 1 ユニット 36 本で 462 秒かかった。上限を切ると実行時間も件数も収まる。
    let mut kept_search = 0usize;
    all.retain(|source| {
        if source.priority < 60 {
            return true;
        }
        kept_search += 1;
        kept_search <= max_search_urls
    });
    all
}

/// URL を比較用に正規化する。
///
/// - スキームとホストを小文字化
/// - fragment を落とす
/// - `utm_*` クエリを落とす
/// - 末尾スラッシュを 1 つに畳む
pub fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let (without_fragment, _) = match trimmed.split_once('#') {
        Some((head, frag)) => (head, Some(frag)),
        None => (trimmed, None),
    };
    let (base, query) = match without_fragment.split_once('?') {
        Some((base, q)) => (base, Some(q)),
        None => (without_fragment, None),
    };

    let mut out = String::with_capacity(trimmed.len());
    // スキーム小文字化
    if let Some((scheme, rest)) = base.split_once("://") {
        out.push_str(&scheme.to_lowercase());
        out.push_str("://");
        // ホスト小文字化（パスは保持）
        if let Some(slash) = rest.find('/') {
            out.push_str(&rest[..slash].to_lowercase());
            out.push_str(&rest[slash..]);
        } else {
            out.push_str(&rest.to_lowercase());
        }
    } else {
        out.push_str(base);
    }

    // 末尾スラッシュはクエリを付ける前に落とす。
    // 後で落とすと `.../Path/?a=1` のようにクエリの後ろに残ったままになる。
    while out.ends_with('/') && !out.ends_with("://") {
        out.pop();
    }

    if let Some(q) = query {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| !pair.is_empty() && !pair.starts_with("utm_"))
            .collect();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::DiscoverMode;

    #[test]
    fn mode_parses_known_values() {
        assert_eq!(DiscoverMode::parse("agent"), DiscoverMode::Agent);
        assert_eq!(DiscoverMode::parse("static"), DiscoverMode::Static);
        assert_eq!(DiscoverMode::parse("nonsense"), DiscoverMode::Static);
        assert_eq!(DiscoverMode::default(), DiscoverMode::Static);
    }


    use super::*;
    use crate::unit::Lang;

    fn key(year: u16, month: u8, lang: Lang) -> UnitKey {
        UnitKey { year, month, lang }
    }

    #[test]
    fn normalize_drops_fragment_and_utm() {
        assert_eq!(
            normalize_url("HTTPS://Example.COM/Path/?utm_source=x&a=1#frag"),
            "https://example.com/Path?a=1"
        );
    }

    #[test]
    fn normalize_keeps_meaningful_query() {
        assert_eq!(
            normalize_url("https://example.com/s?q=1"),
            "https://example.com/s?q=1"
        );
    }

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(
            normalize_url("https://example.com/a//"),
            "https://example.com/a"
        );
    }

    fn entry(url: &str, kind: SourceKind, priority: u8, section: Option<Section>) -> UrlEntry {
        UrlEntry {
            url: url.to_owned(),
            kind,
            priority,
            section,
            query: None,
        }
    }

    #[test]
    fn merge_keeps_deterministic_entries() {
        let det = ja::ja_sources(&key(2007, 1, Lang::Ja));
        let merged = merge_sources(det, Vec::new(), 8);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].priority, 10);
        assert_eq!(merged[1].priority, 20);
    }

    #[test]
    fn merge_adds_search_hits_after_deterministic() {
        let det = en::en_sources(&key(2007, 4, Lang::En));
        let searched = vec![entry(
            "https://example.com/x",
            SourceKind::SearchHit,
            90,
            None,
        )];
        let merged = merge_sources(det, searched, 8);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].priority, 10, "決定的ソースが先");
        assert_eq!(merged[1].priority, 90);
    }

    #[test]
    fn merge_drops_duplicate_url_and_section() {
        let det = ja::ja_sources(&key(2007, 1, Lang::Ja));
        // 同じ URL・同じセクションを検索側が返しても 1 本に。
        let dup = det[0].clone();
        let merged = merge_sources(det, vec![dup], 8);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_keeps_same_url_with_different_section() {
        let det = ja::ja_sources(&key(2007, 1, Lang::Ja));
        // 同じ URL でもセクション違いは別エントリ（できごと / 予定・予測）。
        let merged = merge_sources(det, Vec::new(), 8);
        assert_eq!(merged.len(), 2);
        assert_ne!(merged[0].section, merged[1].section);
    }

    #[test]
    fn merge_drops_search_hit_that_duplicates_a_deterministic_url() {
        // 実測 2007-04 の事故: 同じ Wikipedia ページが section 違いで 2 エントリ残り、
        // 101 件を 2 回生成した。決定的ソースが指す URL は検索由来を丸ごと落とす。
        let det = ja::ja_sources(&key(2007, 4, Lang::Ja));
        let dup_url = det[0].url.clone();
        let searched = vec![entry(&dup_url, SourceKind::SearchHit, 90, None)];
        let merged = merge_sources(det, searched, 8);
        assert_eq!(merged.len(), 2, "できごと と 予定・予測 だけが残る");
        assert!(merged.iter().all(|s| s.kind == SourceKind::JaYearArticle));
    }

    #[test]
    fn merge_prefers_lower_priority_on_collision() {
        let det = vec![entry(
            "https://example.com/a",
            SourceKind::JaYearArticle,
            10,
            None,
        )];
        let searched = vec![entry(
            "https://example.com/a",
            SourceKind::SearchHit,
            90,
            None,
        )];
        let merged = merge_sources(det, searched, 8);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].kind, SourceKind::JaYearArticle);
    }

    #[test]
    fn merge_normalizes_before_comparing() {
        let det = vec![entry(
            "https://example.com/a",
            SourceKind::JaYearArticle,
            10,
            None,
        )];
        let searched = vec![entry(
            "HTTPS://Example.com/a/?utm_source=x",
            SourceKind::SearchHit,
            90,
            None,
        )];
        let merged = merge_sources(det, searched, 8);
        assert_eq!(merged.len(), 1, "正規化して同一とみなす");
    }
}
