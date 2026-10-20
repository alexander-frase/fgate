//! A cargo target directory shared by several checkouts of one workspace (git worktrees, one per agent, all building
//! into one `CARGO_TARGET_DIR` so the dependencies are built once).
//!
//! Cargo names a workspace member's build output by its path relative to the workspace, not by the checkout it came
//! from, so every checkout writes its own crates to the same files. It then decides from file times whether they are
//! fresh: a build from another checkout that happened later looks newer than your sources, and cargo reuses it, so
//! your build runs another checkout's code, or fails against its older or newer API. The dependencies are the same
//! in every checkout and safe to share.
//!
//! So before a compiling cargo command fgate looks at who built that profile last (a `.fgate-checkouts` file in the
//! target directory). When it was another checkout, or nobody is recorded, it cleans this workspace's own packages
//! for that profile first (`cargo clean -p <member> …`, the dependencies stay) and records this checkout. Setting
//! `switch_clean = 0` turns it off.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The record in the target directory: one line per profile, `<profile key>\t<workspace root>`.
const STAMP: &str = ".fgate-checkouts";

/// Cargo's subcommands (and their aliases) that compile the workspace.
const COMPILING: [&str; 14] = [
    "build", "b", "check", "c", "test", "t", "run", "r", "clippy", "doc", "d", "bench", "rustc",
    "rustdoc",
];

/// What a cargo command line says about where and how it builds.
#[derive(Debug, PartialEq, Eq)]
pub struct Build {
    /// `--target-dir` from the command line (the environment's comes later).
    pub target_dir: Option<PathBuf>,
    /// `--manifest-path`, which picks the workspace instead of the current folder.
    pub manifest: Option<PathBuf>,
    /// The profile flags to repeat for `cargo clean` (`--release`, `--profile p`) and `--target t`.
    pub flags: Vec<String>,
    /// The output folder within the target directory: `debug`, `release`, a custom profile's name, under the
    /// target triple's folder when one is given.
    pub key: String,
}

/// The build a command asks for, if it is a compiling cargo command (`cargo +nightly test …`, `cargo.exe b`).
pub fn parse(command: &[String]) -> Option<Build> {
    let program = command.first()?;
    let name = Path::new(program)
        .file_stem()?
        .to_string_lossy()
        .to_lowercase();
    if name != "cargo" {
        return None;
    }
    // Everything after `--` belongs to the program cargo runs.
    let args: Vec<&String> = command[1..]
        .iter()
        .take_while(|a| a.as_str() != "--")
        .collect();
    let mut sub = None;
    let mut target_dir = None;
    let mut manifest = None;
    let mut profile: Option<String> = None;
    let mut triple: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        // A flag's value, `--flag value` (taking the next argument) or `--flag=value`.
        let mut value = |flag: &str| -> Option<String> {
            if a == flag {
                i += 1;
                args.get(i).map(|v| v.to_string())
            } else {
                a.strip_prefix(&format!("{flag}=")).map(str::to_string)
            }
        };
        if let Some(v) = value("--target-dir") {
            target_dir = Some(PathBuf::from(v));
        } else if let Some(v) = value("--manifest-path") {
            manifest = Some(PathBuf::from(v));
        } else if let Some(v) = value("--profile") {
            profile = Some(v);
        } else if let Some(v) = value("--target") {
            triple = Some(v);
        } else if a == "--release" || a == "-r" {
            profile = Some("release".into());
        } else if sub.is_none() && !a.starts_with('-') && !a.starts_with('+') {
            // The toolchain (`+nightly`) and cargo's own options come before the subcommand.
            sub = Some(a.to_string());
        }
        i += 1;
    }
    let sub = sub?;
    if !COMPILING.contains(&sub.as_str()) {
        return None;
    }
    let default = if sub == "bench" { "bench" } else { "dev" };
    let profile = profile.unwrap_or_else(|| default.into());
    let dir = match profile.as_str() {
        "dev" | "test" => "debug",
        "release" | "bench" => "release",
        other => other,
    };
    let mut flags = match profile.as_str() {
        "dev" => vec![],
        "release" => vec!["--release".into()],
        other => vec!["--profile".into(), other.to_string()],
    };
    if let Some(t) = &triple {
        flags.extend(["--target".to_string(), t.clone()]);
    }
    let key = match &triple {
        Some(t) => format!("{t}/{dir}"),
        None => dir.to_string(),
    };
    Some(Build {
        target_dir,
        manifest,
        flags,
        key,
    })
}

