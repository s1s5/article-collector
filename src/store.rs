use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::unit::UnitKey;

/// `<data_dir>/YYYY/MM/<lang>`
pub fn unit_dir(cfg: &Config, u: &UnitKey) -> PathBuf {
    PathBuf::from(&cfg.store.data_dir)
        .join(format!("{:04}", u.year))
        .join(format!("{:02}", u.month))
        .join(u.lang.as_dir())
}

/// `<data_dir>/YYYY/MM/<lang>/docs`
pub fn docs_dir(cfg: &Config, u: &UnitKey) -> PathBuf {
    unit_dir(cfg, u).join("docs")
}

/// 台帳のパス: `<data_dir>/_ledger.jsonl`
pub fn ledger_path(cfg: &Config) -> PathBuf {
    PathBuf::from(&cfg.store.data_dir).join("_ledger.jsonl")
}

/// 同一ディレクトリの tmp に書いてから rename する。
///
/// 別 FS の tmp を使うと `rename` が `EXDEV` で失敗するので、
/// 必ず対象と同じディレクトリに置く。部分書き込みを残さない。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("out.json");
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // 失敗した tmp を残さない。
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// JSON を読む。壊れていれば `None`（スキップ判定を「未完了」に倒すため）。
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit::Lang;

    fn cfg(dir: &str) -> Config {
        let mut c = Config::default();
        c.store.data_dir = dir.to_owned();
        c
    }

    fn key(year: u16, month: u8, lang: Lang) -> UnitKey {
        UnitKey { year, month, lang }
    }

    #[test]
    fn unit_dir_is_zero_padded() {
        let c = cfg("data");
        assert_eq!(
            unit_dir(&c, &key(2007, 1, Lang::Ja)),
            PathBuf::from("data/2007/01/ja")
        );
        assert_eq!(
            docs_dir(&c, &key(2026, 12, Lang::En)),
            PathBuf::from("data/2026/12/en/docs")
        );
    }

    #[test]
    fn ledger_path_sits_under_data_dir() {
        assert_eq!(
            ledger_path(&cfg("data")),
            PathBuf::from("data/_ledger.jsonl")
        );
    }

    #[test]
    fn write_atomic_creates_parents_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("ac-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("2026").join("12").join("en").join("news.json");

        write_atomic(&target, br#"{"ok":true}"#).expect("write");

        assert!(target.exists());
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!("ac-store-ow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("news.json");

        write_atomic(&target, b"first").expect("write 1");
        write_atomic(&target, b"second").expect("write 2");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_json_returns_none_for_broken_file() {
        let dir = std::env::temp_dir().join(format!("ac-store-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("bad.json");
        write_atomic(&target, b"{not json").expect("write");

        let parsed: Option<serde_json::Value> = read_json(&target);
        assert!(parsed.is_none());

        let missing: Option<serde_json::Value> = read_json(&dir.join("absent.json"));
        assert!(missing.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
