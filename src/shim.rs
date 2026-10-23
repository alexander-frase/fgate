//! Cargo inside a gated script (`fgate sh build.sh`, `fgate python shots.py`) gets the same care as `fgate cargo …`.
//!
//! fgate only sees the command it was given. When that is a script that runs `cargo build` itself, fgate never learns
//! which target directory and profile it builds, so it can neither clean another checkout's crates out of a shared
//! target first (switch_clean) nor keep the executables afterwards (keep_bins), and the record of who built a profile
//! last goes stale: the next checkout's build trusts it and runs the script's crates as its own.
//!
//! So a gated command that is not cargo itself gets a `cargo` first on its PATH: a copy of fgate in fgate's folder
//! (`shim/cargo`, or `cargo.exe` on Windows). When the script runs `cargo …`, that copy starts, finds the real cargo
//! further along the PATH, and runs it as a nested gated command: no second wait (the script holds the slot), but the
//! same clean, record and kept executables as `fgate cargo …`. A script that names cargo by its full path, or that
//! clears the PATH, bypasses it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Set on a command whose PATH starts with the shim: the shim's folder.
pub const DIR_VAR: &str = "FGATE_SHIM_DIR";

/// cargo's file name on this system.
fn file_name() -> String {
    format!("cargo{}", std::env::consts::EXE_SUFFIX)
}

/// When this process is the shim (fgate's copy named cargo, started from a gated command): the real cargo to run.
pub fn invoked_as_cargo() -> Option<PathBuf> {
    let dir = std::env::var_os(DIR_VAR)?;
    let me = std::env::current_exe().ok()?;
    let stem = me.file_stem()?.to_string_lossy().to_lowercase();
    if stem != "cargo" {
        return None;
    }
    let found = real_cargo(Path::new(&dir), std::env::var_os("PATH").as_deref());
    if found.is_none() {
        eprintln!(
            "fgate: no cargo found on the PATH after fgate's shim in {}",
            dir.to_string_lossy()
        );
    }
    found
}

/// The first cargo on `path` outside the shim's folder.
fn real_cargo(shim: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|d| !same_dir(d, shim))
        .map(|d| d.join(file_name()))
        .find(|p| p.is_file())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Puts the shim in place, a copy of this fgate in `dir`, and returns `dir`. Copies only when this fgate changed, so
/// an update reaches the shim with the next gated command. None when it can't (the command then runs without it).
pub fn install(dir: &Path) -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    crate::switch::copy_if_changed(&me, &dir.join(file_name())).ok()?;
    Some(dir.to_path_buf())
}

/// The shim's folder in fgate's folder (`config.dir`, which `FGATE_DIR` can move).
pub fn dir_in(settings: &Path) -> PathBuf {
    settings.join("shim")
}

/// The PATH with `dir` first.
pub fn path_with(dir: &Path) -> Option<OsString> {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let rest: Vec<PathBuf> = std::env::split_paths(&old)
        .filter(|d| !same_dir(d, dir))
        .collect();
    std::env::join_paths(std::iter::once(dir.to_path_buf()).chain(rest)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fgate-shim-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_real_cargo_is_the_first_after_the_shim() {
        let root = temp("path");
        let (shim, real, later) = (root.join("shim"), root.join("real"), root.join("later"));
        for d in [&shim, &real, &later] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join(file_name()), "").unwrap();
        }
        let path = std::env::join_paths([&shim, &root.join("missing"), &real, &later]).unwrap();
        assert_eq!(real_cargo(&shim, Some(&path)), Some(real.join(file_name())));
        let only_shim = std::env::join_paths([&shim]).unwrap();
        assert_eq!(real_cargo(&shim, Some(&only_shim)), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_shim_goes_first_on_the_path_once() {
        let root = temp("first");
        let path = path_with(&root).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        assert_eq!(dirs[0], root);
        assert_eq!(dirs.iter().filter(|d| same_dir(d, &root)).count(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_shim_is_a_copy_of_fgate_named_cargo() {
        let root = temp("install");
        let dir = install(&root).unwrap();
        let shim = dir.join(file_name());
        let me = std::env::current_exe().unwrap();
        assert_eq!(
            std::fs::metadata(&shim).unwrap().len(),
            std::fs::metadata(&me).unwrap().len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
