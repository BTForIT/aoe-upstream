//! Configurable pane-text rules: regex patterns that classify a captured
//! tmux pane tail into named events.
//!
//! This is the detection engine behind the daemon's pane watchdog
//! (`server::pane_watchdog`). Rules are pure data, declared in config as
//! `[[watchdog.rules]]` entries:
//!
//! ```toml
//! [watchdog]
//! interval_secs = 180
//!
//! [[watchdog.rules]]
//! name = "usage-cap"
//! kind = "cap"
//! pattern = '(?i)^usage limit reached'
//! negative = ['(?i)not your usage limit']
//! tail_lines = 30
//! command = "notify-operator.sh"
//! ```
//!
//! Everything is fail-open: an invalid pattern logs and drops that rule at
//! compile time; the rest keep working. See `docs/guides/pane-rules.md`.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Where a rule's pattern is applied within the pane tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleScope {
    /// Match each non-empty line of the tail window individually.
    #[default]
    Line,
    /// Match once against the tail window joined with newlines (for
    /// multi-line prompts like device-code sign-in blocks).
    Window,
}

fn default_tail_lines() -> usize {
    15
}

fn default_true() -> bool {
    true
}

fn default_cooldown_secs() -> u64 {
    30 * 60
}

/// One declarative pane-text rule, as written in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRuleConfig {
    /// Unique-ish label used in logs and exported to the rule command as
    /// `AOE_RULE_NAME`.
    pub name: String,
    /// Free-form category label ("cap", "auth", ...), exported to the rule
    /// command as `AOE_RULE_KIND`.
    #[serde(default)]
    pub kind: String,
    /// Regex tried against each line ([`RuleScope::Line`]) or the joined
    /// tail window ([`RuleScope::Window`]). Case-sensitive unless the
    /// pattern opts into `(?i)`.
    pub pattern: String,
    /// Negative-guard regexes: text they match can never fire this rule
    /// (applied to the raw line or window before the pattern).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub negative: Vec<String>,
    /// How many non-empty lines from the live edge of the pane are in
    /// scope. Small windows keep stale scrollback (a finished sign-in
    /// flow, a recovered error) from firing.
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
    #[serde(default)]
    pub scope: RuleScope,
    /// Strip leading decoration (selector glyphs, bullets) and a `1.` or
    /// `2)` option enumerator from each line before matching, so anchored
    /// patterns survive TUI chrome. Line scope only.
    #[serde(default = "default_true")]
    pub strip_decoration: bool,
    /// Evaluation order: lower fires first when several rules match.
    #[serde(default)]
    pub priority: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Shell command run when the rule matches, with `AOE_RULE_NAME`,
    /// `AOE_RULE_KIND`, `AOE_SESSION_ID`, and `AOE_SESSION_TITLE` in its
    /// environment. Omit to only log matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Floor in seconds between firings of this rule for the same session,
    /// so a pane that stays blocked does not run the command every tick.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

/// Watchdog section of the user config (`[watchdog]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatchdogConfig {
    /// Disable the pane watchdog without deleting the rules.
    #[serde(default)]
    pub disabled: bool,
    /// Scan interval in seconds (minimum 10; default 180).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PaneRuleConfig>,
}

/// A rule with its regexes compiled, ready to run against pane text.
#[derive(Debug)]
pub struct CompiledRule {
    pub name: String,
    pub kind: String,
    pattern: Regex,
    negative: Vec<Regex>,
    tail_lines: usize,
    scope: RuleScope,
    strip_decoration: bool,
    pub priority: u32,
    pub command: Option<String>,
    pub cooldown: std::time::Duration,
}

