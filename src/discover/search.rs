use std::time::Duration;

use super::{normalize_url, SourceKind, UrlEntry};
use crate::config::Config;
use crate::unit::{Lang, UnitKey};

#[derive(Clone, Debug, serde::Deserialize)]
pub struct SearchHit {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub content: String,
}

/// SearXNG の応答。`results` と `unresponsive_engines` だけ見る。
#[derive(Clone, Debug, serde::Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchHit>,
    #[serde(default)]
    unresponsive_engines: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub enum SearchError {
    /// HTTP・JSON の失敗。呼び出し側はこのクエリを諦める。
    Transport(String),
    Http(u16),
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "search transport: {m}"),
            Self::Http(s) => write!(f, "search http {s}"),
        }
    }
}

impl std::error::Error for SearchError {}

const MONTHS_EN: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// 言語別の固定クエリセット（§8.3）。
///
/// 実測 2007-04 ja: 21 / 11 / 7 / 7 件。en: 10 / 10 件。
/// 検索は LLM 0 回なので、常時走らせても金銭コストは無い。
pub fn queries_for(u: &UnitKey) -> Vec<String> {
    match u.lang {
        Lang::Ja => vec![
            format!("{}年{}月 出来事 事件 事故", u.year, u.month),
            format!("{}年{}月 日本 ニュース", u.year, u.month),
            format!("{}年{}月 国際 情勢", u.year, u.month),
            format!("{}年{}月 経済 企業", u.year, u.month),
        ],
        Lang::En => {
            let m = MONTHS_EN[(u.month.clamp(1, 12) - 1) as usize];
            vec![
                format!("{m} {} world news events", u.year),
                format!("{m} {} news headlines", u.year),
                format!("{m} {} politics economy", u.year),
            ]
        }
    }
}

/// 件数不足での再入（Expand）用に**追加する**クエリ。
///
/// 既存の 4/3 本に足す形にする。置き換えると初回の成果が消える。
pub fn expand_queries_for(u: &UnitKey) -> Vec<String> {
    match u.lang {
        Lang::Ja => vec![
            format!("{}年{}月 主な出来事", u.year, u.month),
            format!("{}年{}月 話題", u.year, u.month),
        ],
        Lang::En => {
            let m = MONTHS_EN[(u.month.clamp(1, 12) - 1) as usize];
            vec![
                format!("{m} {} archive", u.year),
                format!("{m} {} major events", u.year),
            ]
        }
    }
}

fn lang_code(lang: Lang) -> &'static str {
    match lang {
        Lang::Ja => "ja",
        Lang::En => "en",
    }
}

/// 1 クエリを SearXNG に投げる。0 件かつ `unresponsive_engines` があれば 1 回だけ再試行。
pub async fn search(
    cfg: &Config,
    query: &str,
    lang: Lang,
) -> Result<Vec<SearchHit>, SearchError> {
    let base = cfg.search.base_url.trim_end_matches('/');
    let url = format!(
        "{base}/search?q={}&format=json&language={}&pageno={}",
        urlencode(query),
        lang_code(lang),
        cfg.search.pageno
    );

    let mut last_err: Option<SearchError> = None;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(cfg.fetch.interval_ms)).await;
        }
        match fetch_once(&url).await {
            Ok(resp) => {
                if resp.results.is_empty() && !resp.unresponsive_engines.is_empty() {
                    // 全エンジンが落ちた可能性。1 回だけやり直す。
                    last_err = Some(SearchError::Transport("all engines unresponsive".to_owned()));
                    continue;
                }
                return Ok(resp.results);
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| SearchError::Transport("no attempt".to_owned())))
}

async fn fetch_once(url: &str) -> Result<SearchResponse, SearchError> {
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(40))
        .send()
        .await
        .map_err(|e| SearchError::Transport(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(SearchError::Http(status.as_u16()));
    }
    resp.json::<SearchResponse>()
        .await
        .map_err(|e| SearchError::Transport(e.to_string()))
}

fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 検索结果の URL を `SourceKind` に振り分ける（§8.5 の優先度表）。
///
/// **ただし他の言語のサイトは受け付けない。** 実測 2007-04 ja で
/// `zh.wikipedia.org` と `baike.baidu.com` から 152 件の中国語ニュースが
/// 混入し、日本語の月次ダイジェストが中国語のタイトルで埋まった。
/// 検索ヒットが対象言語のものかを、**文字種**で判定する。
///
/// ドメインのブロックリスト方式では漏れ続けた（実測 2007-02 ja に
/// `ja.namu.wiki`（韓国語）・`wsj.com`・`nbcnews.com`・`onthisday.com` が混入）。
/// ドメインは無限にあるので列挙できない。**本文の文字種を見るのが確実**。
///
/// - ja を求めるなら、ひらがなかカタカナを含むこと（日本語の記事は必ず含む）
/// - en を求めるなら、かな・漢字をほとんど含まないこと
fn language_matches(hit: &SearchHit, lang: Lang) -> bool {
    let text = format!("{} {}", hit.title, hit.content);
    let total = text.chars().filter(|c| !c.is_whitespace()).count().max(1);

    // **漢字では判定できない。** 日本語と中国語で共通のため、
    // `2007年4月` という中国語タイトルが「漢字が多い＝日本語」と誤判定される
    // （実測でテストが落ちた）。
    // 決め手は**かな文字**。日本語の記事はひらがなかカタカナを必ず含み、
    // 中国語・韓国語・英語は含まない。
    let kana = text
        .chars()
        .filter(|c| matches!(c, '\u{3040}'..='\u{30ff}'))
        .count();
    let han = text
        .chars()
        .filter(|c| matches!(c, '\u{4e00}'..='\u{9fff}'))
        .count();
    let latin = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let hangul = text
        .chars()
        .filter(|c| matches!(c, '\u{ac00}'..='\u{d7af}'))
        .count();

    match lang {
        Lang::Ja => {
            // かなを含み、かつかな＋漢字が 2 割以上（記号ばかりの行を弾く）。
            kana > 0 && (kana + han) * 10 >= total * 2
        }
        Lang::En => {
            // かな・漢字・ハングルをほとんど含まず、ラテン文字が主。
            let non_latin = kana + han + hangul;
            non_latin * 10 < total && latin * 2 >= total
        }
    }
}

fn classify_url(url: &str, _lang: Lang, with_portal: bool) -> Option<(SourceKind, u8)> {
    let lower = url.to_lowercase();

    if lower.contains("jiken.jp") {
        return Some((SourceKind::JikenTimeline, 40));
    }
    if lower.contains("response.jp/article/") {
        // /article/YYYY/MM/DD/ まで深ければ日別、/article/YYYY/MM/ なら月次。
        let depth = lower
            .split("/article/")
            .nth(1)
            .map(|rest| rest.trim_end_matches('/').split('/').count())
            .unwrap_or(0);
        return Some(if depth >= 3 {
            (SourceKind::ResponseDay, 60)
        } else {
            (SourceKind::ResponseMonth, 60)
        });
    }
    // Portal は一次ソースに近いので priority 20 に上げる（年記事に次ぐ）。
    if lower.contains("portal:current_events") {
        // Portal は既定で外す（実測 2007: 全走時間の主要因）。
        return with_portal.then_some((SourceKind::EnPortal, 20));
    }
    if lower.contains("wikinews") {
        return Some((SourceKind::WikinewsMonth, 70));
    }
    if lower.contains("nippon.com") {
        return Some((SourceKind::NipponMonth, 30));
    }
    Some((SourceKind::SearchHit, 90))
}

