use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::{metadata, utils};

const RTK_OPERATIONAL_COMMAND_BYPASS_ENV: &str = "RTK_BYPASS_OPERATIONAL_COMMAND_SHIMS";
const RTK_RECURSION_DEPTH_ENV: &str = "RTK_RECURSION_DEPTH";
const RTK_RECURSION_DEPTH_LIMIT: u32 = 32;
const CODEX_THREAD_ID_ENV: &str = "CODEX_THREAD_ID";
const CODEX_SANDBOX_ENV: &str = "CODEX_SANDBOX";

pub(crate) fn operational_command_name_from_argv0(argv0: &OsStr) -> Option<String> {
    let basename = Path::new(argv0).file_name()?.to_string_lossy();
    let trimmed = basename.strip_suffix(".exe").unwrap_or(&basename);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn build_parse_argv(raw_argv: &[OsString]) -> Vec<OsString> {
    if raw_argv.is_empty() {
        return vec![OsString::from("rtk")];
    }

    let Some(operational_command) = operational_command_name_from_argv0(&raw_argv[0]) else {
        return raw_argv.to_vec();
    };

    if !metadata::is_shim_eligible_top_level_command(&operational_command) {
        return raw_argv.to_vec();
    }

    let mut parse_argv = Vec::with_capacity(raw_argv.len() + 1);
    parse_argv.push(OsString::from("rtk"));
    parse_argv.push(OsString::from(operational_command));
    parse_argv.extend(raw_argv.iter().skip(1).cloned());
    parse_argv
}

fn shim_operational_command_from_argv(raw_argv: &[OsString]) -> Option<String> {
    let argv0 = raw_argv.first()?;
    let operational_command = operational_command_name_from_argv0(argv0)?;
    metadata::is_shim_eligible_top_level_command(&operational_command)
        .then_some(operational_command)
}

fn is_codex_session_with<F>(mut get_env: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    [CODEX_THREAD_ID_ENV, CODEX_SANDBOX_ENV]
        .into_iter()
        .any(|name| get_env(name).is_some_and(|value| !value.trim().is_empty()))
}

fn is_codex_session() -> bool {
    is_codex_session_with(|name| std::env::var(name).ok())
}

fn current_recursion_depth() -> u32 {
    std::env::var(RTK_RECURSION_DEPTH_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0)
}

fn install_child_bypass_env(depth: u32) {
    std::env::set_var(RTK_OPERATIONAL_COMMAND_BYPASS_ENV, "1");
    std::env::set_var(RTK_RECURSION_DEPTH_ENV, (depth + 1).to_string());
}

fn should_exec_native_operational_command(
    raw_argv: &[OsString],
    bypass_enabled: bool,
    codex_session: bool,
) -> Option<String> {
    let operational_command = shim_operational_command_from_argv(raw_argv)?;
    if bypass_enabled || !codex_session {
        Some(operational_command)
    } else {
        None
    }
}

fn maybe_exec_native_operational_command(raw_argv: &[OsString]) -> Result<bool> {
    let bypass_enabled =
        std::env::var(RTK_OPERATIONAL_COMMAND_BYPASS_ENV).unwrap_or_default() == "1";
    let codex_session = is_codex_session();
    let Some(operational_command) =
        should_exec_native_operational_command(raw_argv, bypass_enabled, codex_session)
    else {
        return Ok(false);
    };

    let mut cmd = utils::native_command(&operational_command).with_context(|| {
        format!(
            "Failed to resolve native command for '{}'",
            operational_command
        )
    })?;
    cmd.args(raw_argv.iter().skip(1))
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute native '{}'", operational_command))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(true)
}

pub(crate) fn prepare_runtime_parse_argv() -> Result<Option<Vec<OsString>>> {
    let raw_argv: Vec<OsString> = std::env::args_os().collect();
    let recursion_depth = current_recursion_depth();
    if recursion_depth >= RTK_RECURSION_DEPTH_LIMIT {
        anyhow::bail!(
            "Detected recursive operational_command-shim invocation (depth={}). Refusing to continue.",
            recursion_depth
        );
    }

    if maybe_exec_native_operational_command(&raw_argv)? {
        return Ok(None);
    }

    // Child subprocesses should bypass operational_command rewrite and resolve native commands directly.
    install_child_bypass_env(recursion_depth);

    Ok(Some(build_parse_argv(&raw_argv)))
}

pub(crate) fn should_block_fallback_for_excluded_shim_command(parse_argv: &[OsString]) -> bool {
    let Some(operational_command) = parse_argv
        .first()
        .and_then(|s| operational_command_name_from_argv0(s))
    else {
        return false;
    };

    metadata::is_supported_top_level_command(&operational_command)
        && !metadata::is_shim_eligible_top_level_command(&operational_command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_argv(argv: &[&str]) -> Vec<OsString> {
        argv.iter().map(OsString::from).collect()
    }

    #[test]
    fn test_is_codex_session_with_thread_id() {
        assert!(is_codex_session_with(|name| match name {
            CODEX_THREAD_ID_ENV => Some("thread_123".to_string()),
            _ => None,
        }));
    }

    #[test]
    fn test_is_codex_session_with_sandbox() {
        assert!(is_codex_session_with(|name| match name {
            CODEX_SANDBOX_ENV => Some("seatbelt".to_string()),
            _ => None,
        }));
    }

    #[test]
    fn test_is_codex_session_without_signals() {
        assert!(!is_codex_session_with(|_| None));
        assert!(!is_codex_session_with(|name| match name {
            CODEX_THREAD_ID_ENV => Some("   ".to_string()),
            CODEX_SANDBOX_ENV => Some(String::new()),
            _ => None,
        }));
    }

    #[test]
    fn test_non_codex_shim_invocation_execs_native() {
        let raw = os_argv(&["git", "status"]);
        assert_eq!(
            should_exec_native_operational_command(&raw, false, false).as_deref(),
            Some("git")
        );
    }

    #[test]
    fn test_codex_shim_invocation_stays_in_rtk_path() {
        let raw = os_argv(&["git", "status"]);
        assert_eq!(
            should_exec_native_operational_command(&raw, false, true),
            None
        );
    }

    #[test]
    fn test_bypass_env_execs_native_even_in_codex() {
        let raw = os_argv(&["git", "status"]);
        assert_eq!(
            should_exec_native_operational_command(&raw, true, true).as_deref(),
            Some("git")
        );
    }

    #[test]
    fn test_non_shim_invocation_never_execs_native_directly() {
        let raw = os_argv(&["rtk", "git", "status"]);
        assert_eq!(
            should_exec_native_operational_command(&raw, false, false),
            None
        );
    }
}