/// Compile a rule list, dropping disabled entries and (with a warning) any
/// rule whose pattern or guards fail to parse, plus later duplicates of a
/// `name` already seen — the watchdog's cooldown map is keyed by rule name,
/// so duplicates would share (and corrupt) each other's cooldown state.
/// The result is sorted by `priority` (stable, so config order breaks ties).
pub fn compile(rules: &[PaneRuleConfig]) -> Vec<CompiledRule> {
    let mut seen_names = std::collections::HashSet::new();
    let mut compiled: Vec<CompiledRule> = rules
        .iter()
        .filter(|r| r.enabled)
        .filter_map(|r| {
            if !seen_names.insert(r.name.clone()) {
                tracing::warn!(
                    target: "pane_rules",
                    rule = %r.name,
                    "duplicate rule name; rule dropped"
                );
                return None;
            }
            let pattern = match Regex::new(&r.pattern) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        target: "pane_rules",
                        rule = %r.name,
                        error = %e,
                        "invalid rule pattern; rule dropped"
                    );
                    return None;
                }
            };
            let mut negative = Vec::with_capacity(r.negative.len());
            for n in &r.negative {
                match Regex::new(n) {
                    Ok(g) => negative.push(g),
                    Err(e) => {
                        tracing::warn!(
                            target: "pane_rules",
                            rule = %r.name,
                            error = %e,
                            "invalid negative guard; rule dropped"
                        );
                        return None;
                    }
                }
            }
            Some(CompiledRule {
                name: r.name.clone(),
                kind: r.kind.clone(),
                pattern,
                negative,
                tail_lines: r.tail_lines.max(1),
                scope: r.scope,
                strip_decoration: r.strip_decoration,
                priority: r.priority,
                command: r.command.clone(),
                cooldown: std::time::Duration::from_secs(r.cooldown_secs),
            })
        })
        .collect();
    compiled.sort_by_key(|r| r.priority);
    compiled
}

/// Largest tail window across a rule set, used to size the pane capture.
pub fn max_tail_lines(rules: &[CompiledRule]) -> usize {
    rules.iter().map(|r| r.tail_lines).max().unwrap_or(0)
}

/// Strip leading non-alphanumeric decoration, then a numeric option
/// enumerator (`1.` or `2)` only, so a line like "5-hour limit reached"
/// survives). Case is preserved; patterns opt into `(?i)` themselves.
fn normalize_line(line: &str) -> &str {
    let trimmed = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = &trimmed[digits..];
    if digits > 0 && (rest.starts_with('.') || rest.starts_with(')')) {
        rest[1..].trim_start()
    } else {
        trimmed
    }
}

