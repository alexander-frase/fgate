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
//!
//! That clean also deletes the executables a checkout has just built, as soon as the next checkout's build gets
//! the slot. So after a build, while the slot is still held, fgate copies the workspace's executables into the
//! checkout itself (`<checkout>/target/fgate-bin/<profile>/`), where no other checkout's build touches them
//! (`keep_bins`; 0: off). It copies only when the record says this checkout built the profile last, so a kept
//! executable is always this checkout's own code.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

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
    /// The requested cargo executable and optional rustup toolchain, also used by helper commands.
    cargo: String,
    toolchain: Option<String>,
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

impl Build {
    fn cargo(&self) -> Command {
        let mut command = Command::new(&self.cargo);
        if let Some(toolchain) = &self.toolchain {
            command.arg(toolchain);
        }
        command
    }
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
        cargo: program.clone(),
        toolchain: command.get(1).filter(|a| a.starts_with('+')).cloned(),
        target_dir,
        manifest,
        flags,
        key,
    })
}

/// A compiling cargo command into a target directory that other checkouts may share.
pub struct Shared {
    build: Build,
    /// The shared target directory.
    target: PathBuf,
    /// The workspace the command builds.
    root: PathBuf,
}

/// The shared build `command` makes, if it is a compiling cargo command into a target directory set from outside
/// the workspace (`--target-dir`, `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR`).
pub fn shared(command: &[String]) -> Option<Shared> {
    let build = parse(command)?;
    // Without one of these, the workspace's own target folder: nobody else builds there.
    let target = build
        .target_dir
        .clone()
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR").map(PathBuf::from))?;
    let root = workspace_root(&build)?;
    Some(Shared {
        build,
        target,
        root,
    })
}

/// Before the command runs: cleans this workspace's packages in the shared target directory when another checkout
/// built that profile last. Says what it did; any failure only skips the check (the build itself goes ahead).
pub fn prepare(shared: &Shared) {
    let Shared {
        build,
        target,
        root,
    } = shared;
    let stamp = target.join(STAMP);
    let text = std::fs::read_to_string(&stamp).unwrap_or_default();
    let last = last_builder(&text, &build.key);
    if last.as_deref().is_some_and(|l| same(l, root)) {
        return;
    }
    let Some(members) = metadata(build).map(|m| package_names(&m)) else {
        return;
    };
    if members.is_empty() {
        return;
    }
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
    let mut clean = build.cargo();
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
            let _ = std::fs::create_dir_all(target);
            let _ = std::fs::write(&stamp, record(&text, &build.key, &root.to_string_lossy()));
        }
        Ok(s) => eprintln!("fgate: cargo clean failed ({s}); building without it"),
        Err(e) => eprintln!("fgate: could not run cargo clean ({e}); building without it"),
    }
}

/// After the command, while its slot is still held: copies the workspace's executables in the shared target
/// directory into the checkout (`kept_dir`), where the next checkout's clean can't delete them. Only when the record
/// says this checkout built the profile last, so the copies are its own code. Says what it kept; a failure only
/// leaves a note.
pub fn keep(shared: &Shared) {
    let Shared {
        build,
        target,
        root,
    } = shared;
    let text = std::fs::read_to_string(target.join(STAMP)).unwrap_or_default();
    if !last_builder(&text, &build.key).is_some_and(|l| same(&l, root)) {
        return;
    }
    let Some(bins) = metadata(build).map(|m| bin_names(&m)) else {
        return;
    };
    let from = target.join(&build.key);
    let to = kept_dir(root, &build.key);
    let mut kept = Vec::new();
    for name in bins {
        let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
        let source = from.join(&file);
        if !source.is_file() {
            continue;
        }
        match copy_if_changed(&source, &to.join(&file)) {
            Ok(true) => kept.push(file),
            Ok(false) => kept.push(format!("{file} (unchanged)")),
            Err(e) => eprintln!(
                "fgate: could not keep {file} in {}: {e} (keep_bins)",
                to.display()
            ),
        }
    }
    if !kept.is_empty() {
        eprintln!(
            "fgate: kept {} in {}: run it from there, the shared target's copy goes with the next checkout's \
             build (keep_bins)",
            kept.join(", "),
            to.display()
        );
    }
}

/// Where a checkout's executables of one profile are kept.
fn kept_dir(root: &Path, key: &str) -> PathBuf {
    root.join("target").join("fgate-bin").join(key)
}

