//! The Claude Code hook (PreToolUse on Bash and PowerShell): a cargo build, check, test, clippy or run started
//! without `fgate` in front is refused with the command to use instead. Agents keep their usual commands and only
//! add the prefix. A project adds its own rules on the hook's command line (in its `.claude/settings.json`):
//!
//! ```text
//! fgate hook [--gpu name,name] [--protect-branch main] [--protect-label ci-approved] [--no-admin-merge] [--note "…"]
//! ```
//!
//! `--gpu` names programs (exe names without `.exe`) that must run under `fgate --gpu`; the other rules guard how
//! work lands; `--note` is added to every refusal, to point agents at the project's own instructions.
//! `FGATE_OFF=1` turns the hook off.

use std::io::Read;
use std::process::ExitCode;

/// Cargo commands that compile or run something.
const HEAVY: &[&str] = &[
    "build", "b", "check", "c", "test", "t", "clippy", "run", "r", "bench", "doc", "d", "install",
    "nextest", "rustc", "rustdoc", "llvm-cov", "miri", "fix",
];

/// Words that may stand in front of the command itself.
const PREFIXES: &[&str] = &[
    "time",
    "nice",
    "nohup",
    "exec",
    "command",
    "env",
    "&",
    "start",
    "-NoNewWindow",
    "-Wait",
];

/// A project's rules, from the hook's command line.
#[derive(Debug, Default)]
pub struct Rules {
    /// Programs that need the GPU slot, lower case, without `.exe`.
    pub gpu: Vec<String>,
    /// Branches nothing is pushed to directly.
    pub branches: Vec<String>,
    /// Labels agents never add or remove.
    pub labels: Vec<String>,
    /// Refuse `gh pr merge --admin`.
    pub no_admin_merge: bool,
    /// Added to every refusal.
    pub note: Option<String>,
}

impl Rules {
    pub fn parse(args: &[String]) -> Result<Rules, String> {
        let mut rules = Rules::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
                _ => (arg.as_str(), None),
            };
            if flag == "--no-admin-merge" {
                rules.no_admin_merge = true;
                continue;
            }
            if !["--gpu", "--protect-branch", "--protect-label", "--note"].contains(&flag) {
                return Err(format!("unknown hook option {arg}"));
            }
            let value = match inline {
                Some(value) => value,
                None => args
                    .next()
                    .cloned()
                    .ok_or(format!("{flag} needs a value"))?,
            };
            match flag {
                "--gpu" => rules.gpu.extend(
                    value
                        .split(',')
                        .map(|name| program_name(name.trim()))
                        .filter(|name| !name.is_empty()),
                ),
                "--protect-branch" => rules.branches.push(value),
                "--protect-label" => rules.labels.push(value),
                "--note" => rules.note = Some(value),
                _ => return Err(format!("unknown hook option {arg}")),
            }
        }
        Ok(rules)
    }
}

/// Why a command is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Need {
    /// A build without the gate.
    Build,
    /// A GPU program without the gate.
    Gpu,
    /// A push straight to a protected branch.
    Push(String),
    /// A protected label in a `gh` command.
    Label(String),
    /// `gh pr merge --admin` skips the required checks.
    AdminMerge,
}

