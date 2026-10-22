//! A gated command's output, kept for `fgate peek` and `fgate watch`.
//!
//! When fgate's own output goes to a pipe or a file (an agent's shell), nobody but the agent sees what a long job
//! is doing. So fgate passes the command's output on unchanged and keeps its last lines in
//! `output/<class>-<slot>.txt` in its folder, written again twice a second. When the command ends, its last lines
//! move to `output/runs/`, which keeps the newest runs, so `fgate peek` can also show how a run ended. A stream that
//! goes to a terminal is left alone: whoever sits there sees it, with its colours and progress bars. `capture = 0`
//! turns it off.
//!
//! A kept file starts with `fgate-output <pid> <lines so far> <complete lines here>`, then those complete lines,
//! then any line still in progress. A run's file has its history line (`gate::Done`) before that.

use std::collections::VecDeque;
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::gate::{self, CLASSES, Class, Done, Entry};
use crate::sys::{self, QUIT};

/// Lines kept while a command runs.
const KEEP_LINES: usize = 200;
/// Lines kept of a finished run.
const RUN_LINES: usize = 40;
/// Finished runs kept.
const RUNS_KEPT: usize = 50;
/// Characters kept of one line.
const LINE_MAX: usize = 500;
/// How often the kept lines are written while the command runs.
const WRITE_EVERY: Duration = Duration::from_millis(500);
/// How long the rest of the output may take once the command has ended (something it left running can hold the
/// output open).
const DRAIN: Duration = Duration::from_secs(2);
/// How often `fgate peek` looks again.
const FOLLOW_EVERY: Duration = Duration::from_millis(250);
const HEADER: &str = "fgate-output";

/// The last lines of a command's output, from both of its streams in the order they came.
#[derive(Default)]
pub struct Tail {
    lines: VecDeque<String>,
    /// Complete lines so far, also those no longer kept.
    total: u64,
    /// Each stream's line in progress, as it came.
    partial: [Vec<u8>; 2],
    changed: bool,
}

impl Tail {
    /// Takes what stream 0 (stdout) or 1 (stderr) wrote.
    pub fn feed(&mut self, stream: usize, bytes: &[u8]) {
        self.changed = true;
        let mut rest = bytes;
        while let Some(at) = rest.iter().position(|&b| b == b'\n') {
            let mut line = std::mem::take(&mut self.partial[stream]);
            line.extend_from_slice(&rest[..at]);
            self.lines.push_back(clean(&String::from_utf8_lossy(&line)));
            self.total += 1;
            while self.lines.len() > KEEP_LINES {
                self.lines.pop_front();
            }
            rest = &rest[at + 1..];
        }
        let partial = &mut self.partial[stream];
        partial.extend_from_slice(rest);
        // A line that never ends (a progress bar drawn again and again after `\r`) keeps only its newest part.
        if partial.len() > 16 * 1024 {
            partial.drain(..partial.len() - 4 * 1024);
        }
    }

    /// The kept file's text, with at most `last` complete lines.
    fn text(&self, last: usize) -> String {
        let keep = self.lines.len().min(last);
        let mut text = format!("{HEADER} {} {} {keep}\n", std::process::id(), self.total);
        for line in self.lines.iter().skip(self.lines.len() - keep) {
            text += line;
            text.push('\n');
        }
        for partial in &self.partial {
            let line = clean(&String::from_utf8_lossy(partial));
            if !line.is_empty() {
                text += &line;
                text.push('\n');
            }
        }
        text
    }
}

/// One line made fit to show: what a terminal would show after the last carriage return, without colour codes or
/// other control characters, and at most `LINE_MAX` characters.
fn clean(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let line = line.rsplit('\r').next().unwrap_or(line);
    let mut out = String::new();
    let mut n = 0;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters up to a final byte, such as `\x1b[1;31m` or `\x1b[K`.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST, such as a window title or a hyperlink.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => {
                out.push_str("    ");
                n += 4;
            }
            c if c.is_control() => {}
            c => {
                out.push(c);
                n += 1;
            }
        }
        if n >= LINE_MAX {
            break;
        }
    }
    out
}

/// A kept file, read back.
pub struct Kept {
    /// The fgate that wrote it.
    pub pid: u32,
    /// Complete lines the command wrote in all.
    pub total: u64,
    /// The last of them.
    pub lines: Vec<String>,
    /// Lines still in progress.
    pub current: Vec<String>,
}

