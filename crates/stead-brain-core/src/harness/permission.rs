// SPDX-License-Identifier: MIT
// Vendored from pie-coding-agent 0.75.0; changes: expose danger labels, prompt instead of deny, broaden force-push detection, and scan session path policy.

use std::sync::{Arc, LazyLock};

use regex::RegexSet;

use super::PathPolicy;

struct PredicateRule {
    label: &'static str,
    check: fn(&str) -> bool,
}

#[derive(Clone)]
pub struct PermissionPolicy {
    predicate_rules: Arc<Vec<PredicateRule>>,
    danger_set: Arc<RegexSet>,
    danger_labels: Arc<Vec<&'static str>>,
}

impl PermissionPolicy {
    pub fn default_for_coding_agent() -> Self {
        let patterns = default_danger_patterns();
        let labels = patterns.iter().map(|(label, _)| *label).collect();
        let regexes = patterns.iter().map(|(_, regex)| *regex).collect::<Vec<_>>();
        Self {
            predicate_rules: Arc::new(default_predicate_rules()),
            danger_set: Arc::new(RegexSet::new(regexes).expect("danger patterns compile")),
            danger_labels: Arc::new(labels),
        }
    }

    pub fn prompt_reason(&self, command: &str, paths: &PathPolicy) -> Option<String> {
        let label = self.danger_label(command).or_else(|| {
            outside_path_in_command(command, paths).then_some("path outside session workspace")
        })?;
        Some(format!("{label}: {}", truncate_command(command, 120)))
    }

    fn danger_label(&self, command: &str) -> Option<&'static str> {
        for rule in self.predicate_rules.iter() {
            if (rule.check)(command) {
                return Some(rule.label);
            }
        }
        self.danger_set
            .matches(command)
            .into_iter()
            .next()
            .and_then(|index| self.danger_labels.get(index).copied())
    }
}

fn default_danger_patterns() -> Vec<(&'static str, &'static str)> {
    vec![
        ("sudo invocation", r"\bsudo\b"),
        (
            "curl/wget piped into shell",
            r"\b(curl|wget)\b[^|]*\|\s*(bash|sh|zsh|fish)\b",
        ),
        (
            "dd writing to a block device",
            r"\bdd\b[^\n]*\bof=/dev/(disk|sd[a-z]|nvme|hd[a-z])",
        ),
        ("mkfs / format command", r"\bmkfs(\.|\s)"),
        ("chmod 777 on absolute path", r"\bchmod\b\s+777\s+/"),
        (
            "shutdown / reboot / halt",
            r"\b(shutdown|reboot|halt|poweroff)\b",
        ),
        ("git push --force", r"\bgit\s+push\b[^\n]*(--force|-f)\b"),
        ("piping into eval", r"\|\s*eval\b"),
        (":(){:|:&};: forkbomb", r":\(\)\s*\{\s*:\|:&\s*\}\s*;\s*:"),
    ]
}

fn default_predicate_rules() -> Vec<PredicateRule> {
    vec![
        PredicateRule {
            label: "rm recursive+force on absolute path",
            check: |command| {
                rm_dangerous_with(command, |operand| {
                    operand == "/" || operand.starts_with('/')
                })
            },
        },
        PredicateRule {
            label: "rm recursive+force on $HOME or ~",
            check: |command| {
                rm_dangerous_with(command, |operand| {
                    operand == "~"
                        || operand.starts_with("~/")
                        || operand == "$HOME"
                        || operand.starts_with("$HOME/")
                })
            },
        },
    ]
}

fn rm_dangerous_with(command: &str, target_matches: fn(&str) -> bool) -> bool {
    for clause in split_shell_clauses(command) {
        let tokens = clause.split_whitespace().collect::<Vec<_>>();
        let Some(first) = tokens.first() else {
            continue;
        };
        if first.rsplit('/').next().unwrap_or(first) != "rm" {
            continue;
        }
        let mut recursive = false;
        let mut force = false;
        let mut operands = Vec::new();
        for token in tokens.iter().skip(1) {
            if let Some(long) = token.strip_prefix("--") {
                recursive |= long == "recursive";
                force |= long == "force";
            } else if let Some(short) = token.strip_prefix('-') {
                recursive |= short.contains('r') || short.contains('R');
                force |= short.contains('f');
            } else {
                operands.push(normalize_operand(token));
            }
        }
        if recursive && force && operands.iter().any(|operand| target_matches(operand)) {
            return true;
        }
    }
    false
}

fn normalize_operand(raw: &str) -> String {
    let raw = raw.trim_matches(|c| c == '\'' || c == '"');
    raw.strip_prefix("${HOME}")
        .map(|rest| format!("$HOME{rest}"))
        .unwrap_or_else(|| raw.to_string())
}

fn split_shell_clauses(command: &str) -> Vec<&str> {
    static SPLIT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?:&&|\|\||[;|])").expect("shell split regex"));
    SPLIT
        .split(command)
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect()
}

fn outside_path_in_command(command: &str, paths: &PathPolicy) -> bool {
    command.split_whitespace().any(|token| {
        let token = token
            .trim_matches(|c: char| matches!(c, '\'' | '"' | '(' | ')' | ';' | ',' | '|' | '&'));
        if token.contains("://") {
            return false;
        }
        let candidate = token
            .split_once('=')
            .map(|(_, value)| value)
            .unwrap_or(token);
        let redirect = candidate
            .char_indices()
            .rev()
            .find_map(|(index, character)| matches!(character, '>' | '<').then_some(index));
        let candidate = redirect
            .map(|index| &candidate[index + 1..])
            .unwrap_or(candidate);
        !candidate.is_empty() && !candidate.starts_with('-') && paths.denied_shell_path(candidate)
    })
}

fn truncate_command(command: &str, max_chars: usize) -> String {
    let mut chars = command.chars();
    let value = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() && max_chars > 0 {
        let prefix = value.chars().take(max_chars - 1).collect::<String>();
        format!("{prefix}…")
    } else {
        value
    }
}
