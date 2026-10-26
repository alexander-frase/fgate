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
//!
//! The clean, the build and the keep belong together: with two build slots, another checkout's clean could run
//! between this build and its keep (deleting what it built), or between its own clean and build, so that the record
//! names one checkout while the crates are another's. So a build into a shared target holds a lock there from before
//! its clean until after its keep: one per profile folder (`.fgate-lock-debug`, `.fgate-lock-release`), the same
//! folders cargo itself locks while it builds. A dev build and a release build into one target run side by side, as
//! cargo lets them. Two dev builds can't, so the second waits in fgate's line rather than in a slot, blocked in
//! cargo, and the line lets later jobs that build elsewhere go past it meanwhile (`gate`).

use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// The record in the target directory: one line per profile, `<profile key>\t<workspace root>`.
const STAMP: &str = ".fgate-checkouts";

/// The start of the lock files a build holds in a shared target directory from its clean to its keep; the profile
/// folder's name follows (`.fgate-lock-release`).
const LOCK: &str = ".fgate-lock";

/// Set on a command that holds locks in shared targets: those lock files (a path list), so a nested
/// `fgate cargo …` that needs one of them runs under its parent's lock instead of waiting for its own parent.
pub const LOCK_VAR: &str = "FGATE_TARGET_LOCK";

/// A held lock on a profile folder in a shared target directory; dropping it (or the process ending) gives it back.
pub struct TargetLock {
    _file: File,
    pub path: PathBuf,
}

impl TargetLock {
    /// `LOCK_VAR` for what the holder runs: the locks it was given, and this one.
    pub fn passed_on(&self) -> OsString {
        let inherited = std::env::var_os(LOCK_VAR).unwrap_or_default();
        let held = std::env::split_paths(&inherited).chain([self.path.clone()]);
        std::env::join_paths(held).unwrap_or_else(|_| self.path.clone().into_os_string())
    }
}

/// What came of trying to take a lock.
pub enum Take {
    Held(TargetLock),
    /// Another build holds it.
    Busy,
    /// The lock file can't be made: the build goes ahead without it.
    Without,
}

/// The lock a cargo command needs while it builds into a target directory set from outside its workspace
/// (`--target-dir`, `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR`): the lock file of the profile folder it builds
/// in. A build for another target triple shares the host's lock, since cargo builds its build scripts in the host's
/// folder and locks that too. None for a command that doesn't build, builds into its workspace's own `target/`, or
/// runs under a parent that holds this lock already.
pub fn lock_path(command: &[String]) -> Option<PathBuf> {
    let (sub, build) = parse_any(command)?;
    let builds =
        COMPILING.contains(&sub.as_str()) || sub == "install" || publishes(command).is_some();
    if !builds {
        return None;
    }
    let target = target_dir(&build)?;
    let target = std::path::absolute(&target).unwrap_or(target);
    let path = target.join(format!("{LOCK}-{}", build.folder()));
    let inherited = std::env::var_os(LOCK_VAR).unwrap_or_default();
    if std::env::split_paths(&inherited).any(|held| same(&held.to_string_lossy(), &path)) {
        return None;
    }
    Some(path)
}

