//! FR-3 (#998): the grant-independent absolute deny-list — a fixed veto that NO
//! capability, mode, or persona grant can unlock, spanning built-in AND remote
//! tools. Checked at the top of [`super::tools::execute_tool_with_offload`],
//! ABOVE the `mcp.handles` early-return (so one site covers built-in AND remote
//! dispatch) and BEFORE alias-rewrite / `run_command`→builtin routing (so it
//! sees the pre-rewrite name + args).
//!
//! ## Why structural, not a substring scan
//! The coaching workload is full of `ssh` / `rm -rf` / `systemctl restart`
//! inside runbook and alert TEXT. A regex over serialized args would veto the
//! coach's own questions ("should I run `systemctl restart nginx`?"). So we
//! extract the real axis TARGET of each call — the exec leading token for a
//! shell command — and match THAT. `ssh` as the command a model wants to
//! execute is denied; `ssh` quoted in a `request_user_input` question, or
//! written into a note's `content`, is not.
//!
//! Fail-closed within the axis it checks: a shell command whose leader hits the
//! list is refused regardless of caveats; anything we cannot parse to a target
//! simply is not an exec surface and falls through to the normal leashes.

use serde_json::Value;

/// The verdict for one tool call against the absolute deny-list. `reason` is
/// surfaced to the model verbatim as the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    pub reason: String,
}

/// Tool names that drive a free-form shell command line: the `run_command`
/// built-in PLUS the shell aliases [`super::tools::resolve_tool_alias`] rewrites
/// to it. Listed here so the deny-list sees the exec surface under ANY spelling
/// and BEFORE the rewrite. Keep in sync with the shell-alias arm of
/// `resolve_tool_alias`.
const EXEC_TOOL_NAMES: &[&str] = &[
    "run_command",
    "execute",
    "exec",
    "bash",
    "shell",
    "sh",
    "zsh",
    "terminal",
    "run_shell_command",
    "shell_command",
    "system",
];

/// Exec leading tokens NO session may run — lateral movement off this host and
/// raw destructive / disk / power operations. Matched against the RESOLVED
/// leading token of a shell command (after `sudo` / `env VAR=val` prefixes,
/// reduced to the basename of a path), never against arbitrary args.
#[rustfmt::skip]
const DENY_EXEC_LEADERS: &[&str] = &[
    // lateral movement off this host
    "ssh",
    "scp",
    "sftp",
    "rsh",
    "rlogin",
    "telnet",
    // destructive / raw-disk
    "rmdir",
    "shred",
    "dd",
    "mkfs",
    "fdisk",
    "parted",
    "wipefs",
    // host power state (would take down the machine newt runs on)
    "shutdown", "reboot", "halt", "poweroff",
];

/// Service managers whose STATE-CHANGING actions are denied. Read-only actions
/// (status / show / list / is-*) fall through — the deny is on disruption, not
/// inspection. Matched position-independently over the remaining tokens, since
/// `systemctl restart X` and `service X restart` order their action differently.
const SERVICE_MANAGERS: &[&str] = &["systemctl", "service"];
const SERVICE_STATE_ACTIONS: &[&str] = &[
    "restart",
    "stop",
    "start",
    "reload",
    "try-restart",
    "force-reload",
    "disable",
    "enable",
    "kill",
    "mask",
    "unmask",
    "isolate",
    "reboot",
    "poweroff",
    "halt",
];

/// The single entry point: does this tool call hit the absolute deny-list?
/// `None` = allowed to proceed to the persona allow-list / routing / dispatch.
pub fn deny_check(name: &str, args: &Value) -> Option<Denied> {
    if EXEC_TOOL_NAMES.contains(&name) {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        return deny_exec(command);
    }
    // fs / net axes: structural hooks intentionally deferred — the RFC (#998)
    // acceptance is exec-focused, and fs writes / net reach are already leashed
    // by the fs_write / net caveats. When added, extract `args["path"]` /
    // `args["url"]` host here in the SAME structural style (never a content scan).
    None
}

/// Classify a shell command line by its EXEC TARGET (the leading token), after
/// stripping `sudo` / `doas` / `env VAR=val` wrappers and any leading directory.
fn deny_exec(command: &str) -> Option<Denied> {
    let mut tokens = command.split_ascii_whitespace().peekable();

    // Strip privilege / env prefixes so `sudo rm` and `env X=1 ssh` still match.
    loop {
        match tokens.peek().copied() {
            Some("sudo") | Some("doas") => {
                tokens.next();
            }
            Some("env") => {
                tokens.next();
                // Consume `VAR=val` assignments that follow `env`.
                while tokens
                    .peek()
                    .is_some_and(|t| t.contains('=') && !t.starts_with('='))
                {
                    tokens.next();
                }
            }
            _ => break,
        }
    }

    let Some(raw_leader) = tokens.next() else {
        return None; // empty command — nothing to deny
    };
    // `/usr/bin/ssh` → `ssh`; `./rm` → `rm`.
    let leader = raw_leader.rsplit(['/', '\\']).next().unwrap_or(raw_leader);

    if leader == "rm" || leader == "unlink" {
        let rest: Vec<&str> = tokens.collect();
        if simple_one_file_delete(leader, &rest) {
            return None;
        }
        return Some(Denied {
            reason: deny_message(leader, "an absolutely forbidden command"),
        });
    }

    if DENY_EXEC_LEADERS.contains(&leader) {
        return Some(Denied {
            reason: deny_message(leader, "an absolutely forbidden command"),
        });
    }

    if SERVICE_MANAGERS.contains(&leader) && tokens.any(|t| SERVICE_STATE_ACTIONS.contains(&t)) {
        return Some(Denied {
            reason: deny_message(leader, "a service-state change"),
        });
    }

    None
}

