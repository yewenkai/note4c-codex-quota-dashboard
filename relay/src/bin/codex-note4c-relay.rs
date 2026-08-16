use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use note4c_codex_quota_dashboard::quota_relay::{
    QuotaRelayError, SyncConfig, apply_account_labels, build_manifest, publish_over_ssh,
    read_paid_accounts, render_paid_accounts, validate_freshness, write_local_state,
};

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
            let accounts = read_paid_accounts(registry, 3)?;
            let frame = render_paid_accounts(&accounts, generated_at)?;
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

    let accounts_before_refresh =
        read_paid_accounts(&config.registry_path, config.expected_paid_accounts)?;
    let started_at = now()?;
    if refresh {
        refresh_paid_accounts(
            &config.codex_auth_bin,
            &config.registry_path,
            config.refresh_attempts,
            &accounts_before_refresh
                .iter()
                .map(|account| account.email.clone())
                .collect::<Vec<_>>(),
        )?;
    }

    let mut accounts = read_paid_accounts(&config.registry_path, config.expected_paid_accounts)?;
    if !refresh {
        validate_freshness(
            &accounts,
            started_at.saturating_sub(config.maximum_cache_age_seconds),
        )?;
    }
    apply_account_labels(&mut accounts, &config.account_labels)?;
    let frame = render_paid_accounts(&accounts, started_at)?;
    let manifest = build_manifest(&frame, started_at);
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

fn refresh_paid_accounts(
    codex_auth_bin: &Path,
    registry_path: &Path,
    attempts: usize,
    expected_emails: &[String],
) -> Result<(), QuotaRelayError> {
    if attempts == 0 || attempts > 10 {
        return Err(QuotaRelayError::InvalidRegistry(
            "refreshAttempts 必须在 1–10 之间".into(),
        ));
    }
    let mut successful = HashSet::new();
    let mut last_failure = None;
    for attempt in 0..attempts {
        let output = Command::new(codex_auth_bin)
            .arg("list")
            .arg("--api")
            .arg("--debug")
            .env("CODEX_AUTH_REGISTRY_PATH", registry_path)
            .output()?;
        if output.status.success() {
            let mut trace = String::from_utf8_lossy(&output.stderr).into_owned();
            trace.push('\n');
            trace.push_str(&String::from_utf8_lossy(&output.stdout));
            successful.extend(successful_paid_refreshes(&trace));
        } else {
            last_failure =
                last_nonempty_line(&output.stderr).or_else(|| last_nonempty_line(&output.stdout));
        }
        if expected_emails
            .iter()
            .all(|email| successful.contains(email))
        {
            return Ok(());
        }
        if attempt + 1 < attempts {
            thread::sleep(Duration::from_secs(2));
        }
    }

    let missing = expected_emails
        .iter()
        .filter(|email| !successful.contains(*email))
        .cloned()
        .collect::<Vec<_>>();
    let message = if let Some(detail) = last_failure {
        format!("刷新器报告：{detail}；拒绝覆盖现有画面")
    } else {
        format!(
            "以下付费账号连续 {attempts} 次未取得成功的实时额度响应：{}；拒绝覆盖现有画面",
            missing.join(", ")
        )
    };
    Err(QuotaRelayError::InvalidRegistry(message))
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
