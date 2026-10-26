//! Slots and the line, kept as files under the settings folder.
//!
//! A slot is `slots/<class>-<n>.slot`; whoever holds a lock on it owns it and writes what it runs into it. A
//! place in line is `queue/<class>/<time>-<pid>.ticket`, locked by its waiter for as long as it waits. Both
//! locks sit far past the end of the file, so others can still read what is written there, and Windows
//! releases them when the holder ends. A ticket that can be locked by someone else is therefore left over
//! from a process that has ended, and is cleared away.
//!
//! A cargo build into a shared target directory also needs that target's lock for its profile folder (`switch`),
//! which only one build holds at a time. Its ticket names that lock, and it takes the lock together with its slot,
//! so it never sits in a slot waiting for another build to finish. While its lock is held, or a waiter ahead of it
//! needs the same lock, it keeps its place, and the waiters behind it that need another lock or none go past it.
//! Once its lock is free it goes first again.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::switch::{self, Take, TargetLock};
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
    /// A run that has the machine to itself (`fgate --solo`), such as a performance measurement: while it waits
    /// nothing new starts, and while it runs nothing else does, until it hands the machine back.
    Solo,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Build => "build",
            Class::Gpu => "gpu",
            Class::Now => "now",
            Class::Solo => "solo",
        }
    }

    fn slots(self, config: &Config) -> usize {
        match self {
            Class::Build => config.build_slots,
            Class::Gpu => config.gpu_slots,
            Class::Now => config.now_slots,
            Class::Solo => 1,
        }
    }
}

pub const CLASSES: [Class; 4] = [Class::Build, Class::Gpu, Class::Now, Class::Solo];

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

