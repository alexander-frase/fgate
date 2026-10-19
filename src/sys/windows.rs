//! Windows: a gated command runs in a Windows job (below-normal priority, a hard CPU ceiling, a memory ceiling
//! for everything it starts together, and all of it killed when fgate ends, so no compiler is left running
//! alone). Slots are byte-range file locks; the terminal is the Windows console.

use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, SYSTEMTIME};
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
    SetConsoleMode,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
    JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PRIORITY_CLASS,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::SystemInformation::{
    GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows_sys::Win32::System::Threading::{
    BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetSystemTimes,
};

use super::Usage;

pub struct Job(HANDLE);

// SAFETY: a job handle is a kernel handle; any thread may query it.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

/// `JOBOBJECT_MEMORY_USAGE_INFORMATION`, which windows-sys does not list.
#[repr(C)]
#[derive(Default)]
struct MemoryUsage {
    job_memory: u64,
    peak_job_memory_used: u64,
}
const JOB_OBJECT_MEMORY_USAGE_INFORMATION: i32 = 28;

impl Job {
    /// The command to run and the job it runs in, or why it runs without one. fgate itself joins the job, and
    /// everything it starts from then on joins too.
    pub fn prepare(
        program: &str,
        args: &[String],
        cpu_percent: u32,
        mem_bytes: u64,
    ) -> (Command, Option<Job>, Option<String>) {
        let mut command = Command::new(program);
        command.args(args);
        match Job::contain(cpu_percent, mem_bytes) {
            Ok(job) => (command, Some(job), None),
            Err(e) => (
                command,
                None,
                Some(format!("running without CPU and memory ceilings ({e})")),
            ),
        }
    }

    /// The command has started (it joined the job by itself).
    pub fn started(&self, _child: &Child) {}

    /// The command has ended. What it left running ends when fgate does and the job closes.
    pub fn finish(&self) {}

    fn contain(cpu_percent: u32, mem_bytes: u64) -> Result<Job, String> {
        // SAFETY: plain Win32 calls on a handle owned here; the structs are plain data, sized as the API expects.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(last_error("CreateJobObject"));
            }
            let job = Job(handle);

            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_PRIORITY_CLASS;
            limits.BasicLimitInformation.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS;
            if mem_bytes > 0 {
                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                limits.JobMemoryLimit = mem_bytes as usize;
            }
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(last_error("job limits"));
            }

            if cpu_percent > 0 && cpu_percent < 100 {
                let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = std::mem::zeroed();
                cpu.ControlFlags =
                    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                // In hundredths of a percent of all threads.
                cpu.Anonymous.CpuRate = cpu_percent * 100;
                if SetInformationJobObject(
                    handle,
                    JobObjectCpuRateControlInformation,
                    (&raw const cpu).cast(),
                    size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                ) == 0
                {
                    eprintln!("fgate: no CPU ceiling ({})", last_error("CPU rate"));
                }
            }

            if AssignProcessToJobObject(handle, GetCurrentProcess()) == 0 {
                return Err(last_error("joining the job"));
            }
            Ok(job)
        }
    }

    /// The most memory the job's processes held together at once, in bytes.
    pub fn peak_memory(&self) -> u64 {
        // SAFETY: as in contain.
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let ok = QueryInformationJobObject(
                self.0,
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            );
            if ok == 0 {
                0
            } else {
                info.PeakJobMemoryUsed as u64
            }
        }
    }
}

