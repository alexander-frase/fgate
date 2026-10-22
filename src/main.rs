//! fgate, the build gate: many coding agents share one machine, and each heavy command waits its turn.
//!
//! ```text
//! fgate cargo test -p my_crate         a build slot: cargo build/check/test/clippy/run, test runs
//! fgate --gpu <program> …              the GPU slot: games, renderers, screenshot scripts
//! fgate status                         who holds the slots, who is waiting
//! fgate watch                          the same, live, with memory, CPU, each job's output and recent runs
//! fgate peek [name]                    a job's output as it comes, or how a finished run ended
//! fgate hook [options]                the agent hook: refuses ungated cargo, C++ (and GPU) commands
//! ```
//!
//! A waiting command keeps its place in a fair line and prints where it stands. Once it has a slot, the
//! command and everything it starts run at low priority within a CPU and memory ceiling (a Windows job, a
//! systemd scope on Linux; macOS has no such ceilings) and end together when fgate ends. Slots and places in
//! line are OS file locks, which the system drops when a process ends however it ends, so nothing is ever left
//! locked by a crashed or killed agent. Settings: `config.txt` in fgate's folder, which `fgate status` names
//! (written with defaults on first use).

mod config;
mod gate;
mod hook;
mod output;
mod switch;
mod sys;
mod watch;

use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use config::Config;

const HELP: &str = "fgate: the build gate (https://github.com/alexander-frase/fgate)
  fgate <command> [args…]      run a build or test command in a build slot (waits its turn)
  fgate --gpu <command> […]    run a GPU program (game, renderer, screenshot script) in the GPU slot
  fgate --now <command> […]    run a small command at once, skipping the line, within a small memory
                               ceiling (now_mem_gb); one that needs more belongs in the line
  fgate status                 show the slots and the line
  fgate watch                  a live view of the slots, the line and recent runs (Ctrl+C ends it)
  fgate peek [name]            follow a running job's output, or show how a finished run ended; name is the
                               checkout's name as watch shows it (or `fgate peek <class> <slot>`)
  fgate hook [options]        agent hook for Claude Code and Copilot (reads the tool call on stdin)
      --gpu a,b                  programs that must run under `fgate --gpu`
      --protect-branch <name>    refuse `git push` to this branch
      --protect-label <name>     refuse `gh` commands that name this label
      --no-admin-merge           refuse `gh pr merge --admin`
      --note <text>              added to every refusal
A compiling cargo command into a target directory other checkouts share (CARGO_TARGET_DIR) first cleans this
workspace's own packages there when another checkout built that profile last (switch_clean). After it, the
workspace's executables are copied into the checkout, target/fgate-bin/<profile>/, where no other checkout's
build deletes them (keep_bins). After `cargo publish` or `cargo package`, the workspace's packages are cleaned, so
the next build compiles the real sources, not the copy the verify step built.
When fgate's output goes to a pipe or file (an agent's shell), the command's output is passed on unchanged and its
last lines are kept for `fgate peek` and `fgate watch` (capture).
Settings: config.txt in fgate's folder (`fgate status` shows where), or FGATE_* environment variables.";

/// Set for everything fgate runs: a gated command that starts another `fgate …` already holds a slot.
const HELD_VAR: &str = "FGATE_HELD";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    if args.first().map(String::as_str) == Some(sys::IN_SCOPE) {
        sys::in_scope(&args[1..]);
    }
    if matches!(
        args.first().map(String::as_str),
        None | Some("-h" | "--help" | "help" | "status")
    ) {
        sys::quiet_broken_pipe();
    }
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Some("status") => {
            gate::print_status(&Config::load());
            ExitCode::SUCCESS
        }
        Some("watch") => watch::run(),
        Some("peek") => output::peek(&args[1..]),
        Some("hook") => hook::run(&args[1..]),
        Some("--gpu") => run(gate::Class::Gpu, &args[1..]),
        Some("--now") => run(gate::Class::Now, &args[1..]),
        Some(_) => run(gate::Class::Build, &args),
    }
}

