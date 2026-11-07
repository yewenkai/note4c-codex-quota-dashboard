use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use note4c_codex_quota_dashboard::quota_relay::{
    CodexAuthRegistry, CodexAuthUsage, PaidAccountQuota, QuotaRelayError, SyncConfig,
    apply_account_labels, build_manifest, publish_over_ssh, render_paid_accounts,
    validate_freshness, write_local_state,
};
use serde::Deserialize;

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(usage())?;
    match command.as_str() {
        "preview" => {
            let registry = value_after(&mut args, "--registry")?;
            let output_bin = value_after(&mut args, "--output-bin")?;
            let output_png = value_after(&mut args, "--output-png")?;
            let generated_at = match optional_generated_at(&mut args)? {
                Some(value) => value,
                None => now()?,
            };
            ensure_no_args(args)?;
            let registry = CodexAuthRegistry::read(registry)?;
            let accounts = registry.cached_quotas(3)?;
            let frame = render_paid_accounts(&accounts, generated_at, registry.active_is_free())?;
            fs::write(&output_bin, frame.packed_bytes())?;
            fs::write(&output_png, frame.png_bytes()?)?;
            println!(
                "{}  {}  {}",
                frame.sha256(),
                output_bin.display(),
                output_png.display()
            );
        }
        "sync" => {
            let config_path = value_after(&mut args, "--config")?;
            let mut refresh = false;
            let mut wait_for_lock = false;
            for argument in args {
                match argument.as_str() {
                    "--refresh" => refresh = true,
                    "--wait" => wait_for_lock = true,
                    _ => return Err(format!("未知参数：{argument}").into()),
                }
            }
            sync(&config_path, refresh, wait_for_lock)?;
        }
        _ => return Err(usage().into()),
    }
    Ok(())
}

fn sync(
    config_path: &Path,
    refresh: bool,
    wait_for_lock: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let config: SyncConfig = serde_json::from_slice(&fs::read(config_path)?)?;
    fs::create_dir_all(&config.state_directory)?;
    let lock = config.state_directory.join("sync.lock");
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock)?;
    let lock_result = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock_file),
            libc::LOCK_EX | if wait_for_lock { 0 } else { libc::LOCK_NB },
        )
    };
    if lock_result != 0 {
        if wait_for_lock {
            return Err(std::io::Error::last_os_error().into());
        }
        println!("另一个同步进程正在运行，本次跳过");
        return Ok(());
    }

    let mut registry = CodexAuthRegistry::read(&config.registry_path)?;
    let started_at = now()?;
    let mut accounts = if refresh && registry.schema_version == 4 {
        let (latest_registry, quotas) = refresh_from_cli(&config, started_at)?;
        registry = latest_registry;
        quotas
    } else {
        if refresh {
            let emails = registry
                .paid_accounts(config.expected_paid_accounts)?
                .iter()
                .map(|account| account.email.clone())
                .collect::<Vec<_>>();
            refresh_legacy_accounts(&config, &emails)?;
            registry = CodexAuthRegistry::read(&config.registry_path)?;
        }
        let cached = registry.cached_quotas(config.expected_paid_accounts)?;
        validate_freshness(
            &cached,
            if refresh {
                started_at
            } else {
                started_at.saturating_sub(config.maximum_cache_age_seconds)
            },
        )?;
        cached
    };

    apply_account_labels(&mut accounts, &config.account_labels)?;
    let generated_at = now()?;
    let frame = render_paid_accounts(&accounts, generated_at, registry.active_is_free())?;
    let manifest = build_manifest(&frame, generated_at);
    write_local_state(&config.state_directory, &frame, &manifest)?;
    if let Some(publisher) = &config.publisher {
        publish_over_ssh(publisher, &config.state_directory, &manifest)?;
    }
    println!(
        "已生成并{} revision={}（{} 个付费账号）",
        if config.publisher.is_some() {
            "发布"
        } else {
            "暂存"
        },
        manifest.revision,
        accounts.len()
    );
    Ok(())
}

fn retry_refresh<T>(
    attempts: usize,
    mut refresh: impl FnMut() -> Result<T, QuotaRelayError>,
) -> Result<T, QuotaRelayError> {
    if attempts == 0 || attempts > 10 {
        return Err(QuotaRelayError::InvalidRegistry(
            "refreshAttempts 必须在 1–10 之间".into(),
        ));
    }
    for _ in 1..attempts {
        if let Ok(value) = refresh() {
            return Ok(value);
        }
        thread::sleep(Duration::from_secs(2));
    }
    refresh()
}

