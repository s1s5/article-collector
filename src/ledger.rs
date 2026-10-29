use std::io::Write;
use std::path::Path;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LedgerRow {
    pub ts: String,
    pub unit: String,
    pub stage: String,
    pub status: String,
    pub attempt: u8,
    pub duration_ms: u64,
    pub model: String,
    pub prompt_hash: String,
    pub out_sha256: String,
    pub counts: serde_json::Value,
    pub error: Option<String>,
}

impl LedgerRow {
    pub fn new(unit: impl Into<String>, stage: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            unit: unit.into(),
            stage: stage.into(),
            status: status.into(),
            attempt: 1,
            duration_ms: 0,
            model: String::new(),
            prompt_hash: String::new(),
            out_sha256: String::new(),
            counts: serde_json::Value::Null,
            error: None,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }

    pub fn with_counts(mut self, counts: serde_json::Value) -> Self {
        self.counts = counts;
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }
}

/// 1 行 1 JSON で追記する。同時 1 実行なのでロックは取らない。
pub fn append(path: &Path, row: &LedgerRow) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(row).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// 同じ `(unit, stage)` の**最後の**行が `done` なら true。
///
/// `failed` の後に `done` が来た場合を正しく扱うため、先頭ではなく末尾から見る。
pub fn is_done(path: &Path, unit: &str, stage: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<LedgerRow>(line).ok())
        .find(|row| row.unit == unit && row.stage == stage)
        .is_some_and(|row| row.status == "done")
}

/// 台帳を全部読む。壊れた行は飛ばす。S10 の `status` で使う。
pub fn read_all(path: &Path) -> Vec<LedgerRow> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<LedgerRow>(line).ok())
        .collect()
}


/// フェーズ・状態ごとの件数。
#[derive(Debug, Default, serde::Serialize)]
pub struct StageCounts {
    pub done: usize,
    pub failed: usize,
}

/// `status` の集計結果。
#[derive(Debug, serde::Serialize)]
pub struct StatusSummary {
    /// 台帳に出てくるユニット数（重複を除いた数）。
    pub units: usize,
    /// stage → 件数。**`BTreeMap` でキー順を決定的にする。**
    pub by_stage: std::collections::BTreeMap<String, StageCounts>,
    /// 失敗が残っているユニット（最後の行が failed のもの）。
    pub failed_units: Vec<String>,
}

/// 台帳を読んでフェーズ別に集計する。**書き込みも LLM も無し。**
///
/// 「最後の行が failed」だけを失敗として数える（後で成功したユニットを
/// 失敗に数えないため）。`is_done` と同じく末尾から見る。
pub fn summarize(path: &Path) -> StatusSummary {
    let rows = read_all(path);

    let mut units: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut by_stage: std::collections::BTreeMap<String, StageCounts> =
        std::collections::BTreeMap::new();
    let mut last_status: std::collections::BTreeMap<(&str, &str), &str> =
        std::collections::BTreeMap::new();

    for row in &rows {
        units.insert(row.unit.as_str());
        let entry = by_stage.entry(row.stage.clone()).or_default();
        match row.status.as_str() {
            "done" => entry.done += 1,
            "failed" => entry.failed += 1,
            _ => {}
        }
        last_status.insert((row.unit.as_str(), row.stage.as_str()), row.status.as_str());
    }

    // ユニットごとに「最後に失敗した stage があるか」を見る。
    let mut failed_units: Vec<String> = Vec::new();
    for unit in &units {
        let mut any_failed = false;
        for ((u, stage), status) in &last_status {
            if *u == *unit && *status == "failed" {
                let _ = stage;
                any_failed = true;
            }
        }
        if any_failed {
            failed_units.push((*unit).to_owned());
        }
    }

    StatusSummary {
        units: units.len(),
        by_stage,
        failed_units,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ac-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir.join("_ledger.jsonl")
    }

    #[test]
    fn append_writes_one_line_per_row() {
        let path = tmp_path("append");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "done")).expect("append 1");
        append(&path, &LedgerRow::new("2007-02/ja", "b", "done")).expect("append 2");

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn is_done_reads_the_last_row_for_the_pair() {
        let path = tmp_path("isdone");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "failed")).expect("append f");
        assert!(!is_done(&path, "2007-01/ja", "b"));

        append(&path, &LedgerRow::new("2007-01/ja", "b", "done")).expect("append d");
        assert!(is_done(&path, "2007-01/ja", "b"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn is_done_ignores_other_stages_and_units() {
        let path = tmp_path("stage");
        append(&path, &LedgerRow::new("2007-01/ja", "a", "done")).expect("append");
        assert!(!is_done(&path, "2007-01/ja", "b"));
        assert!(!is_done(&path, "2007-01/en", "a"));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn is_done_is_false_for_missing_file() {
        let path = tmp_path("missing").with_file_name("absent.jsonl");
        assert!(!is_done(&path, "2007-01/ja", "b"));
    }

    #[test]
    fn read_all_skips_broken_lines() {
        let path = tmp_path("broken");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "done")).expect("append");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        file.write_all(b"{not json\n").expect("write broken");
        drop(file);

        assert_eq!(read_all(&path).len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn summarize_counts_by_stage() {
        let path = tmp_path("summary");
        append(&path, &LedgerRow::new("2007-01/ja", "a", "done")).expect("1");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "done")).expect("2");
        append(&path, &LedgerRow::new("2007-01/ja", "c", "done")).expect("3");
        append(&path, &LedgerRow::new("2007-01/ja", "d", "done")).expect("4");
        append(&path, &LedgerRow::new("2007-02/ja", "a", "done")).expect("5");
        append(&path, &LedgerRow::new("2007-02/ja", "b", "failed")).expect("6");

        let s = summarize(&path);
        assert_eq!(s.units, 2);
        assert_eq!(s.by_stage["a"].done, 2);
        assert_eq!(s.by_stage["b"].done, 1);
        assert_eq!(s.by_stage["b"].failed, 1);
        assert_eq!(s.by_stage["c"].done, 1);
        assert_eq!(s.by_stage["d"].done, 1);
        assert_eq!(s.failed_units, vec!["2007-02/ja".to_owned()]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn summarize_clears_failure_after_later_success() {
        let path = tmp_path("recover");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "failed")).expect("1");
        append(&path, &LedgerRow::new("2007-01/ja", "b", "done")).expect("2");
        let s = summarize(&path);
        assert!(
            s.failed_units.is_empty(),
            "後で成功したら失敗に数えない: {:?}",
            s.failed_units
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn summarize_is_deterministic_in_key_order() {
        let path = tmp_path("order");
        append(&path, &LedgerRow::new("2007-01/ja", "d", "done")).expect("1");
        append(&path, &LedgerRow::new("2007-01/ja", "a", "done")).expect("2");
        let s = summarize(&path);
        let keys: Vec<&String> = s.by_stage.keys().collect();
        assert_eq!(keys, vec!["a", "d"], "BTreeMap でキー順が固定される");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