impl Kept {
    fn parse(text: &str) -> Option<Kept> {
        let mut lines = text.lines();
        let mut header = lines.next()?.split(' ');
        if header.next()? != HEADER {
            return None;
        }
        let pid = header.next()?.parse().ok()?;
        let total = header.next()?.parse().ok()?;
        let complete: usize = header.next()?.parse().ok()?;
        let all: Vec<String> = lines.map(str::to_string).collect();
        let complete = complete.min(all.len());
        Some(Kept {
            pid,
            total,
            current: all[complete..].to_vec(),
            lines: all[..complete].to_vec(),
        })
    }

    /// The newest line with something on it, for `fgate watch`.
    pub fn latest(&self) -> Option<&str> {
        self.current
            .iter()
            .chain(self.lines.iter().rev())
            .map(|l| l.trim_end())
            .find(|l| !l.trim().is_empty())
    }

    /// The number of the first complete line here, counted from the command's first.
    fn first(&self) -> u64 {
        self.total.saturating_sub(self.lines.len() as u64)
    }
}

fn output_dir(config: &Config) -> PathBuf {
    config.dir.join("output")
}

fn slot_file(config: &Config, class: Class, slot: usize) -> PathBuf {
    output_dir(config).join(format!("{}-{slot}.txt", class.name()))
}

fn runs_dir(config: &Config) -> PathBuf {
    output_dir(config).join("runs")
}

/// Writes a file whole, so a reader never sees half of it: a temporary file, then renamed over the old one.
fn replace(path: &Path, text: &str) {
    let fresh = path.with_extension("tmp");
    if fs::write(&fresh, text).is_ok() && fs::rename(&fresh, path).is_err() {
        let _ = fs::remove_file(&fresh);
    }
}

/// What the holder of a slot keeps of its output, if it keeps any.
pub fn kept(config: &Config, class: Class, slot: usize, holder: &Entry) -> Option<Kept> {
    let text = fs::read_to_string(slot_file(config, class, slot)).ok()?;
    Kept::parse(&text).filter(|k| k.pid == holder.pid)
}

/// A gated command's output being kept.
pub struct Capture {
    path: PathBuf,
    runs: PathBuf,
    tail: Arc<Mutex<Tail>>,
    pumps: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl Capture {
    /// Readies `child` so its output can be kept: each of fgate's own streams that doesn't go to a terminal is
    /// passed on through fgate. None when the setting is off or both go to a terminal.
    pub fn prepare(
        config: &Config,
        class: Class,
        slot: usize,
        child: &mut Command,
    ) -> Option<Capture> {
        let out = !std::io::stdout().is_terminal();
        let err = !std::io::stderr().is_terminal();
        if config.capture == 0 || !(out || err) {
            return None;
        }
        if out {
            child.stdout(Stdio::piped());
        }
        if err {
            child.stderr(Stdio::piped());
        }
        // Python holds back its output in blocks when it isn't writing to a terminal; the lines should show as
        // they come.
        if std::env::var_os("PYTHONUNBUFFERED").is_none() {
            child.env("PYTHONUNBUFFERED", "1");
        }
        let path = slot_file(config, class, slot);
        let _ = fs::create_dir_all(runs_dir(config));
        Some(Capture {
            path,
            runs: runs_dir(config),
            tail: Arc::default(),
            pumps: Vec::new(),
            stop: Arc::default(),
            writer: None,
        })
    }

    /// The command has started: its output is passed on and its last lines kept.
    pub fn start(&mut self, child: &mut Child) {
        if let Some(from) = child.stdout.take() {
            self.pumps
                .push(pump(from, std::io::stdout, 0, self.tail.clone()));
        }
        if let Some(from) = child.stderr.take() {
            self.pumps
                .push(pump(from, std::io::stderr, 1, self.tail.clone()));
        }
        let (tail, path, stop) = (self.tail.clone(), self.path.clone(), self.stop.clone());
        // Written at once, so `peek` and `watch` know the output is kept, then whenever it changed.
        replace(&path, &Tail::default().text(0));
        self.writer = Some(std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
                if last.elapsed() < WRITE_EVERY {
                    continue;
                }
                last = Instant::now();
                let text = match tail.lock() {
                    Ok(mut t) if t.changed => {
                        t.changed = false;
                        t.text(KEEP_LINES)
                    }
                    _ => continue,
                };
                replace(&path, &text);
            }
        }));
    }

    /// The command has ended: waits a moment for the rest of its output, so it comes before fgate's own last words.
    pub fn drain(&mut self) {
        let until = Instant::now() + DRAIN;
        while self.pumps.iter().any(|p| !p.is_finished()) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Keeps the run's last lines with how it ended, and clears the slot's file. Before the slot is given back, so
    /// `fgate peek` finds the run as soon as the slot is free.
    pub fn finish(mut self, done: &Done) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        let text = match self.tail.lock() {
            Ok(t) => t.text(RUN_LINES),
            Err(_) => return,
        };
        let name = format!("{:012}-{}.txt", done.ended, std::process::id());
        replace(&self.runs.join(name), &format!("{}\n{text}", done.line()));
        let _ = fs::remove_file(&self.path);
        prune(&self.runs);
    }
}

