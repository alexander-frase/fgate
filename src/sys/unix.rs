//! Linux and macOS: a gated command runs in its own process group at low priority (nice 10), and whatever it
//! leaves running ends with it. On Linux with a systemd user session it also runs in a transient systemd scope
//! that holds its memory and CPU ceilings (cgroups). Without one, and on macOS, there are no hard ceilings; the
//! line and the wait for free memory still apply. Slots are `flock` locks, which the system drops when the
//! holder ends, however it ends.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use super::Usage;

/// The running command's process group, for the signal handler (0: none).
static GROUP: AtomicI32 = AtomicI32::new(0);

pub struct Job {
    /// The command's process group (its first process's id), once started.
    group: AtomicI32,
    /// The most memory seen at once, in bytes.
    peak: AtomicU64,
    /// The command was given the terminal, which fgate takes back when it ends.
    terminal: bool,
    /// Where fgate's watcher inside a systemd scope leaves the scope's peak memory and out-of-memory kills.
    report: Option<PathBuf>,
}

impl Job {
    /// The command to run and the job it runs in (never a note: `status` and `watch` say when there are no
    /// ceilings here).
    pub fn prepare(
        program: &str,
        args: &[String],
        cpu_percent: u32,
        mem_bytes: u64,
    ) -> (Command, Option<Job>, Option<String>) {
        let mut report = None;
        let mut command = match (scope(cpu_percent, mem_bytes), std::env::current_exe()) {
            (Some(properties), Ok(me)) => {
                // systemd removes a scope, with its record of the peak and of out-of-memory kills, the moment it
                // is empty. So fgate's own watcher runs in it around the command, reads both once the command
                // ends, and leaves them in a file for this fgate.
                let path = std::env::temp_dir().join(format!("fgate-scope-{}", std::process::id()));
                let _ = std::fs::remove_file(&path);
                let mut command = Command::new("systemd-run");
                command.args(["--user", "--scope", "--quiet", "--collect"]);
                for p in properties {
                    command.arg("-p").arg(p);
                }
                command.arg("--").arg(me).arg(IN_SCOPE).arg(&path);
                command.arg(program).args(args);
                report = Some(path);
                command
            }
            _ => {
                let mut command = Command::new(program);
                command.args(args);
                command
            }
        };
        // Its own process group, so everything it starts can be found, signalled and ended together. When fgate
        // runs in the foreground of a terminal, the command's group gets the terminal, as a shell would do, so
        // Ctrl+C reaches it and it can read input.
        let terminal = in_foreground();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                libc::nice(10);
                libc::setpgid(0, 0);
                if terminal {
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid());
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let job = Job {
            group: AtomicI32::new(0),
            peak: AtomicU64::new(0),
            terminal,
            report,
        };
        (command, Some(job), None)
    }

    /// The command has started: signals fgate gets from now on go to its whole group.
    pub fn started(&self, child: &Child) {
        let group = child.id() as i32;
        self.group.store(group, Ordering::SeqCst);
        GROUP.store(group, Ordering::SeqCst);
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            // SAFETY: the handler only reads an atomic and calls kill, both async-signal-safe.
            unsafe {
                libc::signal(
                    signal,
                    forward as extern "C" fn(libc::c_int) as libc::sighandler_t,
                )
            };
        }
    }

    /// The command has ended: ends what it left running, and takes the terminal back.
    pub fn finish(&self) {
        if let Some(path) = &self.report {
            let _ = std::fs::remove_file(path);
        }
        let group = self.group.swap(0, Ordering::SeqCst);
        GROUP.store(0, Ordering::SeqCst);
        // SAFETY: plain system calls on our own process group and terminal.
        unsafe {
            if group > 0 {
                libc::kill(-group, libc::SIGKILL);
            }
            if self.terminal {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp());
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            }
        }
    }

    /// The most memory the command's processes held together at once, in bytes, as far as fgate saw.
    pub fn peak_memory(&self) -> u64 {
        let (peak, _) = self.scope_report();
        self.peak
            .fetch_max(peak, Ordering::SeqCst)
            .max(peak)
            .max(self.usage().peak)
    }

    /// Whether the memory ceiling stopped a process of the command (the system's out-of-memory killer, inside
    /// the scope), given the command's exit code.
    pub fn hit_ceiling(&self, code: i32) -> bool {
        let Some(path) = &self.report else {
            return false;
        };
        // No report at all, and the command was killed: the ceiling ended the whole scope, watcher and all.
        self.scope_report().1 > 0 || (!path.exists() && code == 128 + libc::SIGKILL)
    }

    /// What the watcher in the scope left: (peak memory in bytes, out-of-memory kills).
    fn scope_report(&self) -> (u64, u64) {
        let Some(path) = &self.report else {
            return (0, 0);
        };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut numbers = text.split_whitespace().map(|n| n.parse().unwrap_or(0));
        (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0))
    }

    /// Memory, CPU time and processes of the command's group, as they stand now.
    pub fn usage(&self) -> Usage {
        let group = self.group.load(Ordering::SeqCst);
        if group <= 0 {
            return Usage {
                peak: self.peak.load(Ordering::SeqCst),
                ..Usage::default()
            };
        }
        let mut usage = group_usage(group);
        let peak = usage.memory.max(scope_peak(group));
        usage.peak = self.peak.fetch_max(peak, Ordering::SeqCst).max(peak);
        usage
    }
}

