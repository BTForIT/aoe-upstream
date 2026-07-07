//! Pane watchdog: a daemon background loop that periodically captures each
//! registered session's tmux pane tail, classifies it against the
//! user-configured rules in `[watchdog]` (see [`crate::pane_rules`]), and
//! runs the matching rule's command.
//!
//! The watchdog only observes panes and runs user commands; it never mutates
//! session state itself. Everything is fail-open: a session whose pane
//! cannot be captured is skipped, a command that fails or times out is
//! logged, and the loop keeps ticking.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;
use crate::pane_rules::CompiledRule;

use super::AppState;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(180);
const MIN_INTERVAL: Duration = Duration::from_secs(10);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawn the watchdog loop if the config enables it. No-op when the daemon
/// is read-only, `[watchdog]` is disabled, or no rule compiles.
pub fn spawn(state: Arc<AppState>) {
    if state.read_only {
        return;
    }
    let cfg = crate::session::Config::load_or_warn().watchdog;
    if cfg.disabled {
        return;
    }
    let rules = Arc::new(crate::pane_rules::compile(&cfg.rules));
    if rules.is_empty() {
        return;
    }
    let interval = cfg
        .interval_secs
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_INTERVAL)
        .max(MIN_INTERVAL);
    tracing::info!(
        target: "server.pane_watchdog",
        rules = rules.len(),
        interval_secs = interval.as_secs(),
        "pane watchdog enabled"
    );

    let shutdown = state.shutdown.clone();
    crate::task_util::spawn_supervised(
        "server.pane_watchdog",
        crate::task_util::PanicPolicy::Log,
        async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Per-(session, rule) last-fired times, enforcing each rule's
            // cooldown so a pane that stays in a matching state does not run
            // its command every tick.
            let mut last_fired: HashMap<(String, String), Instant> = HashMap::new();
            // Swallow the immediate first tick; sessions restored during
            // daemon startup should settle before the first scan.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => tick(&state, &rules, &mut last_fired).await,
                    _ = shutdown.cancelled() => break,
                }
            }
        },
    );
}

struct PaneMatch {
    session_id: String,
    title: String,
    profile: String,
    rule_name: String,
    kind: String,
    command: Option<String>,
    cooldown: Duration,
}

async fn tick(
    state: &Arc<AppState>,
    rules: &Arc<Vec<CompiledRule>>,
    last_fired: &mut HashMap<(String, String), Instant>,
) {
    let file_watch = state.file_watch.clone();
    let scan_rules = rules.clone();
    let matches =
        match tokio::task::spawn_blocking(move || scan_panes(&file_watch, &scan_rules)).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(target: "server.pane_watchdog", error = %e, "pane scan task failed");
                return;
            }
        };

    // Entries past their rule's cooldown can never suppress again; drop them
    // so the map stays bounded across session churn.
    last_fired.retain(|(_, rule_name), fired| {
        rules
            .iter()
            .any(|r| r.name == *rule_name && fired.elapsed() < r.cooldown)
    });

    for m in matches {
        let key = (m.session_id.clone(), m.rule_name.clone());
        if let Some(fired) = last_fired.get(&key) {
            if fired.elapsed() < m.cooldown {
                continue;
            }
        }
        last_fired.insert(key, Instant::now());
        tracing::info!(
            target: "server.pane_watchdog",
            session = %m.session_id,
            title = %m.title,
            rule = %m.rule_name,
            kind = %m.kind,
            "pane rule matched"
        );
        if m.command.is_some() {
            run_command(m);
        }
    }
}

/// Capture and classify every registered non-structured session's pane tail.
/// Blocking (tmux subprocesses + storage reads); run under `spawn_blocking`.
fn scan_panes(file_watch: &Arc<FileWatchService>, rules: &[CompiledRule]) -> Vec<PaneMatch> {
    let instances = match super::load_all_instances(file_watch) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                target: "server.pane_watchdog",
                error = %e,
                "load_all_instances failed; skipping tick"
            );
            return Vec::new();
        }
    };
    // Rule windows count non-empty lines, so capture more raw lines than the
    // widest window to leave room for blanks and TUI padding.
    let capture_lines = crate::pane_rules::max_tail_lines(rules)
        .saturating_mul(2)
        .max(50);
    instances
        .iter()
        .filter(|inst| !inst.is_structured())
        .filter_map(|inst| {
            let sess = inst.tmux_session().ok()?;
            if !sess.exists() {
                return None;
            }
            let content = sess.capture_pane(capture_lines).ok()?;
            let rule = crate::pane_rules::classify(&content, rules)?;
            Some(PaneMatch {
                session_id: inst.id.clone(),
                title: inst.title.clone(),
                profile: inst.source_profile.clone(),
                rule_name: rule.name.clone(),
                kind: rule.kind.clone(),
                command: rule.command.clone(),
                cooldown: rule.cooldown,
            })
        })
        .collect()
}

/// Run a matched rule's command via `sh -c` with the match context in the
/// environment. Detached from the tick loop so a slow command cannot stall
/// scanning; killed after [`COMMAND_TIMEOUT`].
fn run_command(m: PaneMatch) {
    let command = m.command.clone().expect("checked by caller");
    crate::task_util::spawn_supervised(
        "server.pane_watchdog.command",
        crate::task_util::PanicPolicy::Log,
        async move {
            let output = tokio::time::timeout(
                COMMAND_TIMEOUT,
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .env("AOE_RULE_NAME", &m.rule_name)
                    .env("AOE_RULE_KIND", &m.kind)
                    .env("AOE_SESSION_ID", &m.session_id)
                    .env("AOE_SESSION_TITLE", &m.title)
                    .env("AOE_SESSION_PROFILE", &m.profile)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            match output {
                Err(_) => {
                    tracing::warn!(
                        target: "server.pane_watchdog",
                        rule = %m.rule_name,
                        session = %m.session_id,
                        timeout_secs = COMMAND_TIMEOUT.as_secs(),
                        "rule command timed out; killed"
                    );
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        target: "server.pane_watchdog",
                        rule = %m.rule_name,
                        session = %m.session_id,
                        error = %e,
                        "rule command failed to run"
                    );
                }
                Ok(Ok(out)) if !out.status.success() => {
                    tracing::warn!(
                        target: "server.pane_watchdog",
                        rule = %m.rule_name,
                        session = %m.session_id,
                        status = %out.status,
                        stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                        "rule command exited non-zero"
                    );
                }
                Ok(Ok(_)) => {
                    tracing::debug!(
                        target: "server.pane_watchdog",
                        rule = %m.rule_name,
                        session = %m.session_id,
                        "rule command completed"
                    );
                }
            }
        },
    );
}
