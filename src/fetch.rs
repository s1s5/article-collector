use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const CRAWL4AI_URL: &str = "http://crawl4ai.localhost/md";
pub const MIN_BODY_CHARS: usize = 200;

/// 実測: 404 ページは 37 字、存在しない Portal は「Wikipedia does not have a portal」。
const MISSING_MARKERS: [&str; 3] = [
    "指定されたページが見つかりません",
    "ページが見つかりません",
    "Wikipedia does not have a portal",
];

/// ウィキニュースの空ページ。実測で大半の日がこれ。
const EMPTY_MARKER: &str = "この日の投稿はありませんでした";

#[derive(Clone, Debug)]
pub struct FetchedDoc {
    pub url: String,
    pub markdown: String,
    /// raw の文字数（sanitize 前）。doc に `raw_chars` として残す。
    pub chars: usize,
    pub fetched_at: String,
}

/// 取得の 3 状態。**欠損は成功でも失敗でもない。**
///
/// Crawl4AI は存在しないページも HTTP 200 で返す（実測）ので、
/// ステータスコードでは欠損を判定できない。本文を見るしかない。
#[derive(Clone, Debug)]
pub enum FetchOutcome {
    Ok(FetchedDoc),
    /// ページが存在しない（404 定型文・極小本文・存在しない Portal）。
    MissingPage { url: String },
    /// ページはあるが中身が空（ウィキニュースの「投稿はありませんでした」のみ）。
    EmptyContent { url: String },
}

#[derive(Debug)]
pub enum FetchError {
    Http { status: u16 },
    Transport(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http { status } => write!(f, "http {status}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// 本文から取得の 3 状態を判定する。**ここが生存判定の唯一の実装**。
///
/// 判定順:
/// 1. 200 字未満 → `MissingPage`（実測: 404 は 37 字）
/// 2. 404 定型文を含む → `MissingPage`
/// 3. 空ページの定型文だけで占められる → `EmptyContent`
/// 4. それ以外 → `Ok`
pub fn classify(markdown: &str) -> Classified {
    let chars = markdown.chars().count();
    if chars < MIN_BODY_CHARS {
        return Classified::Missing;
    }
    if MISSING_MARKERS.iter().any(|m| markdown.contains(m)) {
        return Classified::Missing;
    }
    if is_empty_content(markdown) {
        return Classified::Empty;
    }
    Classified::Alive(chars)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classified {
    Alive(usize),
    Missing,
    Empty,
}

/// 空ページ判定: 定型文と見出しを除いた本文が実質空か。
///
/// **箇条書きの行を除外してはいけない。** ウィキニュースの実ニュースは
/// `* ...` 行にある。行頭記号だけを落として中身を数える。
fn is_empty_content(markdown: &str) -> bool {
    if !markdown.contains(EMPTY_MARKER) {
        return false;
    }
    let stripped = markdown
        .replace(EMPTY_MARKER, "")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| {
            // 行頭の箇条書き記号だけを落とし、本文は残す。
            let t = line.trim_start();
            let body = t
                .strip_prefix("* ")
                .or_else(|| t.strip_prefix("- "))
                .unwrap_or(t);
            body.trim().chars().count()
        })
        .sum::<usize>();
    stripped < 500
}

/// ホスト別の最終呼び出し時刻。`interval_ms` を下限に待つ。
fn last_call() -> &'static Mutex<HashMap<String, Instant>> {
    static LAST: std::sync::OnceLock<Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("")
        .to_lowercase()
}

/// ホスト別に `interval_ms` 以上あける。
async fn throttle(url: &str, interval_ms: u64) {
    if interval_ms == 0 {
        return;
    }
    let host = host_of(url);
    let wait = {
        let mut map = last_call().lock().expect("rate limiter poisoned");
        let now = Instant::now();
        let wait = match map.get(&host) {
            Some(prev) => {
                let elapsed = now.duration_since(*prev).as_millis() as u64;
                interval_ms.saturating_sub(elapsed)
            }
            None => 0,
        };
        map.insert(host, now + Duration::from_millis(wait));
        wait
    };
    if wait > 0 {
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
}

/// Crawl4AI で 1 ページ取る。
///
/// 状態は `FetchOutcome` で返す。HTTP エラーと転送失敗だけ `Err`。
pub async fn fetch_page(
    base_url: &str,
    url: &str,
    interval_ms: u64,
) -> Result<FetchOutcome, FetchError> {
    throttle(url, interval_ms).await;

    for attempt in 0..3u32 {
        let body = serde_json::json!({ "url": url, "f": "fit" });
        let resp = reqwest::Client::new()
            .post(base_url)
            .json(&body)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.as_u16() == 429 {
            // Retry-After を見る。無ければ指数バックオフ + ジッター。
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| {
                    let base = 1u64 << attempt;
                    let jitter = (u64::from(std::process::id()) % 7) * 100;
                    Duration::from_millis(base * 1_000 + jitter)
                });
            tokio::time::sleep(wait).await;
            continue;
        }
        if !status.is_success() {
            return Err(FetchError::Http {
                status: status.as_u16(),
            });
        }

        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        let markdown = value
            .get("markdown")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_owned();

        return Ok(match classify(&markdown) {
            Classified::Alive(chars) => FetchOutcome::Ok(FetchedDoc {
                url: url.to_owned(),
                markdown,
                chars,
                fetched_at: chrono::Utc::now().to_rfc3339(),
            }),
            Classified::Missing => FetchOutcome::MissingPage {
                url: url.to_owned(),
            },
            Classified::Empty => FetchOutcome::EmptyContent {
                url: url.to_owned(),
            },
        });
    }

    Err(FetchError::Http { status: 429 })
}

#[cfg(test)]
mod survival {
    use super::*;

    #[test]
    fn short_404_page_is_missing() {
        // 実測: internet.watch の 404 は 37 字。
        let body = "# 指定されたページが見つかりません\n3秒後にトップページに移動します。\n";
        assert_eq!(classify(body), Classified::Missing);
    }

    #[test]
    fn portal_page_is_missing() {
        // 実測: 存在しない Portal は 1,942 字で 200 OK。
        let body = format!(
            "Wikipedia does not have a portal with this exact name. {}",
            "x".repeat(1_800)
        );
        assert_eq!(classify(&body), Classified::Missing);
    }

    #[test]
    fn wikinews_empty_day_is_empty() {
        let body = format!(
            "## 2007年4月1日\n* {EMPTY_MARKER}\n## 2007年4月2日\n* {EMPTY_MARKER}\n{}",
            "　".repeat(200)
        );
        assert_eq!(classify(&body), Classified::Empty);
    }

    #[test]
    fn wikinews_with_real_content_is_alive() {
        let body = format!(
            "## 2007年4月17日\n{EMPTY_MARKER}\n* バージニア工科大学で銃撃事件。{}\n",
            "詳細。".repeat(300)
        );
        assert!(matches!(classify(&body), Classified::Alive(_)));
    }

    #[test]
    fn long_normal_page_is_alive() {
        let body = "y".repeat(300);
        assert_eq!(classify(&body), Classified::Alive(300));
    }

    #[test]
    fn host_is_extracted() {
        assert_eq!(host_of("https://example.com/a/b"), "example.com");
        assert_eq!(host_of("http://A.B.C/x"), "a.b.c");
    }
}