/// The hidden first argument that makes fgate the watcher inside a systemd scope.
pub const IN_SCOPE: &str = "--in-scope";

/// fgate as the watcher inside a systemd scope: runs the command, then leaves the scope's peak memory and its
/// out-of-memory kills in the report file, and ends with the command's exit code.
pub fn in_scope(args: &[String]) -> ! {
    let [report, program, rest @ ..] = args else {
        eprintln!("fgate: {IN_SCOPE} needs a report file and a command");
        std::process::exit(2);
    };
    let status = Command::new(program).args(rest).status();
    let (peak, kills) = own_scope();
    let _ = std::fs::write(report, format!("{peak} {kills}\n"));
    match status {
        Ok(s) => std::process::exit(
            std::os::unix::process::ExitStatusExt::signal(&s)
                .map_or(s.code().unwrap_or(1), |sig| 128 + sig),
        ),
        Err(e) => {
            eprintln!("fgate: could not start the command: {e}");
            std::process::exit(127)
        }
    }
}

/// This process's own scope: (peak memory in bytes, out-of-memory kills).
#[cfg(target_os = "linux")]
fn own_scope() -> (u64, u64) {
    let Some(dir) = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("0::").map(str::to_string))
        })
    else {
        return (0, 0);
    };
    let read = |file: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup{dir}/{file}")).unwrap_or_default()
    };
    let peak = read("memory.peak").trim().parse().unwrap_or(0);
    let kills = read("memory.events")
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);
    (peak, kills)
}

#[cfg(not(target_os = "linux"))]
fn own_scope() -> (u64, u64) {
    (0, 0)
}

extern "C" fn forward(signal: libc::c_int) {
    let group = GROUP.load(Ordering::SeqCst);
    if group > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe { libc::kill(-group, signal) };
    }
}

/// Whether fgate runs in the foreground of a terminal.
fn in_foreground() -> bool {
    // SAFETY: plain queries of our own terminal and process group.
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
            && libc::tcgetpgrp(libc::STDIN_FILENO) == libc::getpgrp()
    }
}

/// The systemd properties that hold the ceilings, or none when there are none to hold or no systemd to hold
/// them. (That is a fact about the machine, not the command, so `status` and `watch` say it, not each run.)
#[cfg(target_os = "linux")]
fn scope(cpu_percent: u32, mem_bytes: u64) -> Option<Vec<String>> {
    let mut properties = Vec::new();
    if mem_bytes > 0 {
        // A hard ceiling, as on Windows: no swapping past it.
        properties.push(format!("MemoryMax={mem_bytes}"));
        properties.push("MemorySwapMax=0".to_string());
    }
    if cpu_percent > 0 && cpu_percent < 100 {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        properties.push(format!("CPUQuota={}%", cpu_percent as usize * threads));
    }
    if properties.is_empty() {
        return None;
    }
    // By default systemd stops the whole scope when the ceiling kills a process in it, fgate's watcher included.
    // Newer systemd lets the scope carry on, so the watcher can report what happened; older systemd doesn't know
    // the setting, and goes without.
    let mut kept = properties.clone();
    kept.push("OOMPolicy=continue".to_string());
    if systemd_user(&kept) {
        return Some(kept);
    }
    systemd_user(&properties).then_some(properties)
}

#[cfg(target_os = "linux")]
const NO_SYSTEMD: &str =
    "they need a systemd user session (`systemd-run --user`), which isn't running here";