/// 検索を常時走らせ、`UrlEntry` の列にする。
///
/// **失敗したクエリは飛ばす。** 検索が全滅しても決定的ソースは残るので、
/// ここでエラーを返してフェーズ A 全体を落とす必要はない。
/// `pass` に応じてクエリを足して検索する。
///
/// `Expand` では `expand_queries_for` の 2 本を追加し、
/// 1 クエリあたりの取得件数を 10 → 20 に広げる。
pub async fn search_urls_with(
    u: &UnitKey,
    cfg: &Config,
    pass: super::DiscoverPass,
) -> Vec<UrlEntry> {
    let per_query = match pass {
        super::DiscoverPass::Initial => 10,
        super::DiscoverPass::Expand => 20,
    };

    let mut queries = queries_for(u);
    if pass == super::DiscoverPass::Expand {
        queries.extend(expand_queries_for(u));
    }

    let mut out = Vec::new();
    for query in queries {
        match search(cfg, &query, u.lang).await {
            Ok(hits) => {
                eprintln!("[{u}] search `{query}` -> {} hit(s)", hits.len());
                for hit in hits.into_iter().take(per_query) {
                    if hit.url.is_empty() {
                        continue;
                    }
                    // スニペットが空のヒットは本文を持たない可能性が高いので記録だけ残す。
                    if hit.content.trim().is_empty() {
                        eprintln!("  (no snippet) [{}] {}", hit.engine, hit.title);
                    }
                    if !language_matches(&hit, u.lang) {
                        eprintln!("  (wrong language) {}", hit.url);
                        continue;
                    }
                    let Some((kind, priority)) = classify_url(&hit.url, u.lang, cfg.discover.with_portal) else {
                        eprintln!("  (rejected) {}", hit.url);
                        continue;
                    };
                    out.push(UrlEntry {
                        url: normalize_url(&hit.url),
                        kind,
                        priority,
                        section: None,
                        query: Some(query.clone()),
                    });
                }
            }
            Err(e) => {
                eprintln!("[{u}] search `{query}` failed: {e}");
            }
        }
        tokio::time::sleep(Duration::from_millis(cfg.fetch.interval_ms)).await;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(year: u16, month: u8, lang: Lang) -> UnitKey {
        UnitKey { year, month, lang }
    }

    #[test]
    fn ja_queries_are_four_and_contain_year_month() {
        let qs = queries_for(&key(2007, 4, Lang::Ja));
        assert_eq!(qs.len(), 4);
        assert!(qs.iter().all(|q| q.contains("2007年4月")));
        assert!(qs[0].contains("出来事"));
    }

    #[test]
    fn en_queries_are_three_and_use_month_name() {
        let qs = queries_for(&key(2007, 4, Lang::En));
        assert_eq!(qs.len(), 3);
        assert!(qs.iter().all(|q| q.contains("April 2007")));
    }

    #[test]
    fn classifies_known_hosts() {
        assert_eq!(
            classify_url("https://jiken.jp/timeline/heisei/2007/", Lang::Ja, false),
            Some((SourceKind::JikenTimeline, 40))
        );
        assert_eq!(
            classify_url("https://response.jp/article/2007/04/01/", Lang::Ja, false),
            Some((SourceKind::ResponseDay, 60))
        );
        assert_eq!(
            classify_url("https://response.jp/article/2007/04/", Lang::Ja, false),
            Some((SourceKind::ResponseMonth, 60))
        );
        assert_eq!(
            classify_url("https://ja.wikinews.org/wiki/x", Lang::Ja, false),
            Some((SourceKind::WikinewsMonth, 70))
        );
        assert_eq!(
            classify_url("https://www.nippon.com/ja/news/q202609/", Lang::Ja, false),
            Some((SourceKind::NipponMonth, 30))
        );
        assert_eq!(
            classify_url("https://example.com/x", Lang::Ja, false),
            Some((SourceKind::SearchHit, 90))
        );
    }

    fn hit(title: &str, content: &str, url: &str) -> SearchHit {
        SearchHit {
            title: title.to_owned(),
            url: url.to_owned(),
            engine: "test".to_owned(),
            content: content.to_owned(),
        }
    }

    #[test]
    fn accepts_japanese_text_for_ja() {
        let h = hit("小渕恵三内閣発足", "小渕恵三氏が新首相として内閣を発足した", "https://example.com/x");
        assert!(language_matches(&h, Lang::Ja));
    }

    #[test]
    fn rejects_chinese_text_for_ja() {
        // 実測 2007-04 ja: zh.wikipedia と baike.baidu から 152 件混入した。
        let h = hit("2007年4月", "这个月发生的主要事件和新闻", "https://zh.wikipedia.org/x");
        assert!(!language_matches(&h, Lang::Ja));
    }

    #[test]
    fn rejects_korean_text_for_ja() {
        // 実測 2007-02 ja: ja.namu.wiki が混入した。
        let h = hit("2007년 2월", "사건 사고 목록", "https://ja.namu.wiki/x");
        assert!(!language_matches(&h, Lang::Ja));
    }

    #[test]
    fn rejects_english_text_for_ja() {
        // 実測 2007-02 ja: wsj.com / nbcnews.com / onthisday.com が混入した。
        let h = hit(
            "Major Events of February 2007",
            "A list of world news headlines from February 2007",
            "https://www.wsj.com/x",
        );
        assert!(!language_matches(&h, Lang::Ja));
    }

    #[test]
    fn accepts_english_text_for_en() {
        let h = hit("April 2007 events", "World news headlines for April 2007", "https://example.com/x");
        assert!(language_matches(&h, Lang::En));
    }

    #[test]
    fn rejects_japanese_text_for_en() {
        let h = hit("2007年4月の出来事", "日本のニュースまとめ", "https://ja.wikipedia.org/x");
        assert!(!language_matches(&h, Lang::En));
    }

    #[test]
    fn portal_gets_priority_20() {
        assert_eq!(
            classify_url("https://en.wikipedia.org/wiki/Portal:Current_events/April_2026", Lang::En, true),
            Some((SourceKind::EnPortal, 20))
        );
    }

    #[test]
    fn urlencode_handles_japanese() {
        assert_eq!(urlencode("2007年4月 出来事"), "2007%E5%B9%B44%E6%9C%88+%E5%87%BA%E6%9D%A5%E4%BA%8B");
    }
}