pub fn run(args: &[String]) -> ExitCode {
    if std::env::var_os("FGATE_OFF").is_some() {
        return ExitCode::SUCCESS;
    }
    let rules = match Rules::parse(args) {
        Ok(rules) => rules,
        Err(e) => {
            eprintln!("fgate hook: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(call) = serde_json::from_str::<serde_json::Value>(&input) else {
        return ExitCode::SUCCESS;
    };
    let Some(command) = call.pointer("/tool_input/command").and_then(|c| c.as_str()) else {
        return ExitCode::SUCCESS;
    };
    let Some(need) = ungated(command, &rules) else {
        return ExitCode::SUCCESS;
    };
    let mut reason = match need {
        Need::Build => "Builds on this machine wait their turn through the build gate. Put `fgate` right before \
             `cargo`, for example `fgate cargo test`. fgate waits for a free build slot and prints its place in line, \
             then runs the command unchanged. If `fgate status` shows a line, run the command in the background \
             (run_in_background) so the wait doesn't hit the tool's time limit."
            .to_string(),
        Need::Gpu => "This program takes turns on the GPU through the build gate. Put `fgate --gpu` right before \
             it, for example `fgate --gpu <program> <args>`; a script that starts it runs under `fgate --gpu` as a \
             whole. If `fgate status` shows a line, run it in the background (run_in_background)."
            .to_string(),
        Need::Push(branch) => format!(
            "Nothing is pushed to `{branch}` directly. Commit on your own branch, push that, and let it land \
             through a pull request."
        ),
        Need::Label(label) => format!(
            "The `{label}` label is set and removed by people, never by agents. Leave it alone and tell the owner \
             what needs it."
        ),
        Need::AdminMerge => "`gh pr merge --admin` skips the required checks. Let the pull request merge through \
             its checks, or ask the owner."
            .to_string(),
    };
    if let Some(note) = &rules.note {
        reason = format!("{reason} {note}");
    }
    let answer = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{answer}");
    ExitCode::SUCCESS
}

/// What a shell command starts without the gate, if anything.
pub fn ungated(command: &str, rules: &Rules) -> Option<Need> {
    let text = blank_quotes(&drop_heredocs(command));
    for segment in text.split(['\n', ';', '|', '&', '(', ')', '{', '}']) {
        let mut words = segment.split_whitespace().peekable();
        while let Some(word) = words.peek() {
            let assignment = word.split_once('=').is_some_and(|(name, _)| {
                !name.is_empty()
                    && name
                        .trim_start_matches("$env:")
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            if assignment || PREFIXES.contains(word) {
                words.next();
            } else {
                break;
            }
        }
        let Some(program) = words.next() else {
            continue;
        };
        let name = program_name(program);
        if name == "fgate" {
            continue;
        }
        if name == "git" {
            // Skip git's own options (`-C <dir>`, `-c <setting>`) to reach the subcommand.
            let mut sub = None;
            while let Some(w) = words.next() {
                if w == "-C" || w == "-c" {
                    words.next();
                } else if !w.starts_with('-') {
                    sub = Some(w);
                    break;
                }
            }
            if sub == Some("push") {
                for w in words.by_ref() {
                    if let Some(branch) = rules.branches.iter().find(|b| names_branch(w, b)) {
                        return Some(Need::Push(branch.clone()));
                    }
                }
            }
        } else if name == "gh" {
            let rest: Vec<&str> = words.collect();
            if let Some(label) = rules
                .labels
                .iter()
                .find(|label| rest.iter().any(|w| w.contains(label.as_str())))
            {
                return Some(Need::Label(label.clone()));
            }
            if rules.no_admin_merge
                && rest.windows(2).any(|w| w == ["pr", "merge"])
                && rest.contains(&"--admin")
            {
                return Some(Need::AdminMerge);
            }
        } else if name == "cargo" {
            let sub = words.find(|w| !w.starts_with('-') && !w.starts_with('+'));
            if sub.is_some_and(|s| HEAVY.contains(&s)) {
                return Some(Need::Build);
            }
        } else if rules.gpu.contains(&name) {
            return Some(Need::Gpu);
        }
    }
    None
}

/// A push refspec or branch naming `branch`: `main`, `HEAD:main`, `refs/heads/main`, `+main`.
fn names_branch(word: &str, branch: &str) -> bool {
    let target = word
        .rsplit(':')
        .next()
        .unwrap_or(word)
        .trim_start_matches('+');
    target == branch || target.strip_prefix("refs/heads/") == Some(branch)
}

/// `"C:/x/target/debug/my_game.exe"` → `my_game`.
fn program_name(word: &str) -> String {
    let word = word.trim_matches(['"', '\'']).replace('\\', "/");
    let base = word.rsplit('/').next().unwrap_or(&word);
    base.strip_suffix(".exe")
        .unwrap_or(base)
        .to_ascii_lowercase()
}

/// Removes heredoc bodies (`<<EOF … EOF`): text written to a file, not run.
fn drop_heredocs(command: &str) -> String {
    let mut out = String::new();
    let mut end: Option<String> = None;
    for line in command.lines() {
        if let Some(word) = &end {
            if line.trim() == word {
                end = None;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if let Some((_, rest)) = line.split_once("<<") {
            let rest = rest.trim_start_matches(['-', '~']).trim_start();
            let word: String = rest
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !word.is_empty() {
                end = Some(word);
            }
        }
    }
    out
}

/// Replaces what's inside quotes with spaces when it holds a space: a commit message that mentions `cargo test`
/// runs nothing. A quoted single word, such as an exe's path, stays.
fn blank_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(['"', '\'']) {
        let q = rest[start..].chars().next().unwrap_or('"');
        out.push_str(&rest[..=start]);
        let inner = &rest[start + 1..];
        let mut end = None;
        let mut escaped = false;
        for (i, c) in inner.char_indices() {
            if escaped {
                escaped = false;
            } else if c == '\\' && q == '"' {
                escaped = true;
            } else if c == q {
                end = Some(i);
                break;
            }
        }
        let end = end.unwrap_or(inner.len());
        let quoted = &inner[..end];
        if quoted.chars().any(char::is_whitespace) {
            out.extend(quoted.chars().map(|c| if c == '\n' { '\n' } else { ' ' }));
        } else {
            out.push_str(quoted);
        }
        rest = &inner[end..];
        if let Some(c) = rest.chars().next() {
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Rules {
        Rules::default()
    }

    /// The rules a game project might give: two GPU programs and the landing guards.
    fn project() -> Rules {
        let args = "--gpu my_game,My_Viewer.exe --protect-branch main --protect-label ci-approved --no-admin-merge \
                    --note=See-AGENTS.md";
        Rules::parse(&args.split(' ').map(String::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn options_parse() {
        let rules = project();
        assert_eq!(rules.gpu, ["my_game", "my_viewer"]);
        assert_eq!(rules.branches, ["main"]);
        assert_eq!(rules.labels, ["ci-approved"]);
        assert!(rules.no_admin_merge);
        assert_eq!(rules.note.as_deref(), Some("See-AGENTS.md"));
        assert!(Rules::parse(&["--gpu".into()]).is_err());
        assert!(Rules::parse(&["--colour".into(), "red".into()]).is_err());
    }

    #[test]
    fn plain_cargo_builds_are_caught() {
        for rules in [none(), project()] {
            let ungated = |c| ungated(c, &rules);
            assert_eq!(ungated("cargo test -p my_crate"), Some(Need::Build));
            assert_eq!(
                ungated("CARGO_TARGET_DIR=E:/x/target cargo clippy --workspace -- -D warnings"),
                Some(Need::Build)
            );
            assert_eq!(
                ungated("cd crates && cargo build --release 2>&1 | tail -20"),
                Some(Need::Build)
            );
            assert_eq!(
                ungated("$env:CARGO_TARGET_DIR='E:/x'; cargo +stable t"),
                Some(Need::Build)
            );
            assert_eq!(ungated("& cargo.exe check --workspace"), Some(Need::Build));
        }
    }

    #[test]
    fn gpu_programs_are_caught_when_named() {
        let rules = project();
        assert_eq!(
            ungated("\"C:/target/debug/my_game.exe\" --x", &rules),
            Some(Need::Gpu)
        );
        assert_eq!(
            ungated("SCREENSHOT=a.png ./target/debug/my_viewer.exe pump", &rules),
            Some(Need::Gpu)
        );
        assert_eq!(
            ungated("\"C:/target/debug/my_game.exe\" --x", &none()),
            None
        );
    }

    #[test]
    fn gated_and_harmless_commands_pass() {
        let rules = project();
        let ungated = |c| ungated(c, &rules);
        assert_eq!(
            ungated("CARGO_TARGET_DIR=E:/x/target fgate cargo test -p my_crate 2>&1 | tail"),
            None
        );
        assert_eq!(
            ungated("fgate --gpu target/debug/my_viewer.exe pump --shots"),
            None
        );
        assert_eq!(
            ungated("cargo fmt --all --check && cargo clean -p my_crate"),
            None
        );
        assert_eq!(
            ungated("cargo --version; cargo metadata --format-version 1"),
            None
        );
        assert_eq!(ungated("git commit -m \"fix; cargo test passes\""), None);
        assert_eq!(ungated("grep -rn 'cargo build' docs"), None);
        assert_eq!(
            ungated("cat > run.sh <<'EOF'\ncargo test --workspace\nEOF\necho ok"),
            None
        );
        assert_eq!(ungated("ls target/debug/my_game.exe"), None);
    }

    #[test]
    fn landing_rules() {
        let rules = project();
        let ungated = |c| ungated(c, &rules);
        let push = Some(Need::Push("main".into()));
        assert_eq!(ungated("git push origin HEAD:main"), push);
        assert_eq!(ungated("git -C . push -q origin main"), push);
        assert_eq!(ungated("git push origin +refs/heads/main"), push);
        assert_eq!(ungated("git push -u origin HEAD"), None);
        assert_eq!(ungated("git push origin worktree-main-menu"), None);
        assert_eq!(ungated("git checkout main && git pull --ff-only"), None);
        let label = Some(Need::Label("ci-approved".into()));
        assert_eq!(ungated("gh pr edit 5 --add-label ci-approved"), label);
        assert_eq!(
            ungated("gh api repos/x/y/issues/5/labels -f 'labels[]=ci-approved'"),
            label
        );
        assert_eq!(
            ungated("gh pr merge 5 --admin --merge"),
            Some(Need::AdminMerge)
        );
        assert_eq!(ungated("gh pr merge 5 --auto --merge"), None);
        assert_eq!(ungated("gh pr view --json labels"), None);
    }

    #[test]
    fn landing_rules_are_off_without_options() {
        let rules = none();
        assert_eq!(ungated("git push origin HEAD:main", &rules), None);
        assert_eq!(
            ungated("gh pr edit 5 --add-label ci-approved", &rules),
            None
        );
        assert_eq!(ungated("gh pr merge 5 --admin --merge", &rules), None);
    }
}