impl Job {
    /// Memory, CPU time and processes, as they stand now.
    pub fn usage(&self) -> Usage {
        // SAFETY: as in contain; each struct is sized as its class expects.
        unsafe {
            let mut basic: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let mut usage = Usage {
                peak: self.peak_memory(),
                ..Usage::default()
            };
            if QueryInformationJobObject(
                self.0,
                JobObjectBasicAccountingInformation,
                (&raw mut basic).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            ) != 0
            {
                usage.cpu = (basic.TotalUserTime + basic.TotalKernelTime).max(0) as u64;
                usage.processes = basic.ActiveProcesses;
            }
            let mut memory = MemoryUsage::default();
            if QueryInformationJobObject(
                self.0,
                JOB_OBJECT_MEMORY_USAGE_INFORMATION,
                (&raw mut memory).cast(),
                size_of::<MemoryUsage>() as u32,
                std::ptr::null_mut(),
            ) != 0
            {
                usage.memory = memory.job_memory;
            }
            usage
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is ours. Closing it kills whatever is still in the job.
        unsafe { CloseHandle(self.0) };
    }
}

/// The hour of the local clock, 0–23.
pub fn local_hour() -> u32 {
    // SAFETY: plain data filled in by the call.
    unsafe {
        let mut now: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut now);
        now.wHour as u32
    }
}

/// The machine's memory: (total, free), in bytes.
pub fn memory() -> (u64, u64) {
    // SAFETY: plain data, dwLength set as the API requires.
    unsafe {
        let mut status: MEMORYSTATUSEX = std::mem::zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut status) == 0 {
            return (0, u64::MAX);
        }
        (status.ullTotalPhys, status.ullAvailPhys)
    }
}

/// The machine's CPU time so far: (busy, all), in 100 ns units summed over every thread.
pub fn cpu_times() -> (u64, u64) {
    let join = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    // SAFETY: plain data filled in by the call.
    unsafe {
        let (mut idle, mut kernel, mut user): (FILETIME, FILETIME, FILETIME) = std::mem::zeroed();
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return (0, 0);
        }
        // Kernel time includes idle time.
        let all = join(kernel) + join(user);
        (all.saturating_sub(join(idle)), all)
    }
}

/// The local clock: (hour, minute, second).
pub fn local_time() -> (u32, u32, u32) {
    // SAFETY: plain data filled in by the call.
    unsafe {
        let mut now: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut now);
        (now.wHour as u32, now.wMinute as u32, now.wSecond as u32)
    }
}

/// Why commands run without memory and CPU ceilings here, if they do: on Windows they always have them.
pub fn ceilings() -> Option<String> {
    None
}

/// Windows has no broken-pipe signal; nothing to do.
pub fn quiet_broken_pipe() {}

/// Windows keeps per-user program data under %LOCALAPPDATA%.
pub(super) fn default_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// The byte a slot or ticket lock sits on: far past anything ever written, because Windows locks keep others from
/// reading the locked range, and the text must stay readable.
const LOCK_AT_HIGH: u32 = 0x4000_0000;

fn overlapped() -> OVERLAPPED {
    // SAFETY: OVERLAPPED is plain data; all zero is its empty value.
    let mut o: OVERLAPPED = unsafe { std::mem::zeroed() };
    o.Anonymous.Anonymous.OffsetHigh = LOCK_AT_HIGH;
    o
}

/// Takes the file's lock if nobody holds it. Windows drops it when the process ends, however it ends.
pub fn try_lock(file: &File) -> bool {
    let mut o = overlapped();
    let flags = LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY;
    // SAFETY: a live handle and a valid OVERLAPPED; the call does not block.
    unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut o) != 0 }
}

pub fn unlock(file: &File) {
    let mut o = overlapped();
    // SAFETY: as in try_lock.
    unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut o) };
}

unsafe extern "system" fn on_ctrl(_: u32) -> i32 {
    super::QUIT.store(true, Ordering::SeqCst);
    1
}

/// Readies the console for `fgate watch`: escape sequences on, and Ctrl+C or closing sets `QUIT`.
pub fn catch_quit() {
    // SAFETY: plain console calls on this process's own output handle.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(out, &mut mode) != 0 {
            SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

/// The console window's (columns, rows).
pub fn console_size() -> (usize, usize) {
    // SAFETY: plain data filled in by the call.
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
            return (100, 40);
        }
        let w = info.srWindow;
        (
            (w.Right - w.Left + 1).max(1) as usize,
            (w.Bottom - w.Top + 1).max(1) as usize,
        )
    }
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}
