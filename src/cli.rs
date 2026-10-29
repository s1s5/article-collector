use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "article-collector", about = "Monthly news collector")]
pub struct Cli {
    /// 設定ファイル。無ければ既定値で動く。
    #[arg(long, default_value = "config.toml", global = true)]
    pub config: PathBuf,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// フェーズ A: 取得候補の URL を決めて urls.json を書く
    Discover {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "both")]
        lang: String,
        /// 既存の urls.json を無視して作り直す
        #[arg(long)]
        rediscover: bool,
        /// urls.json の中身を表示するだけ（LLM も fetch もしない）
        #[arg(long)]
        dry_run: bool,
        /// static: 固定クエリの検索（既定）/ agent: rig-agent がツールで探索
        #[arg(long, default_value = "static")]
        mode: String,
    },
    /// フェーズ B: URL を取得して抽出し news.json を書く
    Collect {
        /// 単一ユニット（例 2007-01/ja）
        #[arg(long)]
        unit: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "both")]
        lang: String,
        /// all / a / b（S3 では a と b のみ）
        #[arg(long, default_value = "all")]
        stage: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        rediscover: bool,
        #[arg(long)]
        with_portal: bool,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// 保存済みの成果物を検査する（LLM も fetch もしない）
    Verify {
        #[arg(long)]
        unit: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "both")]
        lang: String,
        /// 結果を JSON で出す
        #[arg(long)]
        json: bool,
    },
    /// 台帳をフェーズ別に集計する（読み取りのみ）
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discover() {
        let cli = Cli::try_parse_from([
            "article-collector", "discover", "--from", "2007-01", "--to", "2007-12", "--lang", "ja",
        ])
        .expect("parses");
        assert!(matches!(cli.cmd, Cmd::Discover { .. }));
    }

    #[test]
    fn parses_collect_unit() {
        let cli = Cli::try_parse_from(["article-collector", "collect", "--unit", "2007-01/ja"])
            .expect("parses");
        match cli.cmd {
            Cmd::Collect { unit, .. } => assert_eq!(unit.as_deref(), Some("2007-01/ja")),
            _ => panic!("expected collect"),
        }
    }

    #[test]
    fn stage_defaults_to_all() {
        let cli = Cli::try_parse_from(["article-collector", "collect", "--unit", "2007-01/ja"])
            .expect("parses");
        match cli.cmd {
            Cmd::Collect { stage, .. } => assert_eq!(stage, "all"),
            _ => panic!("expected collect"),
        }
    }

    #[test]
    fn parses_verify() {
        let cli = Cli::try_parse_from([
            "article-collector", "verify", "--unit", "2007-04/ja", "--json",
        ])
        .expect("parses");
        match cli.cmd {
            Cmd::Verify { unit, json, .. } => {
                assert_eq!(unit.as_deref(), Some("2007-04/ja"));
                assert!(json);
            }
            _ => panic!("expected verify"),
        }
    }

    #[test]
    fn parses_status() {
        let cli =
            Cli::try_parse_from(["article-collector", "status", "--json"]).expect("parses");
        match cli.cmd {
            Cmd::Status { json } => assert!(json),
            _ => panic!("expected status"),
        }
    }
}
