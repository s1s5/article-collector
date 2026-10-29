use std::sync::OnceLock;

use chrono::NaiveDate;
use futures::StreamExt;
use rig_core::completion::message::AssistantContent;
use rig_core::completion::{CompletionRequest, CompletionResponse, FinishReason, Reasoning};
use rig_core::completion::request::ToolDefinition;
use rig_core::message::ToolName;
use rig_core::providers::ollama::OllamaConfig;
use serde::{Deserialize, Serialize};

use crate::sanitize::render_for_prompt;
use crate::slice::MonthSection;

pub const DEFAULT_BASE_URL: &str = "http://100.78.108.50:11434";
pub const DEFAULT_MODEL: &str = "ornith-1.5:35b";
pub const DEFAULT_NUM_CTX: u32 = 32768;
/// 出力の上限。`num_ctx` は入力と出力の合計なので、
/// 入力予算（`chunk_input_tokens`）を引いた値を出力に充てる。
/// 実測: 130 件の提出に約 10,400 tok 必要だった。
pub const DEFAULT_MAX_TOKENS: u64 = 16384;
pub const DEFAULT_CHUNK_INPUT_TOKENS: usize = 8000;
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const KEEP_ALIVE: &str = "10m";

pub const SUBMIT_ITEMS_TOOL: &str = "submit_items";

/// 抽出結果を提出させるツール。
///
/// **structured output は使わない。** `tool_choice` も指定しない。
/// スキーマをプロバイダに強制するのではなく、ツール定義を渡して
/// プロンプトで「必ず 1 回呼べ」と指示するだけ。
///
/// 実測（2007-02、同じ本文）: テキスト出力は NDJSON になりパースに失敗したが、
/// ツール経由なら `arguments` に配列が入り 123 件 / null 0 で返った。
fn submit_items_tool() -> ToolDefinition {
    ToolDefinition::new(
        ToolName::new(SUBMIT_ITEMS_TOOL).expect("valid tool name"),
        "月次ニュースの抽出結果を提出する。必ず 1 回だけ呼ぶ。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "date": {
                                "type": ["string", "null"],
                                "description": "YYYY-MM-DD または null"
                            },
                            "title": { "type": "string", "description": "30字以内" },
                            "summary": { "type": "string", "description": "60字以内" }
                        },
                        "required": ["date", "title", "summary"]
                    }
                }
            },
            "required": ["items"]
        }),
    )
}

const PREAMBLE: &str = "\
あなたは日本語ニュースの月次ダイジェストを作る編集者です。
与えられたテキストは、ある年のある月の出来事一覧です。

作業は**機械的な変換**です。要約ではない。

厳守:
- 箇条書きの **1 行 = 1 件** として扱う。1 行も省略しない。
- 複数行を 1 件にまとめない。「大きなものだけ選ぶ」という判断をしない。
- テキストの行数と提出件数はほぼ同じになるのが正解。件数が半分以下なら失敗。
- テキストに書かれた事実だけを書く。推測・補完・知識の追加をしない。
- 各ブロックの先頭に `YYYY-MM-DD` の行がある。そのブロックの項目は
  すべてその日付を持つ。date には先頭の日付を**そのまま写す**。
  自分で日付を考えたり補ったりしない。
- 先頭が `(日付不明)` のブロックだけ date を null にする。

提出方法:
- 抽出結果は必ず `submit_items` ツールを **1 回だけ** 呼んで提出する。
- 本文に JSON を書かない。説明文も書かない。
- ツールを呼ばずに終わらない。

