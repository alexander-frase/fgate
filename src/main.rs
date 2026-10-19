//! fgate, the build gate: many coding agents share one machine, and each heavy command waits its turn.
//!
//! ```text
//! fgate cargo test -p my_crate         a build slot: cargo build/check/test/clippy/run, test runs
//! fgate --gpu <program> …              the GPU slot: games, renderers, screenshot scripts
//! fgate status                         who holds the slots, who is waiting
//! fgate watch                          the same, live, with memory, CPU and recent runs
//! fgate hook [options]                 the Claude Code hook: refuses ungated cargo (and GPU) commands
//! ```
//!
//! A waiting command keeps its place in a fair line and prints where it stands. Once it has a slot, the
//! command and everything it starts run in a Windows job: below-normal priority, a CPU ceiling, a memory
//! ceiling, and killed together when fgate ends. Slots and places in line are OS file locks, which Windows
//! drops when a process ends however it ends, so nothing is ever left locked by a crashed or killed agent.
//! Settings: `%LOCALAPPDATA%\fgate\config.txt` (written with defaults on first use).

#[cfg(windows)]
mod config;
#[cfg(windows)]
mod gate;
#[cfg(windows)]
mod hook;
#[cfg(windows)]
mod job;
#[cfg(windows)]
mod watch;

#[cfg(windows)]
use std::process::{Command, ExitCode};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[cfg(windows)]
use config::Config;

#[cfg(windows)]
const HELP: &str = "fgate: the build gate (https://github.com/alexander-frase/fgate)
  fgate <command> [args…]      run a build or test command in a build slot (waits its turn)
  fgate --gpu <command> […]    run a GPU program (game, renderer, screenshot script) in the GPU slot
  fgate status                 show the slots and the line
  fgate watch                  a live view of the slots, the line and recent runs (Ctrl+C ends it)
  fgate hook [options]         Claude Code PreToolUse hook (reads the tool call on stdin)
      --gpu a,b                  programs that must run under `fgate --gpu`
      --protect-branch <name>    refuse `git push` to this branch
      --protect-label <name>     refuse `gh` commands that name this label
      --no-admin-merge           refuse `gh pr merge --admin`
      --note <text>              added to every refusal
Settings: %LOCALAPPDATA%\\fgate\\config.txt, or FGATE_* environment variables.";

#[cfg(windows)]
/// Set for everything fgate runs: a gated command that starts another `fgate …` already holds a slot.
const HELD_VAR: &str = "FGATE_HELD";

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        Some("hook") => hook::run(&args[1..]),
        Some("--gpu") => run(gate::Class::Gpu, &args[1..]),
        Some(_) => run(gate::Class::Build, &args),
    }
}

#[cfg(windows)]
fn run(class: gate::Class, command: &[String]) -> ! {
    let Some((program, args)) = command.split_first() else {
        eprintln!("fgate: no command given\n{HELP}");
        std::process::exit(2);
    };
    let mut child = Command::new(program);
    child.args(args);

    // Already inside a gated command: run straight away, it owns a slot.
    if std::env::var_os(HELD_VAR).is_some() {
        exit(child.status());
    }

    let config = Config::load();
    let line = short(command);
    let waited = Instant::now();
    let held = gate::acquire(&config, class, &line);
    let waited = waited.elapsed();
    // The settings as they are now: they may have changed while the command waited.
    let config = Config::load();

    let job = match job::Job::contain(config.cpu_percent, config.mem_bytes()) {
        Ok(job) => Some(Arc::new(job)),
        Err(e) => {
            eprintln!("fgate: running without CPU and memory ceilings ({e})");
            None
        }
    };
    child.env(HELD_VAR, class.name());
    if class == gate::Class::Build && std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        child.env("CARGO_BUILD_JOBS", config.jobs.to_string());
    }
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
    let status = child.status();
    let ran = ran.elapsed();
    let peak = job.as_deref().map(job::Job::peak_memory).unwrap_or(0);
    let mut summary = format!("fgate: done in {}", gate::duration(ran));
    if peak > 0 {
        summary += &format!(", peak memory {}", gate::size(peak));
        if config.mem_bytes() > 0 && peak as f64 >= config.mem_bytes() as f64 * 0.95 {
            summary += &format!(
                " — it reached the {} GB ceiling, so an \"out of memory\" failure above came from fgate, not the code \
                 (raise mem_gb in {})",
                config.mem_gb,
                config.file().display()
            );
        }
    }
    eprintln!("{summary}");
    // Gives the slot back (the reporter stops with it).
    if let Ok(mut held) = held.lock() {
        held.take();
    }
    gate::record(
        &config,
        &gate::Done {
            ended: gate::now(),
            class: class.name().into(),
            place: gate::here(),
            command: line,
            ran,
            waited,
            peak,
            code: match &status {
                Ok(s) => s.code().unwrap_or(1),
                Err(_) => 127,
            },
        },
    );
    // Exits while the job is still open: closing it ends every process in it, fgate included, which would
    // replace the command's exit code.
    exit(status)
}

#[cfg(windows)]
/// Ends fgate with the command's own exit code (Windows codes can be large or negative, such as a crash's).
fn exit(status: std::io::Result<std::process::ExitStatus>) -> ! {
    match status {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("fgate: could not start the command: {e}");
            std::process::exit(127)
        }
    }
}

#[cfg(windows)]
/// The command as the status shows it: the program without its folder, at most 140 characters.
fn short(command: &[String]) -> String {
    let program = command[0].replace('\\', "/");
    let program = program.rsplit('/').next().unwrap_or(&program);
    let rest = command[1..].iter().map(String::as_str);
    let mut line = std::iter::once(program)
        .chain(rest)
        .collect::<Vec<_>>()
        .join(" ");
    if line.chars().count() > 140 {
        line = line.chars().take(139).collect::<String>() + "…";
    }
    line
}

#[cfg(not(windows))]
compile_error!(
    "fgate runs on Windows only for now: it uses Windows job objects and file locks. \
     Linux and macOS support is planned: https://github.com/alexander-frase/fgate"
);

#[cfg(not(windows))]
fn main() {}