/// Whether `systemd-run --user --scope` works here with these properties.
#[cfg(target_os = "linux")]
fn systemd_user(properties: &[String]) -> bool {
    let mut probe = Command::new("systemd-run");
    probe.args(["--user", "--scope", "--quiet", "--collect"]);
    for p in properties {
        probe.arg("-p").arg(p);
    }
    probe
        .arg("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(target_os = "linux"))]
fn scope(_cpu_percent: u32, _mem_bytes: u64) -> Option<Vec<String>> {
    // macOS has no way to hold a process tree to a memory or CPU ceiling.
    None
}

/// Why commands run without memory and CPU ceilings here, if they do.
#[cfg(target_os = "linux")]
pub fn ceilings() -> Option<String> {
    (!systemd_user(&[])).then(|| NO_SYSTEMD.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn ceilings() -> Option<String> {
    Some("macOS has no way to hold a process tree to a ceiling".into())
}

/// The scope's own record of its peak memory, on Linux 5.19 and later (0 when there is none).
#[cfg(target_os = "linux")]
fn scope_peak(group: i32) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{group}/cgroup")) else {
        return 0;
    };
    let Some(path) = text.lines().find_map(|l| l.strip_prefix("0::")) else {
        return 0;
    };
    if !path.ends_with(".scope") {
        return 0;
    }
    std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.peak"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn scope_peak(_group: i32) -> u64 {
    0
}

/// Memory, CPU time and count of the processes in a group, from /proc. The CPU time includes children the group's
/// processes have already waited for, so it keeps growing as compilers come and go.
#[cfg(target_os = "linux")]
fn group_usage(group: i32) -> Usage {
    let page = page_size();
    let tick = 10_000_000 / clock_ticks().max(1);
    let mut usage = Usage::default();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return usage;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The name in parentheses can hold spaces; the fields follow its closing one.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<u64> = rest
            .split_whitespace()
            .skip(1)
            .map(|f| f.parse().unwrap_or(0))
            .collect();
        // After the state: ppid, pgrp, … utime (11), stime, cutime, cstime, … rss (21), counted from ppid = 0.
        if fields.len() < 22 || fields[1] != group as u64 {
            continue;
        }
        usage.memory += fields[20] * page;
        usage.cpu += (fields[10] + fields[11] + fields[12] + fields[13]) * tick;
        usage.processes += 1;
    }
    usage
}

/// Memory, CPU time and count of the processes in a group.
#[cfg(target_os = "macos")]
fn group_usage(group: i32) -> Usage {
    /// `mach_timebase_info_data_t`: Mach time units to nanoseconds.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    let mut usage = Usage::default();
    let mut timebase = Timebase { numer: 1, denom: 1 };
    // SAFETY: plain data filled in by the calls; buffers sized as passed.
    unsafe {
        mach_timebase_info(&mut timebase);
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return usage;
        }
        let mut pids = vec![0i32; n as usize + 64];
        let bytes = (pids.len() * size_of::<i32>()) as libc::c_int;
        let n = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        for &pid in pids.iter().take(n.max(0) as usize) {
            let mut info: libc::proc_taskallinfo = std::mem::zeroed();
            let size = size_of::<libc::proc_taskallinfo>() as libc::c_int;
            let got = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKALLINFO,
                0,
                (&raw mut info).cast(),
                size,
            );
            if got != size || info.pbsd.pbi_pgid as i32 != group {
                continue;
            }
            usage.memory += info.ptinfo.pti_resident_size;
            // Mach time units to nanoseconds, then to 100 ns units.
            let ticks = info.ptinfo.pti_total_user + info.ptinfo.pti_total_system;
            usage.cpu += ticks * timebase.numer as u64 / timebase.denom.max(1) as u64 / 100;
            usage.processes += 1;
        }
    }
    usage
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: a plain query.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64 }
}

#[cfg(target_os = "linux")]
fn clock_ticks() -> u64 {
    // SAFETY: a plain query.
    unsafe { libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64 }
}

/// The machine's memory: (total, free), in bytes. Free is what can be had without swapping.
#[cfg(target_os = "linux")]
pub fn memory() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v * 1024)
    };
    match (kb("MemTotal:"), kb("MemAvailable:")) {
        (Some(total), Some(free)) => (total, free),
        _ => (0, u64::MAX),
    }
}

/// The machine's memory: (total, free), in bytes. Free counts free, inactive and speculative pages, which macOS
/// hands out without swapping.
#[cfg(target_os = "macos")]
pub fn memory() -> (u64, u64) {
    // SAFETY: plain data filled in by the calls, sized as passed.
    unsafe {
        let mut total: u64 = 0;
        let mut size = size_of::<u64>();
        if libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut total).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return (0, u64::MAX);
        }
        let mut stats: libc::vm_statistics64 = std::mem::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        if libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&raw mut stats).cast(),
            &mut count,
        ) != 0
        {
            return (total, u64::MAX);
        }
        let page = libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64;
        let pages =
            stats.free_count as u64 + stats.inactive_count as u64 + stats.speculative_count as u64;
        (total, pages * page)
    }
}