各要素の形:
{\"date\":\"YYYY-MM-DD または null\",\"title\":\"30字以内\",\"summary\":\"60字以内\"}";

/// 件数が期待に届かないときに差し替えるプロンプト。
///
/// 実測: 初回プロンプトで 2007-01（151 行）から 62 件しか出なかった。
/// 1 日 1〜3 件に間引かれていた。
const PREAMBLE_RETRY: &str = "\
あなたは日本語ニュースの月次ダイジェストを作る編集者です。
前回の提出は件数が足りませんでした。今回は**全行を列挙**してください。

厳守:
- 入力の箇条書き行を上から順に 1 行ずつ処理し、**1 行につき 1 件** を出す。
- 行を飛ばさない。まとめない。選別しない。
- 「重要でない」と判断して省くことを禁ずる。重要度の判断はこの工程では行わない。
- 各ブロック先頭の `YYYY-MM-DD` をそのまま date に写す。
- 先頭が `(日付不明)` のブロックだけ date を null にする。

提出方法:
- 必ず `submit_items` ツールを 1 回だけ呼ぶ。
- 件数は入力の箇条書き行数と同程度になるはず。

各要素の形:
{\"date\":\"YYYY-MM-DD または null\",\"title\":\"30字以内\",\"summary\":\"60字以内\"}";

/// 英語用のプロンプト。日付は `YYYY-MM-DD` で機械的に振られているので、
/// 日本語版と同じく「写すだけ」を強制する。
const PREAMBLE_EN: &str = "\
You are an editor building a monthly news digest.
The input is a list of events from one month of one year.

This is a **mechanical transformation**, not a summary.

Rules:
- Treat **one bullet line as one item**. Do not skip any line.
- Do not merge lines. Do not decide which events are 'big enough'.
- The number of submitted items should be close to the number of input lines.
- Write only facts stated in the text. No inference, no added knowledge.
- Each block starts with a `YYYY-MM-DD` line. Every item in that block has
  that date. Copy the leading date **verbatim**; do not compute it yourself.
- Only blocks starting with `(unknown date)` get `date: null`.

Submission:
- Call the `submit_items` tool **exactly once**.
- Do not write JSON in the body. Do not write prose.
- Do not finish without calling the tool.

Item shape:
{\"date\":\"YYYY-MM-DD or null\",\"title\":\"<=80 chars\",\"summary\":\"<=200 chars\"}";

const PREAMBLE_EN_RETRY: &str = "\
You are an editor building a monthly news digest.
Your previous submission had too few items. This time **enumerate every line**.

Rules:
- Walk the input bullet lines from top to bottom and emit **one item per line**.
- Do not skip lines. Do not merge. Do not filter.
- Do not judge importance. That is not part of this step.
- Copy each block's leading `YYYY-MM-DD` verbatim into `date`.
- Only blocks starting with `(unknown date)` get `date: null`.

Submission:
- Call `submit_items` exactly once.
- The item count should be close to the input line count.

Item shape:
{\"date\":\"YYYY-MM-DD or null\",\"title\":\"<=80 chars\",\"summary\":\"<=200 chars\"}";

/// 言語別のプロンプト対（通常 / 件数不足時の再試行）。
fn preambles(lang: crate::unit::Lang) -> (&'static str, &'static str) {
    match lang {
        crate::unit::Lang::Ja => (PREAMBLE, PREAMBLE_RETRY),
        crate::unit::Lang::En => (PREAMBLE_EN, PREAMBLE_EN_RETRY),
    }
}

/// 期待件数の下限。入力の箇条書き行数から算出する。
/// 期待される項目数の下限。
///
/// **行頭の箇条書き記号で数えてはいけない。** サニタイズ済みのセクションでは
/// `* ` が落ちているため、記号で数えると常に 0 になり、リトライが発動しない。
/// 実測: これで 2007-01 ja が 115 件ではなく 30 件で確定した。
///
/// 数えるのは「非空行から日付だけの行を除いたもの」。日付見出しは
/// `render_for_prompt` が別途 ISO 日付として付け直すので、項目にはならない。
fn expected_min_lines(section: &MonthSection) -> usize {
    let content_lines = section
        .blocks
        .iter()
        .flat_map(|b| b.lines.iter())
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .filter(|line| !is_date_only_line(line))
        .count();
    // 見出しや補足も混ざるので、控えめに 6 割を下限にする。
    // 実測 2007-01 ja: 非空行 121 / 内容行 121 → 下限 72 で、115 件出た。
    (content_lines * 6) / 10
}

/// 日付だけの行か（`1月1日` / `January 19` / `2007-01-01`）。
fn is_date_only_line(line: &str) -> bool {
    static JA: OnceLock<regex::Regex> = OnceLock::new();
    static EN: OnceLock<regex::Regex> = OnceLock::new();
    static ISO: OnceLock<regex::Regex> = OnceLock::new();

    let ja = JA.get_or_init(|| {
        regex::Regex::new(r"^\d{1,2}月\d{1,2}日(?:\s*[（(].*)?$").expect("ja date only")
    });
    if ja.is_match(line) {
        return true;
    }
    let en = EN.get_or_init(|| {
        regex::Regex::new(
            r"^(?:January|February|March|April|May|June|July|August|September|October|November|December)\s+\d{1,2}(?:,\s*\d{4})?$",
        )
        .expect("en date only")
    });
    if en.is_match(line) {
        return true;
    }
    let iso = ISO.get_or_init(|| {
        regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("iso date only")
    });
    iso.is_match(line)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewsItem {
    pub date: Option<String>,
    pub title: String,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub num_ctx: u32,
    pub max_tokens: u64,
    pub keep_alive: String,
    pub temperature: f64,
    pub think: bool,
    /// 1 チャンクの入力予算（トークン）。文字数に換算して chunk_section に渡す。
    pub chunk_input_tokens: usize,
    /// 同時に投げるリクエスト数。実測: 4 が上限。
    pub concurrency: usize,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            num_ctx: DEFAULT_NUM_CTX,
            max_tokens: DEFAULT_MAX_TOKENS,
            keep_alive: KEEP_ALIVE.to_owned(),
            temperature: 0.0,
            think: false,
            chunk_input_tokens: DEFAULT_CHUNK_INPUT_TOKENS,
            concurrency: DEFAULT_CONCURRENCY,
        }
    }
}

impl LlmConfig {
    /// `config.toml` の `[llm]` から組み立てる。
    pub fn from_cfg(cfg: &crate::config::LlmCfg) -> Self {
        Self {
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            num_ctx: cfg.num_ctx,
            // `num_ctx` は入力と出力の合計。入力予算を引いた値を出力に充てる。
            // これを `num_ctx` そのままにすると max_tokens が大きく過ぎて
            // 実際の入力を入れる余地をサーバ側で失うことがある。
            max_tokens: u64::from(cfg.num_ctx.saturating_sub(cfg.chunk_input_tokens as u32)),
            keep_alive: cfg.keep_alive.clone(),
            temperature: cfg.temperature,
            think: cfg.think,
            chunk_input_tokens: cfg.chunk_input_tokens,
            concurrency: cfg.concurrency,
        }
    }
}

#[derive(Debug)]
pub enum MapError {
    /// 出力が上限で切れた。`think: false` を外すとこれになる（実測: thinking 35,077字）。
    Truncated { finish: String },
    Parse(String),
    Llm(String),
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { finish } => write!(f, "output truncated (finish_reason={finish})"),
            Self::Parse(m) => write!(f, "parse error: {m}"),
            Self::Llm(m) => write!(f, "llm error: {m}"),
        }
    }
}

impl std::error::Error for MapError {}

pub struct ExtractResult {
    pub items: Vec<NewsItem>,
    pub finish_reasons: Vec<String>,
    /// プロンプト本体の sha256。台帳に残して「プロンプトを変えたら再抽出」を判定できるようにする。
    pub prompt_sha256: String,
    /// 何回 LLM を呼んだか（1 or 2）。
    pub attempts: u8,
    /// 期待した件数の下限（入力の箇条書き行数から算出）。
    pub expected_min: usize,
    /// 何チャンクに分割したか。
    pub chunks: usize,
}

fn finish_label(reason: Option<FinishReason>) -> String {
    match reason {
        Some(FinishReason::Stop) => "stop".to_owned(),
        Some(FinishReason::Length) => "length".to_owned(),
        Some(FinishReason::ToolCalls) => "tool_calls".to_owned(),
        Some(FinishReason::ContentFilter) => "content_filter".to_owned(),
        Some(FinishReason::Other(s)) => s,
        // FinishReason は #[non_exhaustive]。未知の variant も落とさず拾う。
        Some(other) => format!("{other:?}").to_lowercase(),
        None => "none".to_owned(),
    }
}

fn text_of(resp: &CompletionResponse) -> &str {
    resp.choice
        .iter()
        .find_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `submit_items` の引数から `items` を取り出す。呼ばれていなければ `None`。
///
/// **複数回の呼び出しをすべて集約する。** 実測でモデルは 1 応答で
/// `submit_items` を複数回呼ぶことがあった（1 回目だけ読むと件数が半減した）。
fn items_from_tool_call(resp: &CompletionResponse) -> Option<(Vec<NewsItem>, usize)> {
    let mut all = Vec::new();
    let mut calls = 0usize;
    for content in &resp.choice {
        let AssistantContent::ToolCall(call) = content else {
            continue;
        };
        if call.function.name.as_str() != SUBMIT_ITEMS_TOOL {
            continue;
        }
        calls += 1;
        let Some(raw) = call.function.arguments.get("items") else {
            continue;
        };
        if let Ok(items) = serde_json::from_value::<Vec<NewsItem>>(raw.clone()) {
            all.extend(items);
        }
    }
    if calls == 0 {
        None
    } else {
        Some((all, calls))
    }
}

/// 出力から JSON 配列を取り出す。以下の形を受け入れる。
///
/// 1. 素の配列 `[{...},{...}]`
/// 2. 前置きやコードフェンス付きの配列
/// 3. **NDJSON**（1 行 1 オブジェクト）— 実測で `ornith-1.5:35b` は
///    2007-02 で配列ではなくこの形を返した（`done_reason=stop`、15,179 字）
fn parse_items(raw: &str) -> Result<Vec<NewsItem>, MapError> {
    let body = strip_fence(raw);
    if let Ok(v) = serde_json::from_str::<Vec<NewsItem>>(body) {
        return Ok(v);
    }
    // 前置きがある場合に備え、最初の '[' から最後の ']' までを試す。
    if let (Some(start), Some(end)) = (body.find('['), body.rfind(']')) {
        if end > start {
            let slice = &body[start..=end];
            if let Ok(v) = serde_json::from_str::<Vec<NewsItem>>(slice) {
                return Ok(v);
            }
        }
    }
    // NDJSON: 1 行 1 オブジェクト。1 行でも読めたらそれを採用する。
    let ndjson = parse_ndjson(body);
    if !ndjson.is_empty() {
        return Ok(ndjson);
    }
    Err(MapError::Parse(format!(
        "no JSON array or NDJSON objects in output ({} chars)",
        raw.chars().count()
    )))
}

/// 1 行 1 オブジェクトを読む。行末のカンマと空行は許容する。
fn parse_ndjson(body: &str) -> Vec<NewsItem> {
    body.lines()
        .map(|line| line.trim().trim_end_matches(','))
        .filter(|line| line.starts_with('{') && line.ends_with('}'))
        .filter_map(|line| serde_json::from_str::<NewsItem>(line).ok())
        .collect()
}

fn strip_fence(raw: &str) -> &str {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    let rest = rest.trim_start_matches('\n');
    match rest.rfind("```") {
        Some(pos) => rest[..pos].trim(),
        None => rest,
    }
}

fn date_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("date pattern"))
}