/// Before `command` runs: cleans this workspace's packages in a shared target directory when another checkout built
/// that profile last. Says what it did; any failure only skips the check (the build itself goes ahead).
pub fn prepare(command: &[String]) {
    let Some(build) = parse(command) else { return };
    let Some(target) = build
        .target_dir
        .clone()
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR").map(PathBuf::from))
    else {
        // The workspace's own target folder: nobody else builds there.
        return;
    };
    let Some(root) = workspace_root(&build) else {
        return;
    };
    let stamp = target.join(STAMP);
    let text = std::fs::read_to_string(&stamp).unwrap_or_default();
    let last = last_builder(&text, &build.key);
    if last.as_deref().is_some_and(|l| same(l, &root)) {
        return;
    }
    let Some(members) = members(&build) else {
        return;
    };
    match &last {
        Some(other) => eprintln!(
            "fgate: {} in {} was last built by another checkout ({other}): cleaning this workspace's {} \
             package(s) there first, so cargo doesn't reuse them (switch_clean)",
            build.key,
            target.display(),
            members.len()
        ),
        None => eprintln!(
            "fgate: nobody is recorded as building {} in {}: cleaning this workspace's {} package(s) there once \
             (switch_clean)",
            build.key,
            target.display(),
            members.len()
        ),
    }
    let mut clean = Command::new("cargo");
    clean.arg("clean");
    for m in &members {
        clean.args(["-p", m]);
    }
    clean.args(&build.flags);
    if let Some(d) = &build.target_dir {
        clean.arg("--target-dir").arg(d);
    }
    if let Some(m) = &build.manifest {
        clean.arg("--manifest-path").arg(m);
    }
    match clean.status() {
        Ok(s) if s.success() => {
            let _ = std::fs::create_dir_all(&target);
            let _ = std::fs::write(&stamp, record(&text, &build.key, &root.to_string_lossy()));
        }
        Ok(s) => eprintln!("fgate: cargo clean failed ({s}); building without it"),
        Err(e) => eprintln!("fgate: could not run cargo clean ({e}); building without it"),
    }
}

/// The workspace the command builds (its root folder), as cargo finds it.
fn workspace_root(build: &Build) -> Option<PathBuf> {
    let mut c = Command::new("cargo");
    c.args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(m) = &build.manifest {
        c.arg("--manifest-path").arg(m);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    let toml = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    toml.parent().map(Path::to_path_buf)
}

/// The workspace's own packages, by name.
fn members(build: &Build) -> Option<Vec<String>> {
    let mut c = Command::new("cargo");
    c.args(["metadata", "--no-deps", "--format-version", "1"]);
    if let Some(m) = &build.manifest {
        c.arg("--manifest-path").arg(m);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let names: Vec<String> = json
        .get("packages")?
        .as_array()?
        .iter()
        .filter_map(|p| p.get("name")?.as_str().map(str::to_string))
        .collect();
    (!names.is_empty()).then_some(names)
}

/// Who the record says built `key` last.
fn last_builder(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (k, root) = l.split_once('\t')?;
        (k == key).then(|| root.to_string())
    })
}

/// The record with `key` now built by `root`.
fn record(text: &str, key: &str, root: &str) -> String {
    let mut out: Vec<String> = text
        .lines()
        .filter(|l| l.split_once('\t').is_some_and(|(k, _)| k != key))
        .map(str::to_string)
        .collect();
    out.push(format!("{key}\t{root}"));
    out.join("\n") + "\n"
}

/// Whether two workspace roots are the same folder (Windows paths ignore case and slash direction).
fn same(a: &str, b: &Path) -> bool {
    let norm = |s: &str| {
        let s = s.replace('\\', "/");
        let s = s.trim_end_matches('/');
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    norm(a) == norm(&b.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn finds_compiling_cargo_commands_and_their_profile() {
        let b = parse(&cmd(
            "cargo run -p viewer --release -- --building x --release",
        ))
        .unwrap();
        assert_eq!(
            (b.key.as_str(), b.flags.clone()),
            ("release", vec!["--release".to_string()])
        );
        let b = parse(&cmd("cargo.exe +nightly t -p sim")).unwrap();
        assert_eq!((b.key.as_str(), b.flags.len()), ("debug", 0));
        let b = parse(&cmd(
            "cargo clippy --workspace --profile=ci --target x86_64-pc-windows-msvc",
        ))
        .unwrap();
        assert_eq!(b.key, "x86_64-pc-windows-msvc/ci");
        assert_eq!(b.flags, cmd("--profile ci --target x86_64-pc-windows-msvc"));
        assert_eq!(parse(&cmd("cargo bench")).unwrap().key, "release");
        let b = parse(&cmd(
            "cargo build --target-dir E:/t --manifest-path a/Cargo.toml",
        ))
        .unwrap();
        assert_eq!(b.target_dir, Some(PathBuf::from("E:/t")));
        assert_eq!(b.manifest, Some(PathBuf::from("a/Cargo.toml")));
        // A flag's value before the subcommand is not the subcommand.
        let b = parse(&cmd(
            "cargo --target-dir E:/t --manifest-path x/Cargo.toml build",
        ))
        .unwrap();
        assert_eq!(
            (b.target_dir, b.key.as_str()),
            (Some(PathBuf::from("E:/t")), "debug")
        );
        assert!(parse(&cmd("cargo --manifest-path build fmt")).is_none());
    }

    #[test]
    fn leaves_other_commands_alone() {
        for c in [
            "cargo fmt --all",
            "cargo clean -p a",
            "cargo install fgate",
            "python shots.py",
            "cargo",
        ] {
            assert!(parse(&cmd(c)).is_none(), "{c}");
        }
    }

    #[test]
    fn the_record_keeps_one_builder_per_profile() {
        let t = record("", "debug", "E:/a");
        let t = record(&t, "release", "E:/b");
        let t = record(&t, "debug", "E:/c");
        assert_eq!(last_builder(&t, "debug").as_deref(), Some("E:/c"));
        assert_eq!(last_builder(&t, "release").as_deref(), Some("E:/b"));
        assert_eq!(last_builder(&t, "ci"), None);
        assert!(same("E:/code/game/", Path::new("E:/code/game")));
    }
}