/// A lock file's path as every waiter writes it, so two waiters' locks compare as text.
pub fn normal(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// `E:\code\game\target\release`: the profile folder a lock file guards, for messages, written with the lock path's
/// own slashes.
pub fn lock_name(path: &Path) -> String {
    let path = path.to_string_lossy();
    let (dir, file) = path.rsplit_once(['/', '\\']).unwrap_or(("", &path));
    let folder = file
        .strip_prefix(LOCK)
        .unwrap_or(file)
        .trim_start_matches('-');
    // The separator the path itself uses there.
    let slash = &path[dir.len()..path.len() - file.len()];
    format!("{dir}{slash}{folder}")
}

fn open_lock(path: &Path) -> Option<File> {
    std::fs::create_dir_all(path.parent()?).ok()?;
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()
}

/// Takes the lock if it is free, and writes `text` (who holds it) into it for those who wait.
pub fn take(path: &Path, text: &str) -> Take {
    let Some(mut file) = open_lock(path) else {
        return Take::Without;
    };
    if !crate::sys::try_lock(&file) {
        return Take::Busy;
    }
    // The lock sits far past the text (as a slot's does), so the text can be written and read while it is held.
    let _ = file.set_len(0);
    let _ = file.write_all(text.as_bytes());
    Take::Held(TargetLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// Whether a build holds the lock now.
pub fn busy(path: &Path) -> bool {
    let Ok(file) = File::options().read(true).write(true).open(path) else {
        return false;
    };
    if crate::sys::try_lock(&file) {
        crate::sys::unlock(&file);
        false
    } else {
        true
    }
}

/// What the lock's holder wrote: since when, where from, what (as a slot says it).
pub fn holder(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().filter(|t| !t.is_empty())
}

/// Takes the lock, waiting while another build holds it, for a command that already holds a slot (a nested
/// `fgate cargo …`, a script's cargo). None when the lock file can't be made: the build then goes ahead without it.
pub fn lock(path: &Path, text: &str) -> Option<TargetLock> {
    let started = std::time::Instant::now();
    let mut said = false;
    loop {
        match take(path, text) {
            Take::Held(lock) => return Some(lock),
            Take::Without => return None,
            Take::Busy => {}
        }
        if !said && started.elapsed() > std::time::Duration::from_secs(1) {
            eprintln!(
                "fgate: waiting for another build into {} to finish (a build's clean, build and kept executables go \
                 together){}",
                lock_name(path),
                holder(path).map_or(String::new(), |t| format!(": {}", crate::gate::summary(&t)))
            );
            said = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

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
    /// The profile folder without the target triple's: `debug`, `release`, a custom profile's name.
    fn folder(&self) -> &str {
        self.key.rsplit('/').next().unwrap_or(&self.key)
    }

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
    let (sub, build) = parse_any(command)?;
    COMPILING.contains(&sub.as_str()).then_some(build)
}

/// Whether the command's program is cargo (`cargo`, `cargo.exe`, a path to it).
pub fn is_cargo(command: &[String]) -> bool {
    command.first().is_some_and(|program| {
        Path::new(program)
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().to_lowercase() == "cargo")
    })
}

/// What a cargo command line says, with its subcommand, whatever the subcommand.
fn parse_any(command: &[String]) -> Option<(String, Build)> {
    let program = command.first()?;
    if !is_cargo(command) {
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
        } else if a == "--debug" {
            // `cargo install --debug`.
            profile = Some("dev".into());
        } else if sub.is_none() && !a.starts_with('-') && !a.starts_with('+') {
            // The toolchain (`+nightly`) and cargo's own options come before the subcommand.
            sub = Some(a.to_string());
        }
        i += 1;
    }
    let sub = sub?;
    let default = match sub.as_str() {
        "bench" => "bench",
        "install" => "release",
        _ => "dev",
    };
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
    let build = Build {
        cargo: program.clone(),
        toolchain: command.get(1).filter(|a| a.starts_with('+')).cloned(),
        target_dir,
        manifest,
        flags,
        key,
    };
    Some((sub, build))
}

/// The packaging a command does, if it is a `cargo publish` or `cargo package` that verifies the package by
/// building it.
pub fn publishes(command: &[String]) -> Option<Build> {
    let (sub, build) = parse_any(command)?;
    let skips = command
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--no-verify" || a == "--list" || a == "-l");
    (["publish", "package"].contains(&sub.as_str()) && !skips).then_some(build)
}

/// After a `cargo publish` or `cargo package` that succeeded: its verify step built the packaged copy in
/// `target/package/<name>-<version>/` under the same names as the workspace's own build, and cargo's records then
/// point at the copy's sources, so a later `cargo build` takes the published version for fresh and stops rebuilding.
/// Cleaning the workspace's packages (their dev builds) makes the next build start from the real sources.
pub fn clean_after_publish(build: &Build) {
    let Some(members) = metadata(build).map(|m| package_names(&m)) else {
        return;
    };
    if members.is_empty() {
        return;
    }
    let mut clean = build.cargo();
    clean.arg("clean").arg("--quiet");
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
        Ok(s) if s.success() => eprintln!(
            "fgate: cleaned this workspace's {} package(s) after the verify build, so the next cargo build \
             compiles from the real sources, not the packaged copy",
            members.len()
        ),
        Ok(s) => eprintln!("fgate: cargo clean after the verify build failed ({s})"),
        Err(e) => eprintln!("fgate: could not run cargo clean after the verify build ({e})"),
    }
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
    let target = target_dir(&build)?;
    let root = workspace_root(&build)?;
    Some(Shared {
        build,
        target,
        root,
    })
}

/// The target directory a command builds into when it is set from outside the workspace. Without one, the
/// workspace's own target folder: nobody else builds there.
fn target_dir(build: &Build) -> Option<PathBuf> {
    build
        .target_dir
        .clone()
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BUILD_TARGET_DIR").map(PathBuf::from))
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
pub fn copy_if_changed(from: &Path, to: &Path) -> std::io::Result<bool> {
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
    fn finds_publishing_that_builds_the_package() {
        for c in [
            "cargo publish",
            "cargo +stable publish --dry-run",
            "cargo package --allow-dirty",
            "cargo.exe publish -p fgate --manifest-path x/Cargo.toml",
        ] {
            assert!(publishes(&cmd(c)).is_some(), "{c}");
        }
        for c in [
            "cargo publish --no-verify",
            "cargo package --list",
            "cargo build",
            "cargo run -- publish",
            "cargo login",
        ] {
            assert!(publishes(&cmd(c)).is_none(), "{c}");
        }
        let b = publishes(&cmd("cargo publish --target x86_64-pc-windows-msvc")).unwrap();
        assert_eq!(b.flags, cmd("--target x86_64-pc-windows-msvc"));
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
    fn each_profile_folder_has_its_own_lock() {
        let lock = |c: &str| lock_path(&cmd(c)).map(|p| normal(&p));
        let dev = lock("cargo build --target-dir /t").unwrap();
        assert!(dev.ends_with("/t/.fgate-lock-debug"), "{dev}");
        // Everything that builds in target/debug shares cargo's lock there, and so fgate's.
        for c in [
            "cargo test --target-dir /t",
            "cargo clippy --all-targets --target-dir=/t",
            "cargo check --profile test --target-dir /t",
            "cargo build --target x86_64-pc-windows-msvc --target-dir /t",
            "cargo publish --dry-run --target-dir /t",
            "cargo install --debug --path . --target-dir /t",
        ] {
            assert_eq!(lock(c).as_deref(), Some(dev.as_str()), "{c}");
        }
        let release = lock("cargo build --release --target-dir /t").unwrap();
        assert!(release.ends_with("/t/.fgate-lock-release"), "{release}");
        for c in [
            "cargo bench --target-dir /t",
            "cargo run -r -p game --target-dir /t",
            "cargo install --path . --target-dir /t",
        ] {
            assert_eq!(lock(c).as_deref(), Some(release.as_str()), "{c}");
        }
        let ci = lock("cargo clippy --profile ci --target-dir /t").unwrap();
        assert!(ci.ends_with("/t/.fgate-lock-ci"), "{ci}");
        for c in [
            "cargo fmt --target-dir /t",
            "cargo publish --no-verify --target-dir /t",
        ] {
            assert_eq!(lock(c), None, "{c}");
        }
        assert_eq!(lock_name(Path::new("/t/.fgate-lock-release")), "/t/release");
        assert_eq!(
            lock_name(Path::new(r"E:\game\target\.fgate-lock-debug")),
            r"E:\game\target\debug"
        );
    }

    #[test]
    fn a_build_holds_its_profile_folder_until_it_is_done() {
        let dir = std::env::temp_dir().join(format!("fgate-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (debug, release) = (
            dir.join(".fgate-lock-debug"),
            dir.join(".fgate-lock-release"),
        );
        assert!(!busy(&debug), "nobody built here yet");
        let Take::Held(held) = take(&debug, "1\n2\na\ncargo build") else {
            panic!("the lock is free");
        };
        assert!(busy(&debug), "another dev build waits");
        assert!(matches!(take(&debug, "b"), Take::Busy));
        assert_eq!(holder(&debug).as_deref(), Some("1\n2\na\ncargo build"));
        let Take::Held(other) = take(&release, "c") else {
            panic!("a release build goes beside it");
        };
        drop(held);
        assert!(!busy(&debug), "and goes once it is done");
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
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
