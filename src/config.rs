use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub llm: LlmCfg,
    pub fetch: FetchCfg,
    pub items: ItemsCfg,
    pub dedup: DedupCfg,
    pub store: StoreCfg,
    pub search: SearchCfg,
    pub discover: DiscoverCfg,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmCfg {
    pub base_url: String,
    pub model: String,
    pub num_ctx: u32,
    pub keep_alive: String,
    pub temperature: f64,
    pub timeout_sec: u64,
    pub chunk_input_tokens: usize,
    pub think: bool,
    /// 同時に投げる LLM リクエスト数。実測（2007-01 ja）:
    /// 4 並列で 71.7 秒 / 8 並列で 75.2 秒。mint-ollama が飽和して頭打ちになる。
    pub concurrency: usize,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FetchCfg {
    pub crawl4ai_url: String,
    pub concurrency: usize,
    pub interval_ms: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ItemsCfg {
    pub min: usize,
    pub max: usize,
    /// priority 90（検索ヒット）が最終件数に占めてよい割合の上限。
    /// 実測 2007: `internet.watch` / `k-tai.watch` / `itmedia` の索引が
    /// 「大きなニュース」の枠を圧迫した。0.0 で無効（上限なし）。
    pub search_hit_ratio: f32,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DedupCfg {
    pub jaccard: f32,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StoreCfg {
    pub data_dir: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchCfg {
    pub base_url: String,
    pub pageno: u32,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoverCfg {
    pub mode: String,
    pub max_turns: usize,
    pub target_urls: usize,
    /// 1 ユニットで採用する検索由来 URL の上限。
    /// 実測 2007-04: 36 本をそのまま取ると LLM 呼び出しが膨張し 462 秒かかった。
    /// 決定的ソース（Wikipedia 年記事）はこの上限に含めない。
    pub max_search_urls: usize,
    /// 英語で `Portal:Current events` を候補に含めるか。
    ///
    /// **既定は false。** Portal は 1 か月 6 チャンク前後の LLM 呼び出しになり、
    /// 2007 年の試走では全走時間の主要因だった（24 ユニットで 1 時間 45 分）。
    /// 年記事だけで下限 30 に届かない月は Expand が救う。
    /// 品質を優先するなら `true` に戻す。
    pub with_portal: bool,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self {
            base_url: "http://100.78.108.50:11434".to_owned(),
            model: "ornith-1.5:35b".to_owned(),
            // 実測: 16384 では入力 8,000tok + 出力 130 件 ≈ 10,400tok で溢れ、
            // llama-server が `unexpected end of JSON input` を返した（HTTP 500）。
            // モデルは 262,144 まで対応しているので余裕を持たせる。
            num_ctx: 32768,
            keep_alive: "10m".to_owned(),
            temperature: 0.0,
            timeout_sec: 300,
            chunk_input_tokens: 8000,
            // 既定で切らないと思考が出力予算を食い、done_reason=length で JSON が切れる。
            think: false,
            // 実測: 4 並列が上限。8 にしても壁時間は短くならない。
            concurrency: 4,
        }
    }
}

impl Default for FetchCfg {
    fn default() -> Self {
        Self {
            crawl4ai_url: crate::fetch::CRAWL4AI_URL.to_owned(),
            concurrency: 2,
            // Wikipedia API が 1.2 秒間隔でも 429 を返した実測に基づく。
            interval_ms: 1000,
        }
    }
}

impl Default for ItemsCfg {
    fn default() -> Self {
        Self {
            min: 30,
            max: 200,
            // 実測 2007-03 ja は 643 件のうち過半が IT 系索引だった。
            // 3 割までに抑える。
            search_hit_ratio: 0.3,
        }
    }
}

impl Default for DedupCfg {
    fn default() -> Self {
        Self { jaccard: 0.6 }
    }
}

impl Default for StoreCfg {
    fn default() -> Self {
        Self {
            data_dir: "data".to_owned(),
        }
    }
}

impl Default for SearchCfg {
    fn default() -> Self {
        Self {
            base_url: "http://searxng.localhost".to_owned(),
            pageno: 1,
        }
    }
}

impl Default for DiscoverCfg {
    fn default() -> Self {
        Self {
            mode: "static".to_owned(),
            max_turns: 5,
            target_urls: 12,
            // 実測: 検索由来を 8 本に絞ると、決定的ソース 1〜2 本と合わせて
            // 1 ユニット 10 本前後。上限 200 件に対して十分。
            max_search_urls: 8,
            // 実測 2007: Portal を全月で使うと 1 ユニットあたり 6 チャンク増える。
            with_portal: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            llm: LlmCfg::default(),
            fetch: FetchCfg::default(),
            items: ItemsCfg::default(),
            dedup: DedupCfg::default(),
            store: StoreCfg::default(),
            search: SearchCfg::default(),
            discover: DiscoverCfg::default(),
        }
    }
}

/// 設定を読む。`config.toml` が無ければ既定で動く（検証を設定ファイル無しでも回すため）。
///
/// 環境変数が最優先:
/// - `ARTICLE_COLLECTOR_DATA_DIR` → `store.data_dir`
/// - `ARTICLE_COLLECTOR_ITEMS_MIN` → `items.min`
pub fn load(path: Option<&Path>) -> Result<Config, String> {
    let mut config = match path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            toml::from_str::<Config>(&text)
                .map_err(|e| format!("parse {}: {e}", path.display()))?
        }
        None => Config::default(),
    };
    apply_env(&mut config)?;
    Ok(config)
}

/// リポジトリ直下の `config.toml` があれば読み、無ければ既定。
pub fn load_default() -> Result<Config, String> {
    let path = Path::new("config.toml");
    if path.exists() {
        load(Some(path))
    } else {
        let mut config = Config::default();
        apply_env(&mut config)?;
        Ok(config)
    }
}

fn apply_env(config: &mut Config) -> Result<(), String> {
    if let Ok(dir) = std::env::var("ARTICLE_COLLECTOR_DATA_DIR") {
        if !dir.is_empty() {
            config.store.data_dir = dir;
        }
    }
    if let Ok(min) = std::env::var("ARTICLE_COLLECTOR_ITEMS_MIN") {
        if !min.is_empty() {
            config.items.min = min
                .parse()
                .map_err(|_| format!("ARTICLE_COLLECTOR_ITEMS_MIN is not a number: {min}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_usable() {
        let c = Config::default();
        assert_eq!(c.llm.model, "ornith-1.5:35b");
        assert!(!c.llm.think, "think must default to false");
        assert_eq!(c.items.min, 30);
        assert_eq!(c.items.max, 200);
        assert_eq!(c.store.data_dir, "data");
    }

    #[test]
    fn parses_partial_toml() {
        let text = "[llm]\nmodel = \"x\"\n";
        let c: Config = toml::from_str(text).expect("parses");
        assert_eq!(c.llm.model, "x");
        // 指定の無いキーは既定で埋まる
        assert_eq!(c.items.min, 30);
        assert_eq!(c.fetch.interval_ms, 1000);
    }

    #[test]
    fn rejects_unknown_key() {
        let text = "[llm]\nnope = 1\n";
        assert!(toml::from_str::<Config>(text).is_err());
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(toml::from_str::<Config>("[llm\n").is_err());
    }
}
