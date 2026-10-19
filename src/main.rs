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
//! command and everything it starts run at low priority within a CPU and memory ceiling (a Windows job, a
//! systemd scope on Linux; macOS has no such ceilings) and end together when fgate ends. Slots and places in
//! line are OS file locks, which the system drops when a process ends however it ends, so nothing is ever left
//! locked by a crashed or killed agent. Settings: `config.txt` in fgate's folder, which `fgate status` names
//! (written with defaults on first use).

mod config;
mod gate;
mod hook;
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
  fgate hook [options]         Claude Code PreToolUse hook (reads the tool call on stdin)
      --gpu a,b                  programs that must run under `fgate --gpu`
      --protect-branch <name>    refuse `git push` to this branch
      --protect-label <name>     refuse `gh` commands that name this label
      --no-admin-merge           refuse `gh pr merge --admin`
      --note <text>              added to every refusal
Settings: config.txt in fgate's folder (`fgate status` shows where), or FGATE_* environment variables.";

/// Set for everything fgate runs: a gated command that starts another `fgate …` already holds a slot.
const HELD_VAR: &str = "FGATE_HELD";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
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
        exit(Command::new(program).args(args).status());
    }

    let config = Config::load();
    let line = short(command);
    let waited = Instant::now();
    let held = gate::acquire(&config, class, &line);
    let waited = waited.elapsed();
    // The settings as they are now: they may have changed while the command waited.
    let config = Config::load();

    let mem_gb = config.mem_gb_for(class);
    let mem_bytes = mem_gb as u64 * (1u64 << 30);
    let (mut child, job, note) = sys::Job::prepare(program, args, config.cpu_percent, mem_bytes);
    if let Some(note) = note {
        eprintln!("fgate: {note}");
    }
    let job = job.map(Arc::new);
    child.env(HELD_VAR, class.name());
    if class != gate::Class::Gpu && std::env::var_os("CARGO_BUILD_JOBS").is_none() {
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
    let status = child.spawn().and_then(|mut running| {
        if let Some(job) = &job {
            job.started(&running);
        }
        running.wait()
    });
    let ran = ran.elapsed();
    let peak = job.as_deref().map(sys::Job::peak_memory).unwrap_or(0);
    let mut summary = format!("fgate: done in {}", gate::duration(ran));
    if peak > 0 {
        summary += &format!(", peak memory {}", gate::size(peak));
        if mem_bytes > 0 && peak as f64 >= mem_bytes as f64 * 0.95 {
            summary += &if class == gate::Class::Now {
                format!(
                    " — it reached the {mem_gb} GB ceiling of `fgate --now`, so an \"out of memory\" failure above \
                     came from fgate, not the code: this command is too big to skip the line. Run it again without \
                     `--now`."
                )
            } else {
                format!(
                    " — it reached the {mem_gb} GB ceiling, so an \"out of memory\" failure above came from fgate, \
                     not the code (raise mem_gb in {})",
                    config.file().display()
                )
            };
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
                Ok(s) => code(s),
                Err(_) => 127,
            },
        },
    );
    // Ends what the command left running. On Windows that happens when fgate exits with the job still open
    // (closing it first would end fgate too, and replace the command's exit code).
    if let Some(job) = &job {
        job.finish();
    }
    exit(status)
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

#[cfg(not(any(windows, unix)))]
compile_error!("fgate runs on Windows, Linux and macOS.");