fn simple_one_file_delete(leader: &str, rest: &[&str]) -> bool {
    const SHELL_META: &[char] = &['&', '|', ';', '`', '$', '\n', '>', '<', '(', ')'];
    let mut operands = Vec::new();
    let mut end_of_flags = false;
    for token in rest {
        if token.contains(SHELL_META) {
            return false;
        }
        if !end_of_flags && *token == "--" {
            end_of_flags = true;
            continue;
        }
        if !end_of_flags && token.starts_with('-') {
            match leader {
                // `rm -f file` is still a one-file delete; recursive / dir /
                // forceful tree forms stay absolutely denied.
                "rm" if token.chars().skip(1).all(|c| c == 'f') => continue,
                _ => return false,
            }
        }
        operands.push(*token);
    }
    operands.len() == 1
}

fn deny_message(target: &str, what: &str) -> String {
    format!(
        "Refused: `{target}` is {what} under newt's absolute deny-list — a fixed \
         safety floor that no capability, mode, or persona grant can unlock. This \
         is not a permission you can request; if the operation is genuinely \
         required, a human must perform it outside the agent."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(c: &str) -> Option<Denied> {
        deny_check("run_command", &json!({ "command": c }))
    }

    #[test]
    fn denies_lateral_movement_destruction_and_service_changes() {
        assert!(cmd("ssh host 'uptime'").is_some(), "ssh denied");
        assert!(cmd("rm -rf build/").is_some(), "recursive rm denied");
        assert!(cmd("rm -r build/").is_some(), "recursive rm denied");
        assert!(cmd("rm a b").is_some(), "multi-target rm denied");
        assert!(cmd("sudo rm -rf /").is_some(), "sudo recursive rm denied");
        assert!(
            cmd("/usr/bin/scp a b:").is_some(),
            "path-qualified scp denied"
        );
        assert!(
            cmd("env FOO=1 ssh host").is_some(),
            "env-prefixed ssh denied"
        );
        assert!(
            cmd("systemctl restart nginx").is_some(),
            "systemctl restart denied"
        );
        assert!(
            cmd("service nginx restart").is_some(),
            "service restart denied"
        );
        assert!(cmd("shutdown -h now").is_some(), "shutdown denied");
        assert!(cmd("dd if=/dev/zero of=/dev/sda").is_some(), "dd denied");
    }

    #[test]
    fn allows_simple_one_file_delete_for_governed_routing() {
        assert!(
            cmd("rm src/cockpit.rs").is_none(),
            "plain one-file rm should reach routing/delete_file"
        );
        assert!(
            cmd("rm -f src/cockpit.rs").is_none(),
            "force one-file rm should reach routing/delete_file"
        );
        assert!(
            cmd("unlink src/cockpit.rs").is_none(),
            "unlink one-file delete should reach routing/delete_file"
        );
    }

    /// The absolute floor ignores the tool NAME's spelling: a shell alias that
    /// rewrites to `run_command` is caught under its raw name, pre-rewrite.
    #[test]
    fn denies_exec_under_shell_aliases_too() {
        for alias in ["bash", "exec", "shell", "sh", "system"] {
            let hit = deny_check(alias, &json!({ "command": "ssh host" }));
            assert!(hit.is_some(), "{alias} → ssh must be denied");
        }
    }

    #[test]
    fn allows_inspection_and_ordinary_commands() {
        assert!(
            cmd("systemctl status nginx").is_none(),
            "status is read-only"
        );
        assert!(
            cmd("service nginx status").is_none(),
            "service status read-only"
        );
        assert!(cmd("cargo test").is_none());
        assert!(cmd("git log --oneline").is_none());
        assert!(cmd("ls -la").is_none());
        assert!(cmd("").is_none(), "empty command is a no-op, not a hit");
        assert!(
            cmd("ripgrep ssh src/").is_none(),
            "ssh as an ARG is not the target"
        );
    }

    /// The crux of #998: `ssh` / `rm -rf` / `systemctl restart` appearing as
    /// DATA in another tool's args (a coach's question, a runbook excerpt) must
    /// NOT be vetoed — only the same tokens in EXEC-TARGET position are.
    #[test]
    fn does_not_veto_forbidden_words_as_data_in_other_tools() {
        let question = json!({
            "prompt": "The alert says `ssh web01 && systemctl restart nginx`, \
                       then `rm -rf /var/cache/*`. Should I?",
        });
        assert!(deny_check("request_user_input", &question).is_none());
        let note = json!({
            "path": "notes/incident.md",
            "content": "Runbook: ssh in, systemctl restart, rm -rf the cache.",
        });
        assert!(deny_check("write_file", &note).is_none());
    }
}