/// The machine's CPU time so far: (busy, all), summed over every thread, in clock ticks.
#[cfg(target_os = "linux")]
pub fn cpu_times() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else {
        return (0, 0);
    };
    // user nice system idle iowait irq softirq steal (guest time is already in user).
    let t: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|v| v.parse().unwrap_or(0))
        .collect();
    let all: u64 = t.iter().sum();
    let idle = t.get(3).copied().unwrap_or(0) + t.get(4).copied().unwrap_or(0);
    (all.saturating_sub(idle), all)
}

/// The machine's CPU time so far: (busy, all), summed over every core, in clock ticks.
#[cfg(target_os = "macos")]
pub fn cpu_times() -> (u64, u64) {
    /// `host_cpu_load_info`: ticks spent user, system, idle, nice.
    #[repr(C)]
    struct CpuLoad {
        ticks: [u32; 4],
    }
    unsafe extern "C" {
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    const HOST_CPU_LOAD_INFO: i32 = 3;
    // SAFETY: plain data filled in by the call, sized as passed.
    unsafe {
        let mut load = CpuLoad { ticks: [0; 4] };
        let mut count = 4u32;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        if host_statistics(host, HOST_CPU_LOAD_INFO, (&raw mut load).cast(), &mut count) != 0 {
            return (0, 0);
        }
        let [user, system, idle, nice] = load.ticks.map(u64::from);
        (user + system + nice, user + system + nice + idle)
    }
}

/// The local clock: (hour, minute, second).
pub fn local_time() -> (u32, u32, u32) {
    // SAFETY: plain data filled in by the calls.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return (0, 0, 0);
        }
        (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32)
    }
}

/// The hour of the local clock, 0–23.
pub fn local_hour() -> u32 {
    local_time().0
}

/// Lets `status` and `help` end quietly when what reads them stops early (`fgate status | head`).
pub fn quiet_broken_pipe() {
    // SAFETY: restores the default action, so a closed pipe ends the process without a message.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// Linux keeps state under $XDG_STATE_HOME (~/.local/state); macOS under ~/Library/Application Support.
pub(super) fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map_or_else(std::env::temp_dir, |h| {
            h.join("Library").join("Application Support")
        });
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir)
}

/// Takes the file's lock if nobody holds it. The system drops it when the process ends, however it ends.
pub fn try_lock(file: &File) -> bool {
    // SAFETY: a live descriptor; the call does not block.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

pub fn unlock(file: &File) {
    // SAFETY: as in try_lock.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

extern "C" fn on_quit(_: libc::c_int) {
    super::QUIT.store(true, Ordering::SeqCst);
}

/// Readies the terminal for `fgate watch`: Ctrl+C or the terminal closing sets `QUIT`.
pub fn catch_quit() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to atomics.
        unsafe {
            libc::signal(
                signal,
                on_quit as extern "C" fn(libc::c_int) as libc::sighandler_t,
            )
        };
    }
}

/// The terminal's (columns, rows).
pub fn console_size() -> (usize, usize) {
    // SAFETY: plain data filled in by the call.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) != 0 || size.ws_col == 0 {
            return (100, 40);
        }
        (size.ws_col as usize, size.ws_row.max(1) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_sees_its_processes_and_ends_them() {
        let args = ["-c".to_string(), "sleep 30 & sleep 30".to_string()];
        let (mut command, job, _) = Job::prepare("sh", &args, 0, 0);
        let job = job.unwrap();
        let mut child = command.spawn().unwrap();
        job.started(&child);
        std::thread::sleep(std::time::Duration::from_millis(500));
        let usage = job.usage();
        assert!(usage.processes >= 2, "processes: {}", usage.processes);
        assert!(usage.memory > 0 && usage.peak >= usage.memory);
        // Ending the group ends the shell and the sleep it left in the background.
        job.finish();
        let status = child.wait().unwrap();
        assert!(!status.success());
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(group_usage(child.id() as i32).processes, 0);
    }

    #[test]
    fn the_machine_reads_sensibly() {
        let (total, free) = memory();
        assert!(total > 0 && free > 0 && free <= total, "{total} {free}");
        let (busy, all) = cpu_times();
        assert!(all > 0 && busy <= all);
        let (h, m, s) = local_time();
        assert!(h < 24 && m < 60 && s < 61);
    }

    #[test]
    fn locks_exclude_other_holders() {
        let path = std::env::temp_dir().join(format!("fgate-lock-test-{}", std::process::id()));
        let a = File::create(&path).unwrap();
        let b = File::open(&path).unwrap();
        assert!(try_lock(&a));
        assert!(!try_lock(&b));
        unlock(&a);
        assert!(try_lock(&b));
        drop(b);
        let _ = std::fs::remove_file(path);
    }
}