fn refresh_from_cli(
    config: &SyncConfig,
    observed_at: i64,
) -> Result<(CodexAuthRegistry, Vec<PaidAccountQuota>), QuotaRelayError> {
    let codex_home = config
        .registry_path
        .parent()
        .filter(|path| {
            path.ends_with("accounts") && config.registry_path.ends_with("registry.json")
        })
        .and_then(Path::parent)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            QuotaRelayError::InvalidRegistry(
                "新版 registryPath 必须是 CODEX_HOME/accounts/registry.json 的绝对路径".into(),
            )
        })?;
    retry_refresh(config.refresh_attempts, || {
        let output = Command::new(&config.codex_auth_bin)
            .args(["list", "--api", "--json"])
            .env("CODEX_HOME", codex_home)
            .output()?;
        if !output.status.success() {
            let details = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
            let code = details
                .as_ref()
                .and_then(|value| value["error"]["code"].as_str())
                .unwrap_or("请确认 codexAuthBin 指向新版 codex-auth");
            return Err(QuotaRelayError::Refresh(format!(
                "codex-auth 退出状态 {}（{code}）",
                output.status
            )));
        }
        let registry = CodexAuthRegistry::read(&config.registry_path)?;
        let quotas = live_quotas(
            &output.stdout,
            &registry,
            config.expected_paid_accounts,
            observed_at,
        )?;
        Ok((registry, quotas))
    })
}

#[derive(Deserialize)]
struct CliList {
    schema_version: u32,
    command: String,
    accounts: Vec<CliAccount>,
}

#[derive(Deserialize)]
struct CliAccount {
    account_key: String,
    plan: Option<String>,
    usage: CliUsage,
}

#[derive(Deserialize)]
struct CliUsage {
    source: String,
    refresh: CliRefresh,
    #[serde(flatten)]
    snapshot: CodexAuthUsage,
}

#[derive(Deserialize)]
struct CliRefresh {
    requested: bool,
    method: Option<String>,
    status: String,
    http_status: Option<u16>,
}

fn live_quotas(
    json: &[u8],
    registry: &CodexAuthRegistry,
    expected_count: usize,
    observed_at: i64,
) -> Result<Vec<PaidAccountQuota>, QuotaRelayError> {
    let result: CliList = serde_json::from_slice(json)?;
    if result.schema_version != 1 || result.command != "list" {
        return Err(QuotaRelayError::Refresh(
            "不支持的 codex-auth list JSON 格式".into(),
        ));
    }
    registry
        .paid_accounts(expected_count)?
        .into_iter()
        .map(|account| {
            let mut matches = result
                .accounts
                .iter()
                .filter(|row| row.account_key == account.account_key);
            let row = matches
                .next()
                .filter(|_| matches.next().is_none())
                .ok_or_else(|| {
                    QuotaRelayError::Refresh(format!("{} 的账号标识缺失或重复", account.email))
                })?;
            let usage = &row.usage;
            let refresh = &usage.refresh;
            if !refresh.requested
                || refresh.method.as_deref() != Some("api")
                || refresh.status != "ok"
                || usage.source != "api"
            {
                let status = refresh
                    .http_status
                    .map_or_else(|| refresh.status.clone(), |code| format!("HTTP {code}"));
                return Err(QuotaRelayError::Refresh(format!(
                    "{} 未取得实时额度（{status}）",
                    account.email
                )));
            }
            if row.plan.as_deref() != account.plan() {
                return Err(QuotaRelayError::Refresh(format!(
                    "{} 的账号计划已变化",
                    account.email
                )));
            }
            // The CLI can confirm an unchanged snapshot without advancing its stored updated_at.
            account.quota(&usage.snapshot, observed_at, registry.is_active(account))
        })
        .collect()
}

fn refresh_legacy_accounts(
    config: &SyncConfig,
    expected_emails: &[String],
) -> Result<(), QuotaRelayError> {
    let mut successful = HashSet::new();
    retry_refresh(config.refresh_attempts, || {
        let output = Command::new(&config.codex_auth_bin)
            .arg("list")
            .arg("--api")
            .arg("--debug")
            .env("CODEX_AUTH_REGISTRY_PATH", &config.registry_path)
            .output()?;
        if !output.status.success() {
            let detail = last_nonempty_line(&output.stderr)
                .or_else(|| last_nonempty_line(&output.stdout))
                .unwrap_or_else(|| output.status.to_string());
            return Err(QuotaRelayError::Refresh(format!("刷新器报告：{detail}")));
        }
        let mut trace = String::from_utf8_lossy(&output.stderr).into_owned();
        trace.push('\n');
        trace.push_str(&String::from_utf8_lossy(&output.stdout));
        successful.extend(successful_paid_refreshes(&trace));
        let missing = expected_emails
            .iter()
            .filter(|email| !successful.contains(*email))
            .map(String::as_str)
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(QuotaRelayError::Refresh(format!(
                "以下付费账号未取得实时额度：{}",
                missing.join(", ")
            )))
        }
    })
}

fn last_nonempty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn successful_paid_refreshes(trace: &str) -> HashSet<String> {
    trace
        .lines()
        .filter(|line| line.contains("status=200 result=usage-windows"))
        .filter_map(|line| line.split("response usage: ").nth(1))
        .filter_map(|line| line.split(" status=").next())
        .filter_map(|identity| identity.split(" | ").next())
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
fn validate_paid_refresh<'a>(
    trace: &str,
    expected_emails: impl IntoIterator<Item = &'a str>,
) -> Result<(), QuotaRelayError> {
    let successful = successful_paid_refreshes(trace);

    let missing = expected_emails
        .into_iter()
        .filter(|email| !successful.contains(*email))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(QuotaRelayError::InvalidRegistry(format!(
            "以下付费账号未取得成功的实时额度响应：{}；拒绝覆盖现有画面",
            missing.join(", ")
        )))
    }
}