/// LLM が返した date 文字列を検証する。範囲外・不正な形式は `None`。
pub fn normalize_date(raw: &str, year: u16, month: u8) -> Option<String> {
    if !date_re().is_match(raw) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(raw, "%Y-%m-%d").ok()?;
    use chrono::Datelike;
    if parsed.year() != i32::from(year) || parsed.month() != u32::from(month) {
        return None;
    }
    Some(raw.to_owned())
}

fn build_request(cfg: &LlmConfig, prompt: String, preamble: &'static str) -> CompletionRequest {
    let mut request = CompletionRequest::new(prompt).preamble(preamble);
    // 既定では思考が出力予算を食い、実測で content 5,996字 / thinking 35,077字 /
    // done_reason=length となり JSON が切れた。設定で有効化しない限り必ず Off にする。
    if !cfg.think {
        request = request.reasoning(Reasoning::Off);
    }
    request
        .temperature(cfg.temperature)
        .max_tokens(cfg.max_tokens)
        // structured output ではなく、ツール定義を渡すだけ。tool_choice は指定しない。
        .tool(submit_items_tool())
        .additional_params(serde_json::json!({
            "keep_alive": cfg.keep_alive,
            "options": { "num_ctx": cfg.num_ctx }
        }))
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 月セクションから記事を抽出する。
///
/// **1 回あたりの入力を小さくし、複数チャンクを並列に投げる。**
/// 実測（2007-01 ja、30 日ブロック / 9,751 字）:
/// - 1 本送りだと件数不足でリトライが入り 78.2 秒 / 117 件
/// - 4 分割並列だと全チャンク 1 回で完了し 71.7 秒 / 118 件
/// - 8 分割にしても 75.2 秒。mint-ollama が飽和して頭打ちになる
///
/// つまり並列化の主な効果は「速さ」ではなく **認知負荷の低下（リトライ消滅）**。
pub async fn extract_section(
    cfg: &LlmConfig,
    section: &MonthSection,
    year: u16,
    month: u8,
    lang: crate::unit::Lang,
) -> Result<ExtractResult, MapError> {
    let body = render_for_prompt(section, year, month, lang);
    // build_request が body を move するので、ハッシュは先に取る。
    let prompt_sha256 = sha256_hex(&body);
    if body.trim().is_empty() {
        return Ok(ExtractResult {
            items: Vec::new(),
            finish_reasons: vec![],
            prompt_sha256,
            attempts: 0,
            expected_min: 0,
            chunks: 0,
        });
    }

    let expected_min = expected_min_lines(section);

    // 極端に小さいセクションは抽出しない。
    // 実測 2026-11 の `予定・予測` は 127 字しかなく、モデルはツールを呼ばずに
    // 説明文（432 字）を返した。これは失敗ではなく「中身が無い」。
    const MIN_SECTION_CHARS: usize = 300;
    let body_chars = body.chars().count();
    if body_chars < MIN_SECTION_CHARS {
        eprintln!("       section is tiny ({body_chars} chars); skipping extraction");
        return Ok(ExtractResult {
            items: Vec::new(),
            finish_reasons: vec!["skipped:tiny".to_owned()],
            prompt_sha256,
            attempts: 0,
            expected_min,
            chunks: 0,
        });
    }

    // チャンク予算は文字数に換算する。文字/トークン比は実測で
    // 日本語 1.78 / 英語 4.8（同じ内容でも英語の方がトークンが少ない）。
    let chars_per_token = match lang {
        crate::unit::Lang::Ja => 1.78_f32,
        crate::unit::Lang::En => 4.8_f32,
    };
    let max_chars = (cfg.chunk_input_tokens as f32 * chars_per_token) as usize;
    // **サニタイズしてから分割する。**
    // 予算 `max_chars` はサニタイズ後の文字数で決めている
    // （リンク除去で ja は 16%、en は 31% になる）。
    // 生のセクションを渡すと 3〜6 倍のチャンクに割れる。
    // 実測: Portal 2026-04 は生 630,951 字 → サニタイズ後 182,572 字。
    // 生で割ると 15 チャンク、サニタイズ後なら 5〜6 チャンクになる。
    let sanitized = crate::sanitize::sanitize_section(section);
    let chunks = crate::chunk::chunk_section(&sanitized, max_chars.max(1_000));
    let chunk_count = chunks.len();
    // チャンク境界を跨いだ日付を次チャンクの先頭へ引き継ぐ。
    let mut chunks = chunks;
    crate::chunk::carry_date(&mut chunks);

    let client = OllamaConfig::new()
        .with_base_url(&cfg.base_url)
        .with_api_key("ollama")
        .client();
    let model = client.native_completion(&cfg.model);

    // チャンクを上限 `cfg.concurrency` 本まで同時に投げる。
    let concurrency = cfg.concurrency.max(1);
    let results = futures::stream::iter(chunks.into_iter().enumerate().map(|(i, chunk)| {
        let model = model.clone();
        let cfg = cfg.clone();
        async move {
            let outcome =
                extract_chunk(&model, &cfg, &chunk, year, month, i, chunk_count, lang).await;
            (i, outcome)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    let mut finish_reasons = Vec::new();
    let mut attempts = 0u8;
    let mut items = Vec::new();

    // チャンク番号順に並べ直して結合（順序を決定的にする）。
    let mut ordered = results;
    ordered.sort_by_key(|(i, _)| *i);
    for (i, outcome) in ordered {
        let (chunk_items, chunk_finish, chunk_attempts) = outcome?;
        finish_reasons.extend(chunk_finish.into_iter().map(|f| format!("{f}#{i}")));
        attempts = attempts.max(chunk_attempts);
        items.extend(chunk_items);
    }

    // 重複除去（同じ date + 同じ title）。並列結合で境界が重なることは無いが、
    // モデルが同一見出しを 2 回出すことがあったので機械的に落とす。
    let before = items.len();
    let mut seen = std::collections::HashSet::new();
    items.retain(|item| seen.insert((item.date.clone(), item.title.clone())));
    if items.len() != before {
        eprintln!("       dedup exact {} -> {}", before, items.len());
    }

    Ok(ExtractResult {
        items,
        finish_reasons,
        prompt_sha256,
        attempts,
        expected_min,
        chunks: chunk_count,
    })
}

/// 1 チャンクを抽出する。件数不足なら言語別の再試行プロンプトで 1 回だけ再試行。
async fn extract_chunk(
    model: &rig_core::driver::Model<rig_core::providers::ollama::Chat>,
    cfg: &LlmConfig,
    section: &MonthSection,
    year: u16,
    month: u8,
    index: usize,
    total: usize,
    lang: crate::unit::Lang,
) -> Result<(Vec<NewsItem>, Vec<String>, u8), MapError> {
    let body = render_for_prompt(section, year, month, lang);
    let expected = expected_min_lines(section);
    let mut finish_reasons = Vec::new();
    let mut attempts = 0u8;
    let mut best: Option<Vec<NewsItem>> = None;
    let (first, retry) = preambles(lang);

    for (round, preamble) in [(0u8, first), (1, retry)] {
        if round > 0 {
            let got = best.as_ref().map_or(0, Vec::len);
            if got >= expected {
                break;
            }
            eprintln!(
                "       chunk {}/{} retry: {got} < expected {expected}",
                index + 1,
                total
            );
        }
        attempts += 1;

        let resp = model
            .call(build_request(cfg, body.clone(), preamble))
            .await
            .map_err(|e| MapError::Llm(e.to_string()))?;

        let finish = finish_label(resp.finish_reason());
        if finish == "length" || finish == "content_filter" {
            return Err(MapError::Truncated { finish });
        }
        finish_reasons.push(format!("r{round}:{finish}"));

        let mut items = match items_from_tool_call(&resp) {
            Some((items, calls)) => {
                eprintln!(
                    "       chunk {}/{} round={round} calls={calls} items={}",
                    index + 1,
                    total,
                    items.len()
                );
                items
            }
            None => {
                // ツールを呼ばず本文も JSON でない場合は、このチャンクを諦めて空で返す。
                // チャンク 1 本の失敗でユニット全体を落とさない（03-structure S3 の要件）。
                // 切り詰めは `finish_reason` で別途検出しているので、ここでの
                // 取りこぼしは「モデルが書式を守らなかった」だけに限られる。
                match parse_items(text_of(&resp)) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        eprintln!(
                            "       chunk {}/{} round={round} unparsable, dropping: {e}",
                            index + 1,
                            total
                        );
                        finish_reasons.push(format!("r{round}:unparsable"));
                        Vec::new()
                    }
                }
            }
        };
        for item in &mut items {
            item.date = item
                .date
                .as_deref()
                .and_then(|d| normalize_date(d, year, month));
            item.title = item.title.trim().to_owned();
            item.summary = item.summary.trim().to_owned();
        }
        items.retain(|i| !i.title.is_empty());

        if best.as_ref().map_or(true, |b| items.len() > b.len()) {
            best = Some(items);
        }
    }

    Ok((best.unwrap_or_default(), finish_reasons, attempts))
}

#[cfg(test)]
mod truncated_tests {
    use super::*;

    #[test]
    fn length_is_recognized_as_truncation() {
        assert_eq!(finish_label(Some(FinishReason::Length)), "length");
    }

    #[test]
    fn stop_is_not_truncation() {
        assert_eq!(finish_label(Some(FinishReason::Stop)), "stop");
    }

    #[test]
    fn unknown_reason_is_verbatim() {
        assert_eq!(finish_label(Some(FinishReason::Other("boom".into()))), "boom");
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_plain_array() {
        let raw = r#"[{"date":"2007-01-01","title":"a","summary":"b"}]"#;
        let items = parse_items(raw).expect("parses");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].date.as_deref(), Some("2007-01-01"));
    }

    #[test]
    fn parses_fenced_array() {
        let raw = "```json\n[{\"date\":null,\"title\":\"a\",\"summary\":\"b\"}]\n```";
        let items = parse_items(raw).expect("parses");
        assert_eq!(items.len(), 1);
        assert!(items[0].date.is_none());
    }

    #[test]
    fn parses_with_preamble() {
        let raw = "以下が結果です:\n[{\"date\":\"2007-01-02\",\"title\":\"a\",\"summary\":\"b\"}]\n以上。";
        let items = parse_items(raw).expect("parses");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parses_ndjson_objects() {
        let raw = concat!(
            "{\"date\":\"2007-02-01\",\"title\":\"a\",\"summary\":\"b\"}\n",
            "{\"date\":\"2007-02-02\",\"title\":\"c\",\"summary\":\"d\"}\n",
        );
        let items = parse_items(raw).expect("parses ndjson");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].title, "c");
    }

    #[test]
    fn parses_ndjson_with_trailing_commas() {
        let raw = "{\"date\":null,\"title\":\"a\",\"summary\":\"b\"},\n{\"date\":null,\"title\":\"c\",\"summary\":\"d\"},";
        let items = parse_items(raw).expect("parses ndjson with commas");
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn truncated_json_fails_to_parse() {
        let raw = "[{\"date\":\"2007-01-01\",\"title\":\"a\",\"summary\":\"b\"},{\"date\":\"2007-01-0";
        assert!(matches!(parse_items(raw), Err(MapError::Parse(_))));
    }

    #[test]
    fn rejects_date_outside_unit() {
        assert!(normalize_date("2007-02-01", 2007, 1).is_none());
        assert!(normalize_date("2006-01-01", 2007, 1).is_none());
        assert_eq!(normalize_date("2007-01-31", 2007, 1).as_deref(), Some("2007-01-31"));
    }

    #[test]
    fn rejects_malformed_date() {
        assert!(normalize_date("2007/01/01", 2007, 1).is_none());
        assert!(normalize_date("2007-13-01", 2007, 1).is_none());
    }
}
