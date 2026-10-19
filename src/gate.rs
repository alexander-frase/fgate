//! Slots and the line, kept as files under the settings folder.
//!
//! A slot is `slots/<class>-<n>.slot`; whoever holds a lock on it owns it and writes what it runs into it. A
//! place in line is `queue/<class>/<time>-<pid>.ticket`, locked by its waiter for as long as it waits. Both
//! locks sit far past the end of the file, so others can still read what is written there, and Windows
//! releases them when the holder ends. A ticket that can be locked by someone else is therefore left over
//! from a process that has ended, and is cleared away.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::sys::{try_lock, unlock};

/// How often a waiter looks again.
const POLL: Duration = Duration::from_millis(500);
/// How often a waiter says where it stands when nothing changed.
const REPORT_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Build,
    Gpu,
    /// Small commands that skip the line (`fgate --now`), held to a small memory ceiling.
    Now,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Build => "build",
            Class::Gpu => "gpu",
            Class::Now => "now",
        }
    }

    fn slots(self, config: &Config) -> usize {
        match self {
            Class::Build => config.build_slots,
            Class::Gpu => config.gpu_slots,
            Class::Now => config.now_slots,
        }
    }
}

pub const CLASSES: [Class; 3] = [Class::Build, Class::Gpu, Class::Now];

fn open(path: &Path) -> Option<File> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()
}

fn read(file: &mut File) -> String {
    let mut text = String::new();
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.read_to_string(&mut text);
    text
}