/// Passes what `from` writes on to fgate's own stream and into the tail. If nobody reads fgate's stream any more,
/// it stops, and the command finds its output closed, as it would have without fgate.
fn pump<R, W>(mut from: R, to: fn() -> W, stream: usize, tail: Arc<Mutex<Tail>>) -> JoinHandle<()>
where
    R: Read + Send + 'static,
    W: Write + 'static,
{
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            // A fresh handle each time: holding fgate's stderr would keep fgate's own messages out.
            let mut to = to();
            if to.write_all(&buf[..n]).and_then(|()| to.flush()).is_err() {
                break;
            }
            if let Ok(mut t) = tail.lock() {
                t.feed(stream, &buf[..n]);
            }
        }
    })
}

/// Keeps the newest `RUNS_KEPT` runs.
fn prune(runs: &Path) {
    let Ok(entries) = fs::read_dir(runs) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    names.sort();
    for old in names.iter().rev().skip(RUNS_KEPT) {
        let _ = fs::remove_file(old);
    }
}

/// A finished run, as `output/runs/` keeps it.
pub struct Run {
    pub done: Done,
    pub kept: Kept,
}

/// The kept runs, newest first.
fn runs(config: &Config) -> Vec<Run> {
    runs_in(&runs_dir(config))
}

fn runs_in(dir: &Path) -> Vec<Run> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    names.sort();
    names
        .iter()
        .rev()
        .filter_map(|p| {
            let text = fs::read_to_string(p).ok()?;
            let (first, rest) = text.split_once('\n')?;
            Some(Run {
                done: Done::parse(first)?,
                kept: Kept::parse(rest)?,
            })
        })
        .collect()
}

/// A job holding a slot.
struct Running {
    class: Class,
    slot: usize,
    entry: Entry,
}

impl Running {
    /// How to ask for this job: its checkout's name, unless another running job has it too.
    fn ask(&self, all: &[Running]) -> String {
        let shared = all
            .iter()
            .filter(|r| r.entry.place.eq_ignore_ascii_case(&self.entry.place))
            .count()
            > 1;
        if shared {
            format!("fgate peek {} {}", self.class.name(), self.slot)
        } else {
            format!("fgate peek {}", self.entry.place)
        }
    }
}

fn running(config: &Config) -> Vec<Running> {
    CLASSES
        .iter()
        .flat_map(|&class| {
            gate::slots(config, class)
                .into_iter()
                .enumerate()
                .filter_map(move |(slot, e)| e.map(|entry| Running { class, slot, entry }))
        })
        .collect()
}

/// `fgate peek [name | class slot]`: follows a running job's output, or shows how a finished run ended.
pub fn peek(args: &[String]) -> ExitCode {
    let config = Config::load();
    let jobs = running(&config);
    let class_named = |name: &str| CLASSES.iter().copied().find(|c| c.name() == name);
    match args {
        [] => match jobs.len() {
            0 => match runs(&config).into_iter().next() {
                Some(run) => show_run(&run),
                None => fail("nothing is running, and no finished run has kept output yet"),
            },
            1 => follow(&config, &jobs[0]),
            _ => list("jobs are running", &jobs.iter().collect::<Vec<_>>(), &jobs),
        },
        [class, slot] if class_named(class.as_str()).is_some() => {
            let class = class_named(class.as_str());
            let job = slot.parse::<usize>().ok().and_then(|slot| {
                jobs.iter()
                    .find(|j| Some(j.class) == class && j.slot == slot)
            });
            match job {
                Some(job) => follow(&config, job),
                None => fail(&format!(
                    "{} slot {slot} is free",
                    class.map_or("?", Class::name)
                )),
            }
        }
        [name] => {
            let mine: Vec<&Running> = jobs
                .iter()
                .filter(|j| j.entry.place.eq_ignore_ascii_case(name))
                .collect();
            match mine.as_slice() {
                [job] => follow(&config, job),
                [] => match runs(&config)
                    .into_iter()
                    .find(|r| r.done.place.eq_ignore_ascii_case(name))
                {
                    Some(run) => show_run(&run),
                    None if jobs.is_empty() => {
                        fail(&format!("nothing from {name} is running or kept"))
                    }
                    None => list(
                        &format!("other jobs are running (nothing from {name})"),
                        &jobs.iter().collect::<Vec<_>>(),
                        &jobs,
                    ),
                },
                several => list(&format!("{name} runs several jobs"), several, &jobs),
            }
        }
        _ => fail(
            "use: fgate peek [name | class slot], for example `fgate peek geothermal` or `fgate peek gpu 1`",
        ),
    }
}