fn run(class: gate::Class, command: &[String]) -> ! {
    let Some((program, args)) = command.split_first() else {
        eprintln!("fgate: no command given\n{HELP}");
        std::process::exit(2);
    };
    // Already inside a gated command: run straight away, it owns a slot.
    if std::env::var_os(HELD_VAR).is_some() {
        let config = Config::load();
        let shared = shared(&config, command);
        prepare(&config, shared.as_ref());
        let status = Command::new(program).args(args).status();
        after(&config, command, shared.as_ref(), &status);
        exit(status);
    }

    let config = Config::load();
    let line = short(command);
    let waited = Instant::now();
    let held = gate::acquire(&config, class, &line);
    let waited = waited.elapsed();
    // The settings as they are now: they may have changed while the command waited.
    let config = Config::load();
    // A target directory shared with other checkouts: their builds of this workspace's crates must not be reused.
    let shared = shared(&config, command);
    prepare(&config, shared.as_ref());

    let mem_gb = config.mem_gb_for(class);
    let mem_bytes = mem_gb as u64 * (1u64 << 30);
    let (mut child, job, note) = sys::Job::prepare(program, args, config.cpu_percent, mem_bytes);
    if let Some(note) = note {
        eprintln!("fgate: {note}");
    }
    let job = job.map(Arc::new);
    child.env(HELD_VAR, class.name());
    if class != gate::Class::Gpu {
        // cargo's and CMake's compiler jobs, unless the command sets its own.
        for var in ["CARGO_BUILD_JOBS", "CMAKE_BUILD_PARALLEL_LEVEL"] {
            if std::env::var_os(var).is_none() {
                child.env(var, config.jobs.to_string());
            }
        }
    }
    let mut capture = output::Capture::prepare(&config, class, held.index, &mut child);
    eprintln!(
        "fgate: {} slot {} {}{}— {}",
        class.name(),
        held.index,
        if waited.as_secs() >= 1 {
            format!("after {} in line ", gate::duration(waited))
        } else {
            String::new()
        },
        if config.night { "(night limits) " } else { "" },
        line
    );

    // Says what the job uses once a second, for `fgate watch`, until the slot is given back.
    let held = Arc::new(Mutex::new(Some(held)));
    if let Some(job) = job.clone() {
        let held = held.clone();
        std::thread::spawn(move || {
            loop {
                let usage = job.usage();
                match held.lock().as_deref() {
                    Ok(Some(held)) => held.report(&usage),
                    _ => break,
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let ran = Instant::now();
    let status = child.spawn().and_then(|mut running| {
        if let Some(job) = &job {
            job.started(&running);
        }
        if let Some(capture) = &mut capture {
            capture.start(&mut running);
        }
        running.wait()
    });
    let ran = ran.elapsed();
    if let Some(capture) = &mut capture {
        capture.drain();
    }
    let peak = job.as_deref().map(sys::Job::peak_memory).unwrap_or(0);
    let exit_code = status.as_ref().map_or(127, code);
    let hit = job.as_deref().is_some_and(|job| job.hit_ceiling(exit_code));
    let mut summary = format!("fgate: done in {}", gate::duration(ran));
    if peak > 0 {
        summary += &format!(", peak memory {}", gate::size(peak));
    }
    if mem_bytes > 0 && (hit || peak as f64 >= mem_bytes as f64 * 0.95) {
        summary += &if class == gate::Class::Now {
            format!(
                " — it reached the {mem_gb} GB ceiling of `fgate --now`, so an \"out of memory\" failure or a \
                 killed process above came from fgate, not the code: this command is too big to skip the line. \
                 Run it again without `--now`."
            )
        } else {
            format!(
                " — it reached the {mem_gb} GB ceiling, so an \"out of memory\" failure or a killed process \
                 above came from fgate, not the code (raise mem_gb in {})",
                config.file().display()
            )
        };
    }
    eprintln!("{summary}");
    // Before the slot goes to the next build, which may clean these executables away.
    after(&config, command, shared.as_ref(), &status);
    let done = gate::Done {
        ended: gate::now(),
        class: class.name().into(),
        place: gate::here(),
        command: line,
        ran,
        waited,
        peak,
        code: exit_code,
    };
    // Also before the slot is given back, so `fgate peek` finds the run once the slot is free.
    if let Some(capture) = capture {
        capture.finish(&done);
    }
    // Gives the slot back (the reporter stops with it).
    if let Ok(mut held) = held.lock() {
        held.take();
    }
    gate::record(&config, &done);
    // Ends what the command left running. On Windows that happens when fgate exits with the job still open
    // (closing it first would end fgate too, and replace the command's exit code).
    if let Some(job) = &job {
        job.finish();
    }
    exit(status)
}

/// The shared target directory `command` builds into, when a setting needs to know it.
fn shared(config: &Config, command: &[String]) -> Option<switch::Shared> {
    (config.switch_clean != 0 || config.keep_bins != 0)
        .then(|| switch::shared(command))
        .flatten()
}

/// Cleans another checkout's builds of this workspace away first (switch_clean).
fn prepare(config: &Config, shared: Option<&switch::Shared>) {
    if let Some(s) = shared {
        if config.switch_clean != 0 {
            switch::prepare(s);
        }
    }
}

/// After a command that succeeded: keeps a build's executables (keep_bins), and cleans up after a publish's
/// verify build.
fn after(
    config: &Config,
    command: &[String],
    shared: Option<&switch::Shared>,
    status: &std::io::Result<std::process::ExitStatus>,
) {
    if !status.as_ref().is_ok_and(|s| s.success()) {
        return;
    }
    if let Some(s) = shared {
        if config.keep_bins != 0 {
            switch::keep(s);
        }
    }
    if let Some(build) = switch::publishes(command) {
        switch::clean_after_publish(&build);
    }
}

/// Ends fgate with the command's own exit code (Windows codes can be large or negative, such as a crash's).
fn exit(status: std::io::Result<std::process::ExitStatus>) -> ! {
    match status {
        Ok(s) => std::process::exit(code(&s)),
        Err(e) => {
            eprintln!("fgate: could not start the command: {e}");
            std::process::exit(127)
        }
    }
}

/// A finished command's exit code; one ended by a signal gives 128 + the signal, as shells do.
fn code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

/// The command as the status shows it: the program without its folder, on one line, at most 400 characters.
fn short(command: &[String]) -> String {
    let program = command[0].replace('\\', "/");
    let program = program.rsplit('/').next().unwrap_or(&program);
    let rest = command[1..].iter().map(String::as_str);
    // One line: a multi-line argument (`python -c "…"`) would break the slot's file apart.
    let mut line = std::iter::once(program)
        .chain(rest)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() > 400 {
        line = line.chars().take(399).collect::<String>() + "…";
    }
    line
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_command_is_shown_on_one_line() {
        let command: Vec<String> = [
            "C:\\Python\\python.exe",
            "-c",
            "import time\nfor i in range(3):\n    print(i)",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            super::short(&command),
            "python.exe -c import time for i in range(3): print(i)"
        );
        assert_eq!(super::short(&["x".repeat(500)]).chars().count(), 400);
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!("fgate runs on Windows, Linux and macOS.");
