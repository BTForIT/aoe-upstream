# Pane Rules

Pane rules let the `aoe serve` daemon watch every session's terminal output for text you care about and run a command when it appears. Think of them as "when a pane says X, do Y": detect a usage-limit banner and rotate work to another account, catch a sign-in prompt and send yourself a push notification, or flag any agent that prints a phrase your team uses for "I need a human".

The daemon periodically captures the tail of each registered session's tmux pane, runs your rules against it, and executes the matching rule's command with the session context in its environment. Rules are pure configuration; no code changes are needed to add one.

Pane rules require a running daemon (`aoe serve`). A read-only daemon never runs pane rules, since they execute user commands.

## Quick start

Add a `[watchdog]` section to `~/.agent-of-empires/config.toml` (or your platform's config path):

```toml
[[watchdog.rules]]
name = "usage-cap"
kind = "cap"
pattern = '(?i)^usage limit reached'
command = "~/bin/notify-capped.sh"
```

Restart the daemon. Every scan (default: every 3 minutes), any session whose recent pane output contains a line starting with "usage limit reached" runs `~/bin/notify-capped.sh` with these variables in its environment:

| Variable | Value |
|----------|-------|
| `AOE_RULE_NAME` | The rule's `name` ("usage-cap") |
| `AOE_RULE_KIND` | The rule's `kind` ("cap") |
| `AOE_SESSION_ID` | The matched session's id |
| `AOE_SESSION_TITLE` | The matched session's title |
| `AOE_SESSION_PROFILE` | The profile the session belongs to |

The command runs via `sh -c`, is killed after 60 seconds, and its failures are logged, never fatal.

## Rule reference

```toml
[watchdog]
# disabled = true          # turn the watchdog off without deleting rules
# interval_secs = 180      # scan cadence (minimum 10)

[[watchdog.rules]]
name = "device-code"           # required: label for logs and AOE_RULE_NAME
kind = "auth"                  # optional: free-form category, AOE_RULE_KIND
pattern = '(?is)devicelogin.*code'   # required: regex (Rust regex syntax)
negative = ['(?i)example']     # optional: text matching any of these never fires the rule
tail_lines = 8                 # optional: how many recent non-empty lines are in scope (default 15)
scope = "window"               # optional: "line" (default) or "window"
strip_decoration = true        # optional: strip leading TUI chrome before matching (default true)
priority = 1                   # optional: lower fires first when several rules match (default 0)
enabled = true                 # optional: set false to keep a rule but skip it
command = "notify.sh"          # optional: run on match; omit to only log
cooldown_secs = 1800           # optional: per-session refire floor (default 30 minutes)
```

Field notes:

- **`pattern`** is matched case-sensitively; start it with `(?i)` for case-insensitive. Anchors (`^`) apply per line in `line` scope and to the joined window text in `window` scope.
- **`scope = "line"`** tries the pattern against each recent non-empty line individually. **`scope = "window"`** joins the tail into one string first, for prompts that span lines (add `(?s)` so `.` crosses newlines).
- **`tail_lines`** counts non-empty lines from the live edge of the pane. Keep it small: a tight window keeps stale scrollback (a banner from an hour ago, an error the agent already recovered from) from firing.
- **`strip_decoration`** removes leading selector glyphs, bullets, and a `1.` or `2)` option enumerator so an anchored pattern still matches a highlighted TUI menu row. It applies only in `line` scope, and only for the pattern; `negative` guards always see the raw line.
- **`negative`** guards suppress a match, useful when the trigger phrase also appears in benign text.
- **`cooldown_secs`** stops a pane that stays in the matched state from re-running the command every scan. The first match fires immediately; repeats within the window are skipped per session and rule.
- ANSI escape sequences are stripped from the pane before matching.

An invalid regex does not break the daemon: the rule is dropped with a warning in the daemon log and the remaining rules keep running.

## Examples

Notify your phone when any agent is waiting on a device-code sign-in (see [Push Notifications](../push-notifications.md) for a richer channel):

```toml
[[watchdog.rules]]
name = "device-code"
kind = "auth"
pattern = '(?is)devicelogin.*[A-Z0-9]{8,9}'
scope = "window"
tail_lines = 8
cooldown_secs = 600
command = "notify-send 'AoE' \"$AOE_SESSION_TITLE needs a device-code sign-in\""
```

Log every session that hits a provider usage cap, with a guard against prose mentions:

```toml
[[watchdog.rules]]
name = "usage-cap"
kind = "cap"
pattern = '(?i)^(usage limit reached|you.ve hit your usage limit)'
negative = ['(?i)if you hit your usage limit']
tail_lines = 30
command = "echo \"$(date -u +%FT%TZ) $AOE_SESSION_ID $AOE_SESSION_TITLE\" >> ~/aoe-capped.log"
```

Escalate when an agent prints your team's hand-off phrase, using a case-sensitive anchored match so prose does not trigger it:

```toml
[[watchdog.rules]]
name = "needs-human"
kind = "escalation"
pattern = '^ACTION REQUIRED'
strip_decoration = false
priority = 5
command = "~/bin/page-oncall.sh"
```

## How scanning works

- The daemon scans on a fixed interval (`interval_secs`, default 180 seconds, minimum 10). Each scan captures the pane tail of every registered session that is not running in the structured agent view (structured sessions do not have meaningful pane text).
- Rules are evaluated in `priority` order and the first match per session wins, so give your most specific rules the lowest numbers.
- Matches are logged under the `server.pane_watchdog` tracing target whether or not a `command` is set, so you can dry-run a new rule by watching the daemon log.
- Command stdout is discarded; stderr appears in the daemon log when the command exits non-zero.
