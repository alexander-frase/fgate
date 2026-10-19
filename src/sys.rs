//! What differs between operating systems, behind one interface: the job a gated command runs in (its priority,
//! CPU and memory ceilings, usage, and ending everything it started), the slot locks, the machine's memory, CPU
//! and clock, where the settings live, and the terminal for `fgate watch`.

use std::sync::atomic::AtomicBool;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// What a running job uses, for `fgate watch`.
#[derive(Clone, Copy, Default)]
pub struct Usage {
    /// Memory its processes hold together now, in bytes (0 where the system can't say).
    pub memory: u64,
    /// The most they held at once, in bytes.
    pub peak: u64,
    /// CPU time of all its processes so far, in 100 ns units.
    pub cpu: u64,
    /// Processes running in it.
    pub processes: u32,
}

/// Set when `fgate watch` is asked to end (Ctrl+C, the window closing); see `catch_quit`.
pub static QUIT: AtomicBool = AtomicBool::new(false);

/// Where fgate keeps its settings, slots, line and history when `FGATE_DIR` doesn't say.
pub fn settings_dir() -> std::path::PathBuf {
    default_dir().join("fgate")
}
