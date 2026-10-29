mod assemble;
mod chunk;
mod cli;
mod config;
mod dedup;
mod discover;
mod fetch;
mod ledger;
mod map;
mod runner;
mod sanitize;
mod slice;
mod store;
mod unit;
mod verify;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Cmd};
use config::Config;
use unit::{iter_range, langs_of, parse_year_month, Lang, UnitKey};

/// `--from`/`--to` または `--unit` から対象ユニットを決める。
fn resolve_units(
    unit: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    lang: &str,
    limit: Option<usize>,
) -> Result<Vec<UnitKey>, String> {
    let langs = langs_of(lang)?;

    let mut units = if let Some(raw) = unit {
        vec![raw.parse::<UnitKey>()?]
    } else {
        let from_raw = from.ok_or("either --unit or --from/--to is required")?;
        let to_raw = to.ok_or("either --unit or --from/--to is required")?;
        let (fy, fm) = parse_year_month(from_raw)?;
        let (ty, tm) = parse_year_month(to_raw)?;
        iter_range(
            UnitKey {
                year: fy,
                month: fm,
                lang: Lang::Ja,
            },
            UnitKey {
                year: ty,
                month: tm,
                lang: Lang::En,
            },
        )
        .collect()
    };

    units.retain(|u| langs.contains(&u.lang));
    if let Some(n) = limit {
        units.truncate(n);
    }
    Ok(units)
}

async fn run_discover(
    cfg: &Config,
    from: &str,
    to: &str,
    lang: &str,
    rediscover: bool,
    dry_run: bool,
    mode: &str,
) -> Result<bool, String> {
    // `--mode` を設定に反映する。設定ファイルの値を CLI で上書きする。
    let mut cfg = cfg.clone();
    match mode {
        "static" | "agent" => cfg.discover.mode = mode.to_owned(),
        other => return Err(format!("invalid --mode `{other}`: static or agent")),
    }
    let cfg = &cfg;
    let units = resolve_units(None, Some(from), Some(to), lang, None)?;
    if units.is_empty() {
        return Err("no units selected".to_owned());
    }
    for key in units {
        // --rediscover が付いていなければ既存の urls.json を尊重する。
        let plan = match runner::run_stage_a(cfg, &key, rediscover).await {
            Ok(plan) => plan,
            Err(e) => {
                eprintln!("[{key}] discover failed: {e}");
                return Ok(false);
            }
        };
        if dry_run {
            println!("{key}");
            for s in &plan.sources {
                println!("  p{:>3} {:<16} {}", s.priority, format!("{:?}", s.kind), s.url);
            }
        } else {
            eprintln!("[{key}] {} source(s)", plan.sources.len());
        }
    }
    Ok(true)
}

fn stage_wanted(stage: &str, wanted: &str) -> bool {
    stage == "all" || stage == wanted
}