fn write(file: &mut File, text: &str) {
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(text.as_bytes());
    let _ = file.flush();
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a slot holder or a waiter writes: since when, where from, what.
fn describe(line: &str) -> String {
    format!("{}\n{}\n{}\n{}", now(), std::process::id(), here(), line)
}

/// This checkout's short name.
pub fn here() -> String {
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    place(&cwd)
}

/// A checkout's short name: the worktree's name, or the folder's.
fn place(cwd: &str) -> String {
    let path = cwd.replace('\\', "/");
    if let Some((_, rest)) = path.split_once(".claude/worktrees/") {
        return rest.split('/').next().unwrap_or(rest).to_string();
    }
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// `12s`, `3m05s`, `1h02m`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// `850 MB`, `7.9 GB`.
pub fn size(bytes: u64) -> String {
    let gb = bytes as f64 / (1u64 << 30) as f64;
    if gb < 1.0 {
        format!("{:.0} MB", gb * 1024.0)
    } else {
        format!("{gb:.1} GB")
    }
}

/// `terrain-features: cargo test --workspace` from what a holder or waiter wrote.
fn who(text: &str) -> String {
    let mut lines = text.lines().skip(2);
    let place = lines.next().unwrap_or("?");
    let command = lines.next().unwrap_or("?");
    format!("{place}: {command}")
}

/// `terrain-features: cargo test --workspace (1m40s)`.
fn summary(text: &str) -> String {
    let since = text.lines().next().and_then(|s| s.parse::<u64>().ok());
    let age = since.map_or("?".into(), |s| {
        duration(Duration::from_secs(now().saturating_sub(s)))
    });
    format!("{} ({age})", who(text))
}

fn slot_path(config: &Config, class: Class, index: usize) -> PathBuf {
    config
        .dir
        .join("slots")
        .join(format!("{}-{index}.slot", class.name()))
}

/// Each slot of a class: what its holder runs, or `None` when free.
fn holders(config: &Config, class: Class) -> Vec<Option<String>> {
    (0..class.slots(config))
        .map(|i| {
            let mut file = open(&slot_path(config, class, i))?;
            if try_lock(&file) {
                unlock(&file);
                None
            } else {
                Some(read(&mut file))
            }
        })
        .collect()
}

/// A slot this process holds; dropping it (or ending) frees it.
pub struct Held {
    _file: File,
    pub index: usize,
    path: PathBuf,
    /// Where the usage line starts, just after what the holder wrote.
    usage_at: u64,
}

impl Held {
    /// Writes what the job uses after what the slot says it runs, for `fgate watch`. The line keeps one width
    /// and is written over in place, so a reader never sees the slot's text cut short.
    pub fn report(&self, usage: &crate::sys::Usage) {
        let line = format!(
            "\nusage {:>20} {:>20} {:>20} {:>10}",
            usage.memory, usage.peak, usage.cpu, usage.processes
        );
        // A second handle without the lock: the lock sits far past the text.
        if let Ok(mut file) = OpenOptions::new().write(true).open(&self.path) {
            let _ = file.seek(SeekFrom::Start(self.usage_at));
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// A slot holder or waiter, as `fgate watch` shows it.
pub struct Entry {
    /// Since when it holds the slot or waits (Unix seconds).
    pub since: u64,
    pub place: String,
    pub command: String,
    /// What a holder's job uses, once it has said.
    pub usage: Option<crate::sys::Usage>,
}

impl Entry {
    fn parse(text: &str) -> Entry {
        let mut lines = text.lines();
        let since = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let _pid = lines.next();
        let place = lines.next().unwrap_or("?").to_string();
        let command = lines.next().unwrap_or("?").to_string();
        let usage = lines.next().and_then(|l| {
            let mut n = l
                .strip_prefix("usage")?
                .split_whitespace()
                .map(str::parse::<u64>);
            Some(crate::sys::Usage {
                memory: n.next()?.ok()?,
                peak: n.next()?.ok()?,
                cpu: n.next()?.ok()?,
                processes: n.next()?.ok()? as u32,
            })
        });
        Entry {
            since,
            place,
            command,
            usage,
        }
    }

    /// How long it has held its slot or waited.
    pub fn age(&self) -> Duration {
        Duration::from_secs(now().saturating_sub(self.since))
    }
}

/// Each slot of a class: who holds it, or `None` when free.
pub fn slots(config: &Config, class: Class) -> Vec<Option<Entry>> {
    holders(config, class)
        .into_iter()
        .map(|h| h.map(|t| Entry::parse(&t)))
        .collect()
}

/// Everyone waiting in a class, first in line first.
pub fn line(config: &Config, class: Class) -> Vec<Entry> {
    line_up(config, class)
        .iter()
        .map(|(_, t)| Entry::parse(t))
        .collect()
}

/// A finished command, as the history keeps it.
pub struct Done {
    /// When it ended (Unix seconds).
    pub ended: u64,
    pub class: String,
    pub place: String,
    pub command: String,
    pub ran: Duration,
    pub waited: Duration,
    pub peak: u64,
    pub code: i32,
}

fn history_path(config: &Config) -> PathBuf {
    config.dir.join("history.log")
}

/// Adds a finished command to the history (tab-separated, newest last), kept to its last few hundred lines.
pub fn record(config: &Config, done: &Done) {
    let path = history_path(config);
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let line = format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        done.ended,
        done.class,
        clean(&done.place),
        clean(&done.command),
        done.ran.as_secs(),
        done.waited.as_secs(),
        done.peak,
        done.code
    );
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
    if fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let keep = lines[lines.len().saturating_sub(300)..].join("\n") + "\n";
        let _ = fs::write(&path, keep);
    }
}

/// The last `n` finished commands, newest first.
pub fn history(config: &Config, n: usize) -> Vec<Done> {
    let text = fs::read_to_string(history_path(config)).unwrap_or_default();
    text.lines()
        .rev()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 8 {
                return None;
            }
            Some(Done {
                ended: f[0].parse().ok()?,
                class: f[1].to_string(),
                place: f[2].to_string(),
                command: f[3].to_string(),
                ran: Duration::from_secs(f[4].parse().ok()?),
                waited: Duration::from_secs(f[5].parse().ok()?),
                peak: f[6].parse().ok()?,
                code: f[7].parse().ok()?,
            })
        })
        .take(n)
        .collect()
}

/// A place in line.
struct Ticket {
    file: Option<File>,
    path: PathBuf,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}

fn queue_dir(config: &Config, class: Class) -> PathBuf {
    config.dir.join("queue").join(class.name())
}

fn take_ticket(config: &Config, class: Class, line: &str) -> Ticket {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = queue_dir(config, class).join(format!("{nanos:024}-{}.ticket", std::process::id()));
    let file = open(&path).map(|mut f| {
        try_lock(&f);
        write(&mut f, &describe(line));
        f
    });
    Ticket { file, path }
}

/// Everyone waiting in a class, first in line first; clears tickets whose waiter has ended.
fn line_up(config: &Config, class: Class) -> Vec<(PathBuf, String)> {
    let Ok(entries) = fs::read_dir(queue_dir(config, class)) else {
        return Vec::new();
    };
    let mut waiting = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "ticket") {
            continue;
        }
        // A ticket only just made may not be locked yet.
        let young = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age < Duration::from_secs(5));
        let Ok(mut file) = OpenOptions::new().read(true).write(true).open(&path) else {
            continue;
        };
        if !young && try_lock(&file) {
            unlock(&file);
            drop(file);
            let _ = fs::remove_file(&path);
            continue;
        }
        let text = read(&mut file);
        waiting.push((path, text));
    }
    waiting.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));
    waiting
}