fn value_after(
    args: &mut impl Iterator<Item = String>,
    expected: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let actual = args.next().ok_or(usage())?;
    if actual != expected {
        return Err(format!("预期参数 {expected}，收到 {actual}").into());
    }
    Ok(PathBuf::from(
        args.next().ok_or(format!("{expected} 缺少值"))?,
    ))
}

fn ensure_no_args(
    mut args: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(argument) = args.next() {
        Err(format!("未知参数：{argument}").into())
    } else {
        Ok(())
    }
}

fn optional_generated_at(
    args: &mut impl Iterator<Item = String>,
) -> Result<Option<i64>, Box<dyn std::error::Error>> {
    let Some(argument) = args.next() else {
        return Ok(None);
    };
    if argument != "--generated-at" {
        return Err(format!("未知参数：{argument}").into());
    }
    let value = args
        .next()
        .ok_or("--generated-at 缺少 Unix 时间戳")?
        .parse::<i64>()?;
    if value <= 0 {
        return Err("--generated-at 必须为正整数".into());
    }
    Ok(Some(value))
}

fn now() -> Result<i64, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

fn usage() -> &'static str {
    "用法：\n  codex-note4c-relay preview --registry PATH --output-bin PATH --output-png PATH [--generated-at UNIX_SECONDS]\n  codex-note4c-relay sync --config PATH [--refresh] [--wait]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn live_refresh_requires_api_success_for_each_workspace_key() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/registry.sample.json"))
                .unwrap();
        fixture["accounts"][1]["email"] = fixture["accounts"][0]["email"].clone();
        fixture["accounts"][3]["last_usage"] = fixture["accounts"][2]["last_usage"].clone();
        let registry: CodexAuthRegistry = serde_json::from_value(fixture.clone()).unwrap();
        let rows = fixture["accounts"].as_array().unwrap().iter().map(|account| {
            let mut usage = account["last_usage"].clone();
            let paid = account["plan"] != "free";
            usage["source"] = json!(if paid { "api" } else { "cache" });
            usage["updated_at"] = json!(1);
            usage["refresh"] = json!({
                "requested": true,
                "method": "api",
                "status": if paid { "ok" } else { "http_error" }
            });
            json!({ "account_key": account["account_key"], "plan": account["plan"], "usage": usage })
        }).collect::<Vec<_>>();
        let mut list = json!({ "schema_version": 1, "command": "list", "accounts": rows });
        let observed_at = 1_790_000_000;
        let quotas = live_quotas(
            &serde_json::to_vec(&list).unwrap(),
            &registry,
            3,
            observed_at,
        )
        .unwrap();
        assert!(registry.active_is_free());
        assert!(quotas.iter().all(|quota| !quota.active));
        assert_eq!(quotas[0].observed_at, observed_at);
        assert_eq!(quotas[0].five_hour_remaining_percent, 76);
        assert_eq!(quotas[0].credits.as_deref(), Some("0"));
        assert_eq!(quotas[0].reset_credits, Some(1));
        assert_eq!(quotas[1].credits, None);
        assert_eq!(quotas[1].reset_credits, Some(0));

        list["accounts"][1]["usage"]["source"] = json!("cache");
        list["accounts"][1]["usage"]["refresh"]["status"] = json!("http_error");
        list["accounts"][1]["usage"]["refresh"]["http_status"] = json!(503);
        let error = live_quotas(
            &serde_json::to_vec(&list).unwrap(),
            &registry,
            3,
            observed_at,
        )
        .unwrap_err();
        assert!(error.to_string().contains("HTTP 503"));
    }

    #[test]
    fn refresh_accepts_successful_paid_accounts_and_ignores_free_failures() {
        let trace = r#"
[debug] response usage: biz@example.com | Free status=401 result=http-response
[debug] response usage: biz@example.com | team status=200 result=usage-windows
[debug] response usage: plus@example.com status=200 result=usage-windows
"#;
        validate_paid_refresh(trace, ["biz@example.com", "plus@example.com"]).unwrap();
    }

    #[test]
    fn refresh_rejects_any_paid_account_without_a_live_success() {
        let trace = "[debug] response usage: biz@example.com status=200 result=usage-windows";
        let error =
            validate_paid_refresh(trace, ["biz@example.com", "plus@example.com"]).unwrap_err();
        assert!(error.to_string().contains("plus@example.com"));
    }

    #[test]
    fn last_nonempty_line_uses_the_final_diagnostic() {
        let trace = b"first\n\nfinal error\n";
        assert_eq!(last_nonempty_line(trace).as_deref(), Some("final error"));
    }
}