/// 1 ユニットを、指定された段だけ順に回す。
///
/// 段は a → b → c → d の順に依存する。途中の段を指定した場合は、
/// その段が要求する入力を前段の成果物（`urls.json` / `docs/*.json` /
/// `dedup.json`）から読む。
async fn run_unit(
    cfg: &Config,
    key: &UnitKey,
    stage: &str,
    force: bool,
    rediscover: bool,
) -> Result<(), String> {
    let label = key.to_string();

    // --- A: 候補 URL ---
    //
    // **stage a の完了を台帳に残す。** S9 の Expand は「a 行が 2 本」で
    // 検証する。ここを書かないと再入の事実が追えず、再実行のたびに
    // discover をやり直してしまう。
    let mut plan = if stage_wanted(stage, "a") {
        let plan = runner::run_stage_a(cfg, key, rediscover).await.map_err(|e| {
            runner::record_failure(cfg, key, runner::STAGE_A, &e.to_string());
            format!("discover failed: {e}")
        })?;
        eprintln!("[{label}] discover {} source(s)", plan.sources.len());
        runner::record_stage_done(
            cfg,
            key,
            runner::STAGE_A,
            serde_json::json!({
                "sources": plan.sources.len(),
                "pass": "initial",
            }),
        )?;
        Some(plan)
    } else {
        None
    };

    // --- B: 取得と抽出 ---
    if stage_wanted(stage, "b") && !(!force && runner::is_finished(cfg, key, runner::STAGE_B)) {
        if plan.is_none() {
            // b を単独で回した場合も、a の成果物を作った事実を台帳に残す。
            let created = runner::run_stage_a(cfg, key, false)
                .await
                .map_err(|e| format!("discover failed: {e}"))?;
            let _ = runner::record_stage_done(
                cfg,
                key,
                runner::STAGE_A,
                serde_json::json!({
                    "sources": created.sources.len(),
                    "pass": "initial",
                }),
            );
            plan = Some(created);
        }
        let plan_ref = plan.as_ref().expect("plan was just ensured");
        let metrics = runner::run_stage_b(cfg, key, plan_ref).await.map_err(|message| {
            runner::record_failure(cfg, key, runner::STAGE_B, &message);
            message
        })?;
        runner::record(cfg, key, runner::STAGE_B, &metrics)?;
    } else if stage_wanted(stage, "b") {
        eprintln!("[{label}] skip b (ledger says done)");
    }

    // --- C: 重複除去 ---
    if stage_wanted(stage, "c") && !(!force && runner::is_finished(cfg, key, runner::STAGE_C)) {
        let result = runner::run_stage_c(cfg, key).await.map_err(|message| {
            runner::record_failure(cfg, key, runner::STAGE_C, &message);
            message
        })?;
        runner::record_stage_done(
            cfg,
            key,
            runner::STAGE_C,
            serde_json::json!({
                "clusters": result.stats.output,
                "merged": result.stats.merged,
                "llm_calls": result.stats.llm_calls,
            }),
        )?;
    } else if stage_wanted(stage, "c") {
        eprintln!("[{label}] skip c (ledger says done)");
    }

    // --- D: 組み立て（LLM 0 回） ---
    if stage_wanted(stage, "d") {
        let plan_ref = match plan {
            Some(p) => p,
            None => runner::run_stage_a(cfg, key, false)
                .await
                .map_err(|e| format!("discover failed: {e}"))?,
        };
        let outcome = runner::run_stage_d(cfg, key, &plan_ref, 0)
            .await
            .map_err(|message| {
                runner::record_failure(cfg, key, runner::STAGE_D, &message);
                message
            })?;

        match outcome {
            assemble::AssembleOutcome::Done(digest) => {
                runner::record_stage_done(
                    cfg,
                    key,
                    runner::STAGE_D,
                    serde_json::json!({
                        "items": digest.count,
                        "warnings": digest.coverage.warnings,
                    }),
                )?;
            }
            assemble::AssembleOutcome::NeedsExpand { count, min } => {
                // **Expand は unit あたり 1 回だけ。**
                // 実測 en 2000-04 は年記事に事件が 2 件しかなく、何度広げても
                // 30 件には届かない。「30 件無い月」は実在しうるので、
                // 再入後も足りなければ `below_min` を記録して成功扱にする。
                eprintln!("[{label}] needs expand ({count} < {min}); re-entering discover once");
                let expanded = runner::run_stage_a_expand(cfg, key).await.map_err(|e| {
                    runner::record_failure(cfg, key, runner::STAGE_A, &e.to_string());
                    format!("expand discover failed: {e}")
                })?;
                eprintln!("[{label}] expand yielded {} source(s)", expanded.sources.len());
                runner::record_stage_done(
                    cfg,
                    key,
                    runner::STAGE_A,
                    serde_json::json!({
                        "sources": expanded.sources.len(),
                        "pass": "expand",
                    }),
                )?;

                let retry = runner::run_stage_d_with(cfg, key, &expanded, 0, true)
                    .await
                    .map_err(|message| {
                        runner::record_failure(cfg, key, runner::STAGE_D, &message);
                        message
                    })?;

                match retry {
                    assemble::AssembleOutcome::Done(mut digest) => {
                        // `below_min` は `assemble_with(allow_below_min=true)` が
                        // 既に warnings に入れている。ここで足すと二重になる。
                        digest.coverage.expanded = true;
                        runner::write_digest(cfg, key, &digest)?;
                        runner::record_stage_done(
                            cfg,
                            key,
                            runner::STAGE_D,
                            serde_json::json!({
                                "items": digest.count,
                                "expanded": true,
                                "warnings": digest.coverage.warnings,
                            }),
                        )?;
                    }
                    assemble::AssembleOutcome::NeedsExpand { count, min } => {
                        // 2 回目も NeedsExpand を返す設計にはしない。
                        // ここに来たら `items.min` が高すぎる（検証で 999 を入れた場合）。
                        return Err(format!(
                            "still below min after expand: {count} < {min}"
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_collect(
    cfg: &Config,
    unit: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    lang: &str,
    stage: &str,
    force: bool,
    rediscover: bool,
    limit: Option<usize>,
) -> Result<bool, String> {
    let units = resolve_units(unit, from, to, lang, limit)?;
    if units.is_empty() {
        return Err("no units selected".to_owned());
    }

    let mut any_failed = false;
    let mut done = 0usize;

    for key in units {
        match run_unit(cfg, &key, stage, force, rediscover).await {
            Ok(()) => done += 1,
            Err(message) => {
                eprintln!("[{}] FAILED: {message}", key);
                any_failed = true;
            }
        }
    }

    eprintln!("done {done} unit(s), failed? {any_failed}");
    Ok(!any_failed)
}


/// `verify` サブコマンド。**読み取り専用。LLM も fetch も呼ばない。**
fn run_verify(
    cfg: &Config,
    unit: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    lang: &str,
    json: bool,
) -> Result<bool, String> {
    let units = resolve_units(unit, from, to, lang, None)?;
    if units.is_empty() {
        return Err("no units selected".to_owned());
    }

    let mut all_ok = true;
    let mut reports = Vec::with_capacity(units.len());

    for key in &units {
        match verify::verify_unit(cfg, key) {
            Ok(report) => {
                if !report.ok {
                    all_ok = false;
                }
                if !json {
                    let mark = if report.ok { "ok  " } else { "FAIL" };
                    println!(
                        "{mark} {} count={} null={:.0}% warnings={}",
                        report.unit,
                        report.count,
                        report.null_date_ratio * 100.0,
                        report.warnings.len()
                    );
                    for e in &report.errors {
                        println!("       - {e}");
                    }
                }
                reports.push(report);
            }
            Err(e) => {
                all_ok = false;
                if !json {
                    println!("FAIL {} {e}", key);
                }
                reports.push(verify::VerifyReport {
                    unit: key.to_string(),
                    ok: false,
                    count: 0,
                    null_date_ratio: 0.0,
                    warnings: vec![],
                    errors: vec![e.to_string()],
                });
            }
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&reports).map_err(|e| e.to_string())?
        );
    }
    Ok(all_ok)
}

/// `status` サブコマンド。台帳をフェーズ別に集計する。**読み取り専用。**
fn run_status(cfg: &Config, json: bool) -> Result<bool, String> {
    let path = store::ledger_path(cfg);
    let summary = ledger::summarize(&path);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?
        );
        return Ok(true);
    }

    println!("units: {}", summary.units);
    for (stage, counts) in &summary.by_stage {
        println!("  stage {stage}: done={} failed={}", counts.done, counts.failed);
    }
    if !summary.failed_units.is_empty() {
        println!("failed units ({}):", summary.failed_units.len());
        for unit in &summary.failed_units {
            println!("  {unit}");
        }
    }
    Ok(true)
}

fn dispatch(cli: Cli) -> Result<bool, String> {
    let cfg = if cli.config.exists() {
        config::load(Some(&cli.config))?
    } else {
        config::load_default()?
    };

    match cli.cmd {
        Cmd::Discover {
            from,
            to,
            lang,
            rediscover,
            dry_run,
            mode,
        } => tokio::runtime::Runtime::new()
            .map_err(|e| e.to_string())?
            .block_on(run_discover(&cfg, &from, &to, &lang, rediscover, dry_run, &mode)),
        Cmd::Collect {
            unit,
            from,
            to,
            lang,
            stage,
            force,
            rediscover,
            with_portal: _,
            limit,
        } => tokio::runtime::Runtime::new()
            .map_err(|e| e.to_string())?
            .block_on(run_collect(
                &cfg,
                unit.as_deref(),
                from.as_deref(),
                to.as_deref(),
                &lang,
                &stage,
                force,
                rediscover,
                limit,
            )),
        Cmd::Verify {
            unit,
            from,
            to,
            lang,
            json,
        } => run_verify(&cfg, unit.as_deref(), from.as_deref(), to.as_deref(), &lang, json),
        Cmd::Status { json } => run_status(&cfg, json),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::from(2)
        }
    }
}