/// When a file last changed, if it can be told.
fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Free memory, in bytes.
fn free_memory() -> u64 {
    crate::sys::memory().1
}

/// Waits for a slot of `class` in fair order, and takes it.
/// The settings are read again while waiting, when the file changes and once a minute (for the night limits), so
/// slots added while commands wait are used at once.
pub fn acquire(config: &Config, class: Class, line: &str) -> Held {
    let ticket = take_ticket(config, class, line);
    let mut last_said: Option<(String, Instant)> = None;
    let mut fresh: Option<Config> = None;
    let mut read = (modified(&config.file()), Instant::now());
    loop {
        let changed = modified(&config.file());
        if changed != read.0 || read.1.elapsed() >= REPORT_EVERY {
            fresh = Some(Config::load());
            read = (changed, Instant::now());
        }
        let config = fresh.as_ref().unwrap_or(config);
        let waiting = line_up(config, class);
        let position = waiting
            .iter()
            .position(|(p, _)| *p == ticket.path)
            .unwrap_or(0);
        let mine = holders(config, class);
        let free: Vec<usize> = mine
            .iter()
            .enumerate()
            .filter(|(_, h)| h.is_none())
            .map(|(i, _)| i)
            .collect();

        // What holds things up, and the same without the ticking clocks: the waiter speaks again only when the
        // latter changes, or after a minute.
        let mut why = String::new();
        let mut situation = String::new();
        if position < free.len() {
            let anyone_running = CLASSES
                .iter()
                .any(|&c| c != class && holders(config, c).iter().any(Option::is_some))
                || mine.iter().any(Option::is_some);
            let free_bytes = free_memory();
            // By day only a second job waits for memory; at night any job does.
            if (anyone_running || config.night)
                && free_bytes < config.min_free_gb as u64 * (1u64 << 30)
            {
                why = format!(
                    "waiting for memory: {} free, a {} starts at {} GB{}",
                    size(free_bytes),
                    if config.night { "job" } else { "second job" },
                    config.min_free_gb,
                    if config.night { " (night limits)" } else { "" }
                );
                situation = "memory".into();
            } else {
                for index in free {
                    let Some(mut file) = open(&slot_path(config, class, index)) else {
                        continue;
                    };
                    if try_lock(&file) {
                        let text = describe(line);
                        write(&mut file, &text);
                        drop(ticket);
                        return Held {
                            _file: file,
                            index,
                            path: slot_path(config, class, index),
                            usage_at: text.len() as u64,
                        };
                    }
                }
                // Someone took it between looking and taking: look again shortly.
                std::thread::sleep(POLL);
                continue;
            }
        }
        if why.is_empty() {
            let running: Vec<String> = mine.iter().flatten().map(|t| summary(t)).collect();
            why = format!(
                "waiting for a {} slot: #{} in line; running: {}",
                class.name(),
                position + 1,
                running.join(" | ")
            );
            let running: Vec<String> = mine.iter().flatten().map(|t| who(t)).collect();
            situation = format!("{position} {}", running.join("|"));
        }
        let stale = last_said
            .as_ref()
            .is_none_or(|(said, at)| *said != situation || at.elapsed() >= REPORT_EVERY);
        if stale {
            eprintln!("fgate: {why}");
            last_said = Some((situation, Instant::now()));
        }
        std::thread::sleep(POLL);
    }
}

pub fn print_status(config: &Config) {
    let (total, free) = crate::sys::memory();
    println!(
        "fgate: {} of {} free; {}; settings {}",
        size(free),
        size(total),
        if config.night {
            format!(
                "night limits until {}:00 ({} build slot(s), {} GB per job)",
                config.night_until, config.build_slots, config.mem_gb
            )
        } else {
            "day limits".into()
        },
        config.file().display()
    );
    if let Some(why) = crate::sys::ceilings() {
        println!("  no memory or CPU ceilings: {why}");
    }
    for class in CLASSES {
        for (i, holder) in holders(config, class).iter().enumerate() {
            // Free `now` slots are many and say nothing.
            if class == Class::Now && holder.is_none() {
                continue;
            }
            let text = holder.as_deref().map_or("free".into(), summary);
            println!("  {} slot {i}: {text}", class.name());
        }
        for (n, (_, text)) in line_up(config, class).iter().enumerate() {
            println!("  {} line #{}: {}", class.name(), n + 1, summary(text));
        }
    }
}