/// Copies `from` to `to` unless `to` already has its size and modification time. Says whether it copied. The copy
/// takes `from`'s modification time, and replaces `to` by renaming it away first, which Windows allows even while
/// `to` is running.
fn copy_if_changed(from: &Path, to: &Path) -> std::io::Result<bool> {
    let meta = std::fs::metadata(from)?;
    let modified: SystemTime = meta.modified()?;
    if let Ok(old) = std::fs::metadata(to) {
        if old.len() == meta.len() && old.modified().ok() == Some(modified) {
            return Ok(false);
        }
    }
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let fresh = to.with_extension("fgate-new");
    std::fs::copy(from, &fresh)?;
    std::fs::File::options()
        .write(true)
        .open(&fresh)?
        .set_modified(modified)?;
    if to.exists() {
        let old = to.with_extension("fgate-old");
        // Left over from a copy that was running last time; still running, it stays.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(to, &old)?;
        let _ = std::fs::remove_file(&old);
    }
    std::fs::rename(&fresh, to)?;
    Ok(true)
}

/// The workspace the command builds (its root folder), as cargo finds it.
fn workspace_root(build: &Build) -> Option<PathBuf> {
    let mut c = build.cargo();
    c.args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(m) = &build.manifest {
        c.arg("--manifest-path").arg(m);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    let toml = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    toml.parent().map(Path::to_path_buf)
}

/// The workspace's own packages, as `cargo metadata --no-deps` describes them.
fn metadata(build: &Build) -> Option<serde_json::Value> {
    let mut c = build.cargo();
    c.args(["metadata", "--no-deps", "--format-version", "1"]);
    if let Some(m) = &build.manifest {
        c.arg("--manifest-path").arg(m);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    serde_json::from_slice(&out.stdout).ok()
}

fn packages(metadata: &serde_json::Value) -> impl Iterator<Item = &serde_json::Value> {
    metadata
        .get("packages")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
}

/// The workspace's packages, by name.
fn package_names(metadata: &serde_json::Value) -> Vec<String> {
    packages(metadata)
        .filter_map(|p| p.get("name")?.as_str().map(str::to_string))
        .collect()
}

/// The workspace's executables (bin targets), by name.
fn bin_names(metadata: &serde_json::Value) -> Vec<String> {
    packages(metadata)
        .filter_map(|p| p.get("targets")?.as_array())
        .flatten()
        .filter(|t| {
            t.get("kind")
                .and_then(|k| k.as_array())
                .is_some_and(|k| k.iter().any(|k| k == "bin"))
        })
        .filter_map(|t| t.get("name")?.as_str().map(str::to_string))
        .collect()
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
    fn helper_commands_preserve_the_cargo_executable_and_toolchain() {
        for (input, executable, toolchain) in [
            ("cargo build", "cargo", None),
            ("cargo +nightly build", "cargo", Some("+nightly")),
            (
                "tools/cargo.exe +1.87.0 test",
                "tools/cargo.exe",
                Some("+1.87.0"),
            ),
            ("cargo run -- +nightly", "cargo", None),
        ] {
            let build = parse(&cmd(input)).unwrap();
            for subcommand in ["locate-project", "metadata", "clean"] {
                let mut helper = build.cargo();
                helper.arg(subcommand);
                assert_eq!(helper.get_program(), executable);
                let expected: Vec<_> = toolchain.into_iter().chain([subcommand]).collect();
                assert_eq!(helper.get_args().collect::<Vec<_>>(), expected);
            }
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

    #[test]
    fn finds_the_workspace_packages_and_executables() {
        let m: serde_json::Value = serde_json::from_str(
            r#"{"packages": [
                {"name": "game", "targets": [
                    {"name": "game", "kind": ["lib"]},
                    {"name": "game-viewer", "kind": ["bin"]},
                    {"name": "bench", "kind": ["bench"]}]},
                {"name": "tools", "targets": [{"name": "tools", "kind": ["bin"]}]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(package_names(&m), ["game", "tools"]);
        assert_eq!(bin_names(&m), ["game-viewer", "tools"]);
        assert!(bin_names(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn kept_executables_live_in_the_checkout_per_profile() {
        let root = Path::new("/w/game");
        assert_eq!(
            kept_dir(root, "release"),
            Path::new("/w/game/target/fgate-bin/release")
        );
        assert_eq!(
            kept_dir(root, "x86_64-pc-windows-msvc/debug"),
            Path::new("/w/game/target/fgate-bin/x86_64-pc-windows-msvc/debug")
        );
    }

    #[test]
    fn copies_only_what_changed() {
        let dir = std::env::temp_dir().join(format!("fgate-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let from = dir.join("game.exe");
        let to = dir.join("kept").join("game.exe");
        std::fs::write(&from, "one").unwrap();
        assert!(copy_if_changed(&from, &to).unwrap());
        assert!(!copy_if_changed(&from, &to).unwrap());
        std::fs::write(&from, "two!").unwrap();
        assert!(copy_if_changed(&from, &to).unwrap());
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "two!");
        assert!(
            !to.with_extension("fgate-new").exists() && !to.with_extension("fgate-old").exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