fn fail(why: &str) -> ExitCode {
    eprintln!("fgate peek: {why}");
    ExitCode::FAILURE
}

/// Several jobs could be meant: says how to ask for each.
fn list(what: &str, these: &[&Running], all: &[Running]) -> ExitCode {
    eprintln!("fgate peek: {} {what}; peek at one:", these.len());
    let asks: Vec<String> = these.iter().map(|j| j.ask(all)).collect();
    let wide = asks.iter().map(|a| a.chars().count()).max().unwrap_or(0);
    for (job, ask) in these.iter().zip(&asks) {
        eprintln!(
            "  {ask:<wide$}   {:<5} slot {}  {}",
            job.class.name(),
            job.slot,
            job.entry.command
        );
    }
    ExitCode::FAILURE
}

/// Prints a running job's output as it comes, until it ends (or Ctrl+C, or nobody reads any more).
fn follow(config: &Config, job: &Running) -> ExitCode {
    sys::catch_quit();
    match follow_to(&mut std::io::stdout(), config, job) {
        Ok(code) => code,
        // Whoever read along has gone (`fgate peek | head`).
        Err(_) => ExitCode::SUCCESS,
    }
}

fn follow_to(out: &mut impl Write, config: &Config, job: &Running) -> std::io::Result<ExitCode> {
    let e = &job.entry;
    writeln!(
        out,
        "── {} · {} slot {} · {} · running {}",
        e.place,
        job.class.name(),
        job.slot,
        e.command,
        gate::duration(e.age())
    )?;
    let mut seen: Option<u64> = None;
    loop {
        if QUIT.load(Ordering::SeqCst) {
            return Ok(ExitCode::SUCCESS);
        }
        let holder = gate::slots(config, job.class)
            .into_iter()
            .nth(job.slot)
            .flatten();
        if holder.as_ref().is_none_or(|h| h.pid != e.pid) {
            break;
        }
        match kept(config, job.class, job.slot, e) {
            Some(k) => print_new(out, &k, &mut seen)?,
            // A job that keeps output writes its file as it starts; one that hasn't after a moment keeps none.
            None if seen.is_none() && e.age() >= Duration::from_secs(3) => {
                return Ok(fail(
                    "this job keeps no output: its output goes to a terminal, capture = 0 in the settings, or it \
                     was started by an older fgate",
                ));
            }
            None => {}
        }
        std::thread::sleep(FOLLOW_EVERY);
    }
    // Ended: the rest, from the run it left.
    let run = runs(config)
        .into_iter()
        .find(|r| r.kept.pid == e.pid && r.done.ended >= e.since);
    match run {
        Some(run) => {
            print_new(out, &run.kept, &mut seen)?;
            for line in &run.kept.current {
                writeln!(out, "{line}")?;
            }
            writeln!(out, "── {}", ending(&run.done))?;
        }
        None => writeln!(out, "── ended")?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Prints the complete lines of `k` not printed yet.
fn print_new(out: &mut impl Write, k: &Kept, seen: &mut Option<u64>) -> std::io::Result<()> {
    let first = k.first();
    let from = seen.unwrap_or(first);
    if from < first {
        writeln!(out, "   … {} lines went by too fast to show", first - from)?;
    }
    for (i, line) in k.lines.iter().enumerate() {
        if first + i as u64 >= from {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()?;
    *seen = Some(from.max(k.total));
    Ok(())
}

/// `ended after 1m08s · exit 101`.
fn ending(done: &Done) -> String {
    format!(
        "ended after {} · exit {}",
        gate::duration(done.ran),
        done.code
    )
}

/// Shows how a finished run ended, and its last lines.
fn show_run(run: &Run) -> ExitCode {
    let _ = show_run_to(&mut std::io::stdout(), run);
    ExitCode::SUCCESS
}

fn show_run_to(out: &mut impl Write, run: &Run) -> std::io::Result<()> {
    let d = &run.done;
    let ago = Duration::from_secs(gate::now().saturating_sub(d.ended));
    writeln!(
        out,
        "── {} · {} · {} · {} ago, {}",
        d.place,
        d.class,
        d.command,
        gate::duration(ago),
        ending(d)
    )?;
    if run.kept.lines.is_empty() && run.kept.current.is_empty() {
        writeln!(out, "(no output)")?;
    }
    for line in run.kept.lines.iter().chain(&run.kept.current) {
        writeln!(out, "{line}")?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_cleaned_for_showing() {
        assert_eq!(
            clean("\x1b[1m\x1b[32m   Compiling\x1b[0m fgate v0.5.0"),
            "   Compiling fgate v0.5.0"
        );
        assert_eq!(clean("progress 10%\rprogress 20%\r\n"), "progress 20%");
        assert_eq!(clean("windows line\r"), "windows line");
        assert_eq!(
            clean("\x1b]8;;http://x\x1b\\link\x1b]8;;\x07 done"),
            "link done"
        );
        assert_eq!(clean("a\tb\x08c"), "a    bc");
        assert_eq!(clean(&"x".repeat(900)).len(), LINE_MAX);
    }

    #[test]
    fn the_tail_keeps_the_last_lines_of_both_streams() {
        let mut t = Tail::default();
        t.feed(0, b"one\ntw");
        t.feed(1, b"err\n");
        t.feed(0, b"o\nthree");
        assert_eq!(Vec::from(t.lines.clone()), ["one", "err", "two"]);
        assert_eq!(t.total, 3);
        let k = Kept::parse(&t.text(2)).unwrap();
        assert_eq!((k.pid, k.total), (std::process::id(), 3));
        assert_eq!(k.lines, ["err", "two"]);
        assert_eq!(k.current, ["three"]);
        assert_eq!(k.first(), 1);
        assert_eq!(k.latest(), Some("three"));

        for i in 0..KEEP_LINES + 10 {
            t.feed(1, format!("line {i}\n").as_bytes());
        }
        assert_eq!(t.lines.len(), KEEP_LINES);
        assert_eq!(t.total, 3 + KEEP_LINES as u64 + 10);
    }

    #[test]
    fn a_line_that_never_ends_keeps_its_newest_part() {
        let mut t = Tail::default();
        for i in 0..5000 {
            t.feed(0, format!("\rprogress {i}").as_bytes());
        }
        assert!(t.partial[0].len() <= 16 * 1024);
        let k = Kept::parse(&t.text(10)).unwrap();
        assert_eq!(k.latest(), Some("progress 4999"));
    }

    #[test]
    fn peek_prints_only_new_lines() {
        let mut t = Tail::default();
        t.feed(0, b"a\nb\n");
        let mut seen = None;
        let mut out = Vec::new();
        let mut show = |t: &Tail, out: &mut Vec<u8>, keep| {
            print_new(out, &Kept::parse(&t.text(keep)).unwrap(), &mut seen).unwrap();
        };
        show(&t, &mut out, KEEP_LINES);
        t.feed(0, b"c\n");
        show(&t, &mut out, KEEP_LINES);
        assert_eq!(String::from_utf8(out).unwrap(), "a\nb\nc\n");
        // More came than one file keeps.
        for i in 0..5 {
            t.feed(0, format!("{i}\n").as_bytes());
        }
        let mut out = Vec::new();
        show(&t, &mut out, 2);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "   … 3 lines went by too fast to show\n3\n4\n"
        );
    }

    #[test]
    fn runs_are_kept_newest_first_and_pruned() {
        let dir = std::env::temp_dir().join(format!("fgate-runs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for ended in 0..RUNS_KEPT as u64 + 5 {
            let mut t = Tail::default();
            t.feed(1, format!("run {ended}\n").as_bytes());
            let done = Done {
                ended,
                class: "build".into(),
                place: if ended % 2 == 0 { "even" } else { "odd" }.into(),
                command: "cargo test".into(),
                ran: Duration::from_secs(3),
                waited: Duration::ZERO,
                peak: 0,
                code: 101,
            };
            let name = format!("{ended:012}-1.txt");
            replace(
                &dir.join(name),
                &format!("{}\n{}", done.line(), t.text(RUN_LINES)),
            );
            prune(&dir);
        }
        let all = runs_in(&dir);
        assert_eq!(all.len(), RUNS_KEPT);
        assert_eq!(all[0].done.ended, RUNS_KEPT as u64 + 4);
        assert_eq!(all[0].kept.lines, [format!("run {}", RUNS_KEPT + 4)]);
        let even = all.iter().find(|r| r.done.place == "even").unwrap();
        assert_eq!(ending(&even.done), "ended after 3s · exit 101");
        let _ = fs::remove_dir_all(&dir);
    }
}