/// What a slot holder, a waiter or a target lock's holder writes: since when, where from, what.
pub fn describe(line: &str) -> String {
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
pub fn summary(text: &str) -> String {
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

/// Where a solo run that has handed the machine back says so; it means nothing while the solo slot is free.
fn handed_back_path(config: &Config) -> PathBuf {
    config.dir.join("slots").join("solo-0.handed-back")
}

/// Whether the solo run going now has handed the machine back, so others may run beside it.
pub fn handed_back(config: &Config) -> bool {
    handed_back_path(config).exists()
}

/// A solo run lets others run again (after `solo_minutes`), while it goes on.
pub fn hand_back(config: &Config) {
    let _ = fs::write(handed_back_path(config), "");
}

/// Why a command other than a solo run must wait for one, and the same without the ticking clock, or `None`. A
/// solo run that has handed the machine back holds up nobody, and neither does the next one in line while it runs,
/// so a run that hangs can't hold the machine through the next.
fn solo_blocks(config: &Config) -> Option<(String, String)> {
    match holders(config, Class::Solo).into_iter().next().flatten() {
        Some(_) if handed_back(config) => None,
        Some(text) => Some((
            format!(
                "a solo run has the machine until it ends: {}",
                summary(&text)
            ),
            format!("solo {}", who(&text)),
        )),
        None => line_up(config, Class::Solo).first().map(|(_, text)| {
            (
                format!(
                    "a solo run waits for the machine to be quiet, nothing new starts until it is done: {}",
                    summary(text)
                ),
                format!("solo waiting {}", who(text)),
            )
        }),
    }
}

/// Every job running that is not a solo run, which a solo run waits for.
fn others_running(config: &Config) -> Vec<(Class, String)> {
    CLASSES
        .iter()
        .filter(|&&c| c != Class::Solo)
        .flat_map(|&c| {
            holders(config, c)
                .into_iter()
                .flatten()
                .map(move |t| (c, t))
        })
        .collect()
}

/// A slot this process holds; dropping it (or ending) frees it.
pub struct Held {
    file: Option<File>,
    pub index: usize,
    path: PathBuf,
    /// Where the usage line starts, just after what the holder wrote.
    usage_at: u64,
    /// A solo run's hand-back marker, cleared before the slot is given back.
    handed_back: Option<PathBuf>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(path) = &self.handed_back {
            let _ = fs::remove_file(path);
        }
        drop(self.file.take());
    }
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
    /// The fgate process that holds the slot or waits.
    pub pid: u32,
    pub place: String,
    pub command: String,
    /// What a holder's job uses, once it has said.
    pub usage: Option<crate::sys::Usage>,
}

impl Entry {
    fn parse(text: &str) -> Entry {
        let mut lines = text.lines();
        let since = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let pid = lines.next().and_then(|s| s.parse().ok()).unwrap_or(0);
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
            pid,
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

impl Done {
    /// One tab-separated line, as the history keeps it (without the line end).
    pub fn line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.ended,
            self.class,
            clean(&self.place),
            clean(&self.command),
            self.ran.as_secs(),
            self.waited.as_secs(),
            self.peak,
            self.code
        )
    }

    pub fn parse(line: &str) -> Option<Done> {
        let f: Vec<&str> = line.split('\t').collect();
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
    }
}

fn history_path(config: &Config) -> PathBuf {
    config.dir.join("history.log")
}

/// Adds a finished command to the history (tab-separated, newest last), kept to its last few hundred lines.
pub fn record(config: &Config, done: &Done) {
    let path = history_path(config);
    let line = done.line() + "\n";
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
    text.lines().rev().filter_map(Done::parse).take(n).collect()
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

/// A ticket's line naming the target lock its waiter needs.
const NEEDS: &str = "needs ";

/// A place in line, with the target lock its waiter needs, if any, written after what it runs.
fn take_ticket(config: &Config, class: Class, line: &str, lock: Option<&Path>) -> Ticket {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = queue_dir(config, class).join(format!("{nanos:024}-{}.ticket", std::process::id()));
    let mut text = describe(line);
    if let Some(lock) = lock {
        text += &format!("\n{NEEDS}{}", switch::normal(lock));
    }
    let file = open(&path).map(|mut f| {
        try_lock(&f);
        write(&mut f, &text);
        f
    });
    Ticket { file, path }
}

/// The target lock a waiter needs, from its ticket.
fn needs(ticket: &str) -> Option<&str> {
    ticket.lines().skip(4).find_map(|l| l.strip_prefix(NEEDS))
}

/// Which waiters, first in line first, can't start yet because of the target lock they need: another build holds
/// it, or a waiter ahead of them needs it too (that one goes first). They keep their places; the others count as if
/// they weren't there, and go past them. `busy` is asked once per lock.
fn held_up(needs: &[Option<&str>], busy: impl Fn(&str) -> bool) -> Vec<bool> {
    let mut known: HashMap<&str, bool> = HashMap::new();
    needs
        .iter()
        .map(|lock| {
            let Some(lock) = *lock else {
                return false;
            };
            // Asked for the first waiter that needs it; every later one waits behind that one anyway.
            match known.get(lock) {
                Some(_) => true,
                None => {
                    let held = busy(lock);
                    known.insert(lock, held);
                    held
                }
            }
        })
        .collect()
}

/// Who holds a target lock, for a waiter's message.
fn lock_holder(lock: &Path) -> String {
    switch::holder(lock).map_or("another build".into(), |t| summary(&t))
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

/// Waits for a slot of `class` in fair order, and takes it, together with the target lock `lock` when the command
/// needs one (`switch::lock_path`).
/// The settings are read again while waiting, when the file changes and once a minute (for the night limits), so
/// slots added while commands wait are used at once.
pub fn acquire(
    config: &Config,
    class: Class,
    line: &str,
    lock: Option<&Path>,
) -> (Held, Option<TargetLock>) {
    let ticket = take_ticket(config, class, line, lock);
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
        let me = waiting.iter().position(|(p, _)| *p == ticket.path);
        let place = me.unwrap_or(0);
        let needed: Vec<Option<&str>> = waiting.iter().map(|(_, t)| needs(t)).collect();
        let held = held_up(&needed, |l| switch::busy(Path::new(l)));
        // The waiters ahead that can start: those waiting for a target lock are passed.
        let position = held[..place].iter().filter(|h| !**h).count();
        let blocked = match me {
            Some(i) => held[i],
            None => lock.is_some_and(switch::busy),
        };
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
        // Everything waits for a solo run; a solo run waits for everything else to end.
        let others = if class == Class::Solo {
            others_running(config)
        } else {
            Vec::new()
        };
        if let Some((reason, key)) = (class != Class::Solo)
            .then(|| solo_blocks(config))
            .flatten()
        {
            why = format!("waiting: {reason}");
            situation = key;
        } else if let Some(lock) = lock.filter(|_| blocked) {
            // It keeps its place, and the jobs behind it that build elsewhere go past it.
            let mine = switch::normal(lock);
            let first = waiting[..place]
                .iter()
                .find(|(_, t)| needs(t) == Some(mine.as_str()));
            let (holder, key) = match first {
                Some((_, first)) => (
                    format!(
                        "the build into {} ahead in line to go first: {}",
                        switch::lock_name(lock),
                        summary(first)
                    ),
                    format!("behind {}", who(first)),
                ),
                None => {
                    let text = switch::holder(lock).unwrap_or_default();
                    (
                        format!(
                            "another build into {} to finish: {}",
                            switch::lock_name(lock),
                            lock_holder(lock)
                        ),
                        format!("lock {}", who(&text)),
                    )
                }
            };
            why = format!(
                "waiting for {holder}; #{} in line, the jobs behind that build elsewhere go ahead meanwhile",
                place + 1
            );
            situation = key;
        } else if position < free.len() && !others.is_empty() {
            let running: Vec<String> = others
                .iter()
                .map(|(c, t)| format!("{}: {}", c.name(), summary(t)))
                .collect();
            why = format!(
                "waiting for the machine to be quiet for a solo run, nothing new starts meanwhile; running: {}",
                running.join(" | ")
            );
            let running: Vec<String> = others.iter().map(|(_, t)| who(t)).collect();
            situation = format!("quiet {}", running.join("|"));
        } else if position < free.len() {
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
                // The target lock first, so the slot is never held while another build has the folder.
                let target = match lock.map(|l| switch::take(l, &describe(line))) {
                    None | Some(Take::Without) => None,
                    Some(Take::Held(target)) => Some(target),
                    Some(Take::Busy) => {
                        // Taken between looking and taking: look again shortly.
                        std::thread::sleep(POLL);
                        continue;
                    }
                };
                for index in free {
                    let Some(mut file) = open(&slot_path(config, class, index)) else {
                        continue;
                    };
                    if try_lock(&file) {
                        // A solo run and another job may each have taken a slot as the other looked: each looks
                        // again once it holds its own, so at least one sees the other and gives its slot back.
                        let quiet = if class == Class::Solo {
                            let _ = fs::remove_file(handed_back_path(config));
                            others_running(config).is_empty()
                        } else {
                            solo_blocks(config).is_none()
                        };
                        if !quiet {
                            unlock(&file);
                            break;
                        }
                        let text = describe(line);
                        write(&mut file, &text);
                        drop(ticket);
                        let held = Held {
                            file: Some(file),
                            index,
                            path: slot_path(config, class, index),
                            usage_at: text.len() as u64,
                            handed_back: (class == Class::Solo).then(|| handed_back_path(config)),
                        };
                        return (held, target);
                    }
                }
                // Someone took it between looking and taking, or a solo run came in: give the target lock back and
                // look again shortly.
                drop(target);
                std::thread::sleep(POLL);
                continue;
            }
        }
        if why.is_empty() {
            let running: Vec<String> = mine.iter().flatten().map(|t| summary(t)).collect();
            let passed = if place > position {
                format!(
                    " ({} ahead wait for another build to finish, this one goes past them)",
                    place - position
                )
            } else {
                String::new()
            };
            why = format!(
                "waiting for a {} slot: #{} in line{passed}; running: {}",
                class.name(),
                place + 1,
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
            // Free `now` slots are many and say nothing, and the solo slot matters only while it is taken.
            if matches!(class, Class::Now | Class::Solo) && holder.is_none() {
                continue;
            }
            let mut text = holder.as_deref().map_or("free".into(), summary);
            if class == Class::Solo {
                text += if handed_back(config) {
                    " — handed the machine back, others run beside it"
                } else {
                    " — has the machine, nothing else starts"
                };
            }
            println!("  {} slot {i}: {text}", class.name());
        }
        let waiting = line_up(config, class);
        let needed: Vec<Option<&str>> = waiting.iter().map(|(_, t)| needs(t)).collect();
        let held = held_up(&needed, |l| switch::busy(Path::new(l)));
        for (n, (_, text)) in waiting.iter().enumerate() {
            let mut line = summary(text);
            if let (true, Some(lock)) = (held[n], needed[n]) {
                line += &format!(
                    " — waits for another build into {}, later jobs go past it",
                    switch::lock_name(Path::new(lock))
                );
            }
            println!("  {} line #{}: {line}", class.name(), n + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_names_the_lock_its_waiter_needs() {
        let ticket = "1\n2\ngame\ncargo build --release\nneeds e:/game/target/.fgate-lock-release";
        assert_eq!(needs(ticket), Some("e:/game/target/.fgate-lock-release"));
        assert_eq!(needs("1\n2\ngame\nsh build.sh"), None);
        // What it runs is not what it needs, whatever the command says.
        assert_eq!(needs("1\n2\ngame\nneeds x"), None);
    }

    #[test]
    fn waiters_go_past_a_build_whose_folder_is_busy() {
        let (dev, release, ci) = (Some("debug"), Some("release"), Some("ci"));
        let asked = std::cell::RefCell::new(Vec::new());
        let busy = |lock: &str| {
            asked.borrow_mut().push(lock.to_string());
            lock == "debug"
        };
        let line = [dev, None, release, dev, release, ci, None];
        let held = held_up(&line, busy);
        // The dev builds wait for the running one; the second release build waits for the first, which goes.
        assert_eq!(held, [true, false, false, true, true, false, false]);
        assert_eq!(
            *asked.borrow(),
            ["debug", "release", "ci"],
            "each lock is asked once"
        );
        // With one free slot the script second in line goes first, then the release build; the dev build keeps
        // its place at the front for when the folder is free.
        let ahead = |me: usize| held[..me].iter().filter(|h| !**h).count();
        assert_eq!((ahead(1), ahead(2), ahead(6)), (0, 1, 3));
    }
}