/// Run the compiled rules against a raw `capture-pane` tail. Returns the
/// first matching rule in priority order, or None for a healthy pane.
pub fn classify<'a>(raw: &str, rules: &'a [CompiledRule]) -> Option<&'a CompiledRule> {
    let stripped = crate::tmux::utils::strip_ansi(raw);
    let lines: Vec<&str> = stripped
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .collect();

    rules.iter().find(|rule| {
        let window = &lines[lines.len().saturating_sub(rule.tail_lines)..];
        match rule.scope {
            RuleScope::Window => {
                let text = window.join("\n");
                if rule.negative.iter().any(|g| g.is_match(&text)) {
                    return false;
                }
                rule.pattern.is_match(&text)
            }
            RuleScope::Line => window.iter().any(|line| {
                if rule.negative.iter().any(|g| g.is_match(line)) {
                    return false;
                }
                let candidate = if rule.strip_decoration {
                    normalize_line(line)
                } else {
                    line
                };
                rule.pattern.is_match(candidate)
            }),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, pattern: &str) -> PaneRuleConfig {
        PaneRuleConfig {
            name: name.into(),
            kind: String::new(),
            pattern: pattern.into(),
            negative: Vec::new(),
            tail_lines: default_tail_lines(),
            scope: RuleScope::Line,
            strip_decoration: true,
            priority: 0,
            enabled: true,
            command: None,
            cooldown_secs: default_cooldown_secs(),
        }
    }

    #[test]
    fn toml_rule_parses_with_defaults() {
        let cfg: WatchdogConfig = toml::from_str(
            r#"
            [[rules]]
            name = "my-banner"
            kind = "cap"
            pattern = '(?i)^some banner'
            command = "notify.sh"
            "#,
        )
        .unwrap();
        let r = &cfg.rules[0];
        assert_eq!(r.name, "my-banner");
        assert_eq!(r.kind, "cap");
        assert_eq!(r.tail_lines, 15);
        assert_eq!(r.scope, RuleScope::Line);
        assert!(r.strip_decoration);
        assert!(r.enabled);
        assert!(r.negative.is_empty());
        assert_eq!(r.command.as_deref(), Some("notify.sh"));
        assert_eq!(r.cooldown_secs, 30 * 60);
        assert!(!cfg.disabled);
        assert_eq!(cfg.interval_secs, None);
    }

    #[test]
    fn compile_drops_invalid_regex_keeps_valid() {
        let rules = vec![rule("bad", "(unclosed"), rule("good", "^fine$")];
        let compiled = compile(&rules);
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].name, "good");
    }

    #[test]
    fn compile_drops_invalid_negative_guard() {
        let mut r = rule("guarded", "^fine$");
        r.negative = vec!["(unclosed".into()];
        assert!(compile(&[r]).is_empty());
    }

    #[test]
    fn compile_drops_disabled_and_sorts_by_priority() {
        let mut a = rule("second", "a");
        a.priority = 5;
        let mut b = rule("first", "b");
        b.priority = 1;
        let mut c = rule("off", "c");
        c.enabled = false;
        let compiled = compile(&[a, b, c]);
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].name, "first");
        assert_eq!(compiled[1].name, "second");
    }

    #[test]
    fn compile_drops_duplicate_names_keeps_first() {
        let mut a = rule("dup", "^first$");
        a.cooldown_secs = 60;
        let mut b = rule("dup", "^second$");
        b.cooldown_secs = 999;
        let compiled = compile(&[a, b, rule("other", "^ok$")]);
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].name, "dup");
        assert_eq!(compiled[0].cooldown, std::time::Duration::from_secs(60));
        assert_eq!(compiled[1].name, "other");
    }

    #[test]
    fn line_rule_matches_anchored_banner() {
        let compiled = compile(&[rule("cap", r"(?i)^usage limit reached")]);
        let pane = "some earlier output\nUsage limit reached, resets 3pm\n";
        assert_eq!(
            classify(pane, &compiled).map(|m| m.name.as_str()),
            Some("cap")
        );
        // Mid-sentence prose must not fire an anchored pattern.
        assert!(classify("we saw usage limit reached earlier\n", &compiled).is_none());
    }

    #[test]
    fn window_rule_fires_across_lines() {
        let mut r = rule("multi", r"(?is)first half[\s\S]*second half");
        r.scope = RuleScope::Window;
        r.tail_lines = 5;
        let compiled = compile(&[r]);
        let pane = "first half of the prompt\nsome middle text\nsecond half here\n";
        assert_eq!(
            classify(pane, &compiled).map(|m| m.name.as_str()),
            Some("multi")
        );
    }

    #[test]
    fn negative_guard_suppresses_match() {
        let mut r = rule("guarded", r"(?i)^usage limit reached");
        r.negative = vec![r"(?i)not your usage limit".into()];
        let compiled = compile(&[r]);
        assert!(classify("Usage limit reached, resets 3pm\n", &compiled).is_some());
        assert!(classify("usage limit reached (not your usage limit)\n", &compiled).is_none());
    }

    #[test]
    fn strip_decoration_handles_selector_and_enumerator() {
        let compiled = compile(&[rule("opt", r"(?i)^stop and wait")]);
        assert!(classify(" > 1. Stop and wait for limit reset\n", &compiled).is_some());
        // Leading digits without a dot or paren are not an enumerator and
        // must survive normalization.
        let compiled = compile(&[rule("hour", r"(?i)^5-hour limit")]);
        assert!(classify("5-hour limit reached\n", &compiled).is_some());
    }

    #[test]
    fn strip_decoration_off_keeps_raw_line() {
        let mut r = rule("gate", r"^ACTION REQUIRED");
        r.strip_decoration = false;
        let compiled = compile(&[r]);
        assert!(classify("ACTION REQUIRED: approve the send\n", &compiled).is_some());
        assert!(classify("  * ACTION REQUIRED: approve the send\n", &compiled).is_none());
    }

    #[test]
    fn tail_lines_bounds_the_window() {
        let mut r = rule("edge", r"(?i)^stale banner");
        r.tail_lines = 3;
        let compiled = compile(&[r]);
        let mut pane = String::from("stale banner\n");
        for i in 0..10 {
            pane.push_str(&format!("later line {i}\n"));
        }
        assert!(classify(&pane, &compiled).is_none());
    }

    #[test]
    fn ansi_escapes_are_stripped_before_matching() {
        let compiled = compile(&[rule("cap", r"(?i)^usage limit reached")]);
        let pane = "\x1b[31mUsage limit reached\x1b[0m\n";
        assert!(classify(pane, &compiled).is_some());
    }

    #[test]
    fn priority_picks_first_match() {
        let mut cap = rule("cap", r"(?i)^usage limit reached");
        cap.priority = 0;
        let mut gate = rule("gate", r"^ACTION REQUIRED");
        gate.priority = 1;
        let compiled = compile(&[gate, cap]);
        let pane = "ACTION REQUIRED: something\nUsage limit reached\n";
        assert_eq!(
            classify(pane, &compiled).map(|m| m.name.as_str()),
            Some("cap")
        );
    }
}
