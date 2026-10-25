//! `fgate watch`: a live view of the gate in the terminal. Who holds each slot, what its job uses and its newest
//! line of output, who waits, the machine's memory and CPU, and the last commands that ran. It only reads what the
//! gate keeps on disk, so it can be started and stopped at any time without touching anyone's place.
//!
//! The screen is drawn as panels on a dark background, under a header that sums up the gate in a few chips (how
//! many run and wait, free memory, recent failures). From `WIDE` columns the jobs and the recent runs fill a column
//! on the left, and the machine, the line and the limits sit in a sidebar on the right; narrower, the machine goes on
//! top and the line follows the jobs. Each frame is built whole as a grid of cells and written over the last one.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::gate::{self, CLASSES, Class, Done, Entry};
use crate::output;
use crate::sys::{self as job, QUIT};

/// How often the view redraws.
const FRAME: Duration = Duration::from_millis(250);
/// CPU and memory samples kept for the graphs (one a second): enough to fill a wide terminal.
const HISTORY: usize = 150;
/// The widest checkout-name column.
const PLACE: usize = 20;
/// From this many columns, the machine, the line and the limits go into a sidebar.
const WIDE: usize = 122;
/// The sidebar's width.
const SIDEBAR: usize = 38;
/// The command column of the recent runs keeps at least this much room; other columns go first.
const COMMAND_ROOM: usize = 40;
/// How far back the header counts failed runs.
const FAILED_WITHIN: u64 = 600;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

type Rgb = (u8, u8, u8);

/// The screen's background, the panels' and every other row's in a list.
const BASE: Rgb = (15, 19, 25);
const PANEL: Rgb = (19, 25, 33);
const STRIPE: Rgb = (24, 31, 41);
const TEXT: Rgb = (218, 226, 235);
const MUTED: Rgb = (139, 156, 175);
/// The empty part of a meter or graph.
const TRACK: Rgb = (58, 68, 84);
const ACCENT: Rgb = (255, 140, 60);
const GREEN: Rgb = (90, 220, 130);
const AMBER: Rgb = (245, 195, 70);
const RED: Rgb = (245, 90, 90);
const CYAN: Rgb = (95, 200, 235);
const VIOLET: Rgb = (186, 164, 235);
const NIGHT: Rgb = (150, 140, 255);
/// Memory and CPU turn amber, then red, at these shares.
const MEMORY_LEVELS: (f64, f64) = (0.7, 0.88);
const CPU_LEVELS: (f64, f64) = (0.6, 0.85);
const JOB_LEVELS: (f64, f64) = (0.75, 0.92);

/// How a piece of text is drawn: its colour, whether bold, and a background of its own (a chip), if any.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ink {
    fg: Rgb,
    bold: bool,
    bg: Option<Rgb>,
}

const fn ink(fg: Rgb) -> Ink {
    Ink {
        fg,
        bold: false,
        bg: None,
    }
}

const fn bold(fg: Rgb) -> Ink {
    Ink {
        fg,
        bold: true,
        bg: None,
    }
}

/// Text on a background of its own: `color` tinted for a state, or muted on a plain chip.
fn chip(color: Option<Rgb>) -> Ink {
    match color {
        Some(c) => Ink {
            fg: c,
            bold: true,
            bg: Some(blend(c, BASE, 0.16)),
        },
        None => Ink {
            fg: MUTED,
            bold: false,
            bg: Some(STRIPE),
        },
    }
}

/// A key or command, drawn like a keycap.
const KEYCAP: Ink = Ink {
    fg: TEXT,
    bold: true,
    bg: Some(STRIPE),
};

/// Pieces of text in their inks, making one row.
type Spans = Vec<(Ink, String)>;

/// A panel's rows: each row's text and its background.
type Rows = Vec<(Spans, Rgb)>;

pub fn run() -> ExitCode {
    job::catch_quit();
    let mut stdout = std::io::stdout();
    // Own screen, no cursor, a window title.
    let _ = write!(stdout, "\x1b[?1049h\x1b[?25l\x1b]0;fgate watch\x07");

    let mut view = View::default();
    let mut tick = 0usize;
    while !QUIT.load(Ordering::SeqCst) {
        let config = Config::load();
        view.sample();
        let (cols, rows) = job::console_size();
        let frame = view.draw(&config, tick, cols.max(40), rows.max(10));
        let _ = stdout.write_all(frame.render().as_bytes());
        let _ = stdout.flush();
        tick += 1;
        std::thread::sleep(FRAME);
    }
    let _ = write!(stdout, "\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

/// What the view remembers between frames: the CPU and memory history, and each job's last CPU reading.
#[derive(Default)]
struct View {
    cpu: VecDeque<f64>,
    memory: VecDeque<f64>,
    last_cpu: Option<(u64, u64)>,
    last_sample: Option<Instant>,
    /// Per running job (slot file text's start): its CPU time and when it was read, and its share since.
    jobs: HashMap<(String, u64), (u64, Instant, f64)>,
    /// Why there are no memory and CPU ceilings on this system, if there aren't (asked once).
    no_ceilings: Option<Option<String>>,
}

/// What one frame shows, read once.
struct State {
    slots: Vec<(Class, Vec<Option<Entry>>)>,
    waiting: Vec<(Class, Entry)>,
    total: u64,
    free: u64,
    /// Less memory free than a second job needs.
    low: bool,
    /// Waiters are held for memory, not for a slot.
    held: bool,
}

impl View {
    /// Takes the machine's CPU and memory once a second.
    fn sample(&mut self) {
        if self
            .last_sample
            .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
        {
            return;
        }
        self.last_sample = Some(Instant::now());
        let (busy, all) = job::cpu_times();
        if let Some((b0, a0)) = self.last_cpu {
            if all > a0 {
                push(&mut self.cpu, (busy - b0) as f64 / (all - a0) as f64);
            }
        }
        self.last_cpu = Some((busy, all));
        let (total, free) = job::memory();
        if total > 0 {
            push(&mut self.memory, 1.0 - free as f64 / total as f64);
        }
    }

    /// A running job's share of the whole machine's CPU, from how its CPU time grew.
    fn job_cpu(&mut self, key: (String, u64), cpu: u64) -> Option<f64> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
        let now = Instant::now();
        match self.jobs.get(&key).copied() {
            Some((c0, t0, share)) => {
                let secs = now.duration_since(t0).as_secs_f64();
                if secs < 1.5 {
                    // Not known yet (NaN) until a second reading.
                    return Some(share).filter(|s| !s.is_nan());
                }
                let share = (cpu.saturating_sub(c0) as f64 / 1e7 / secs / threads).clamp(0.0, 1.0);
                self.jobs.insert(key, (cpu, now, share));
                Some(share)
            }
            None => {
                self.jobs.insert(key, (cpu, now, f64::NAN));
                None
            }
        }
        .filter(|s| !s.is_nan())
    }

    fn draw(&mut self, config: &Config, tick: usize, cols: usize, rows: usize) -> Frame {
        let mut f = Frame::new(cols, rows);
        let slots: Vec<(Class, Vec<Option<Entry>>)> = CLASSES
            .iter()
            .map(|&c| (c, gate::slots(config, c)))
            .collect();
        let waiting: Vec<(Class, Entry)> = CLASSES
            .iter()
            .flat_map(|&c| gate::line(config, c).into_iter().map(move |e| (c, e)))
            .collect();
        let running = slots
            .iter()
            .map(|(_, s)| s.iter().filter(|h| h.is_some()).count())
            .sum::<usize>();
        let (total, free) = job::memory();
        let low = free < config.min_free_gb as u64 * (1 << 30);
        let state = State {
            held: !waiting.is_empty() && low && (config.night || running > 0),
            slots,
            waiting,
            total,
            free,
            low,
        };
        let now = gate::now();
        let history = gate::history(config, rows);
        let failed = history
            .iter()
            .filter(|d| d.code != 0 && now.saturating_sub(d.ended) <= FAILED_WITHIN)
            .count();

        // Header: the gate at a glance on the left, the limits in force and the clock on the right.
        let margin = 2;
        let inner = cols - 2 * margin;
        let pulse = if running > 0 && tick % 4 < 2 {
            ACCENT
        } else {
            MUTED
        };
        let mut l = Line::new(inner);
        l.push(ink(pulse), "◆ ");
        l.push(bold(ACCENT), "fgate");
        l.push(ink(TEXT), "   ");
        for (ink, text) in summary(running, state.waiting.len(), free, low, failed) {
            l.push(ink, &text);
        }
        let (h, m, s) = job::local_time();
        let mode = if config.night {
            (
                NIGHT,
                format!("● night limits until {}:00", config.night_until),
            )
        } else {
            (AMBER, "● day limits".to_string())
        };
        l.right_many(&[
            (ink(mode.0), &mode.1),
            (ink(MUTED), "   "),
            (ink(TEXT), &format!("{h:02}:{m:02}:{s:02}")),
        ]);
        f.spans(margin, 1, inner, &l.done());

        // Footer: what else there is, the version, and where the settings are.
        let mut l = Line::new(inner);
        for (key, what) in [
            ("fgate peek <name>", "follow a job's output"),
            ("fgate status", "print it once"),
            ("Ctrl+C", "close"),
        ] {
            l.push(KEYCAP, &format!(" {key} "));
            l.push(ink(MUTED), &format!(" {what}    "));
        }
        let version = format!("fgate {}", env!("CARGO_PKG_VERSION"));
        if l.width + width_of(&version) <= inner {
            l.right(ink(MUTED), &version);
        }
        f.spans(margin, rows - 2, inner, &l.done());
        let settings = format!("settings  {}", config.file().display());
        if width_of(&settings) <= inner {
            f.spans(margin, rows - 1, inner, &[(ink(MUTED), settings)]);
        }

        // The body, between them.
        let top = 3;
        let height = rows.saturating_sub(top + 3);
        let bottom = top + height;
        let wide = cols >= WIDE;
        let left = if wide {
            let w = inner - SIDEBAR - 2;
            let side = Area::new(margin + w + 2, top, SIDEBAR, height);
            self.sidebar(&mut f, config, &state, side);
            Area::new(margin, top, w, height)
        } else {
            let used = self.machine_across(
                &mut f,
                config,
                &state,
                Area::new(margin, top, inner, height),
            );
            Area::new(
                margin,
                top + used + 1,
                inner,
                height.saturating_sub(used + 1),
            )
        };
        let w = left.w - 4;

        // A panel for each kind of slot (`now` and `solo` only while one runs); the line too when there is no
        // sidebar.
        let mut panels: Vec<(&str, Spans, Rows)> = Vec::new();
        for (class, holders) in &state.slots {
            let taken = holders.iter().filter(|h| h.is_some()).count();
            let sometimes = matches!(class, Class::Now | Class::Solo);
            if sometimes && taken == 0 {
                continue;
            }
            let mut rows = Rows::new();
            let mut stripe = false;
            for (i, holder) in holders.iter().enumerate() {
                let Some(e) = holder else {
                    continue;
                };
                let latest =
                    output::kept(config, *class, i, e).and_then(|k| k.latest().map(str::to_string));
                let mem_gb = config.mem_gb_for(*class);
                let ceiling = if mem_gb > 0 {
                    mem_gb as u64 * (1 << 30)
                } else {
                    total
                };
                let share = e
                    .usage
                    .and_then(|u| self.job_cpu((e.place.clone(), e.since), u.cpu));
                let bg = if stripe { STRIPE } else { PANEL };
                stripe = !stripe;
                for row in holder_rows(e, i, tick, ceiling, share, latest.as_deref(), w) {
                    rows.push((row, bg));
                }
            }
            if !sometimes {
                if let Some(row) = free_row(holders, w) {
                    rows.push((row, PANEL));
                }
            }
            let mut status = vec![(
                ink(if taken > 0 { TEXT } else { MUTED }),
                format!("{taken}/{} busy", holders.len()),
            )];
            if *class == Class::Solo {
                status.push(if gate::handed_back(config) {
                    (ink(MUTED), " · handed back, others run beside it".into())
                } else {
                    (
                        ink(AMBER),
                        " · alone on the machine, nothing else starts".into(),
                    )
                });
            }
            let waiters = state.waiting.iter().filter(|(c, _)| c == class).count();
            if waiters > 0 {
                status.push((ink(AMBER), format!(" · {waiters} waiting")));
            }
            panels.push((class.name(), status, rows));
        }
        if !wide {
            let queue = state
                .waiting
                .iter()
                .enumerate()
                .map(|(n, (class, e))| (waiting_row(n, *class, e, w), PANEL))
                .collect();
            panels.push(("line", line_status(&state, config), queue));
        }

        // One gap row between panels when there is room to spare for the recent runs.
        let wanted: usize = panels.iter().map(|(_, _, r)| r.len() + 2).sum();
        let gap = usize::from(wanted + panels.len() + 6 <= left.h);
        let mut y = left.y;
        for (title, status, rows) in &panels {
            let room = bottom.saturating_sub(y);
            if room < 2 {
                break;
            }
            let h = (rows.len() + 2).min(room);
            f.panel(Area::new(left.x, y, left.w, h), title, status, ACCENT, rows);
            y += h + gap;
        }

        // What ran last, as much as fits.
        let room = bottom.saturating_sub(y);
        if room >= 4 {
            let done = &history[..history.len().min(room - 3)];
            let cols = RecentColumns::fit(done, w);
            let mut rows = vec![(recent_heading(&cols, w), PANEL)];
            if done.is_empty() {
                rows.push((vec![(ink(MUTED), "nothing yet".to_string())], PANEL));
            }
            for (i, d) in done.iter().enumerate() {
                let bg = if d.code != 0 {
                    blend(RED, PANEL, 0.12)
                } else if i % 2 == 1 {
                    STRIPE
                } else {
                    PANEL
                };
                rows.push((recent_row(d, now, &cols, w), bg));
            }
            let status = vec![(ink(MUTED), format!("{} runs", done.len()))];
            let h = (rows.len() + 2).min(room);
            f.panel(
                Area::new(left.x, y, left.w, h),
                "recent",
                &status,
                ACCENT,
                &rows,
            );
        }
        f
    }

    /// The sidebar: the machine's memory and CPU with their history, the line, then the limits.
    fn sidebar(&mut self, f: &mut Frame, config: &Config, state: &State, a: Area) {
        let w = a.w - 4;
        let bottom = a.y + a.h;
        let used = state.total.saturating_sub(state.free);
        let frac = used as f64 / state.total.max(1) as f64;
        let cpu = self.cpu.back().copied().unwrap_or(0.0);
        let mut rows = Vec::new();

        let mut l = Line::new(w);
        l.push(ink(MUTED), "memory");
        l.right(
            ink(TEXT),
            &format!("{} / {}", gate::size(used), gate::size(state.total)),
        );
        rows.push(l.done());
        let mut l = Line::new(w);
        l.meter(frac, w, MEMORY_LEVELS);
        rows.push(l.done());
        rows.extend(graph(&self.memory, w, MEMORY_LEVELS));
        let mut l = Line::new(w);
        l.push(
            ink(if state.low { RED } else { MUTED }),
            &format!("{} free", gate::size(state.free)),
        );
        l.right(ink(MUTED), &format!("last {}", span(w)));
        rows.push(l.done());
        rows.push(Spans::new());
        let mut l = Line::new(w);
        l.push(ink(MUTED), "cpu");
        l.right(ink(TEXT), &format!("{:.0}%", cpu * 100.0));
        rows.push(l.done());
        let mut l = Line::new(w);
        l.meter(cpu, w, CPU_LEVELS);
        rows.push(l.done());
        rows.extend(graph(&self.cpu, w, CPU_LEVELS));
        let mut l = Line::new(w);
        l.right(ink(MUTED), &format!("last {}", span(w)));
        rows.push(l.done());

        let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
        let status = vec![(ink(MUTED), format!("{threads} threads"))];
        let rows: Rows = rows.into_iter().map(|r| (r, PANEL)).collect();
        let h = (rows.len() + 2).min(a.h);
        f.panel(
            Area::new(a.x, a.y, a.w, h),
            "machine",
            &status,
            VIOLET,
            &rows,
        );
        let mut y = a.y + h + 1;

        // The line: each waiter's place, name and wait, then its command.
        let mut queue = Rows::new();
        if state.held {
            let mut l = Line::new(w);
            l.push(
                ink(RED),
                &format!("held: needs {} GB free", config.min_free_gb),
            );
            queue.push((l.done(), PANEL));
        }
        for (n, (class, e)) in state.waiting.iter().enumerate() {
            let bg = if n % 2 == 1 { STRIPE } else { PANEL };
            for row in queue_rows(n, *class, e, w) {
                queue.push((row, bg));
            }
        }
        if bottom.saturating_sub(y) >= 2 {
            let h = (queue.len() + 2).min(bottom - y);
            f.panel(
                Area::new(a.x, y, a.w, h),
                "line",
                &line_status(state, config),
                VIOLET,
                &queue,
            );
            y += h + 1;
        }

        // The limits, last: they rarely change.
        if bottom.saturating_sub(y) >= 3 {
            let rows: Rows = self
                .limits(config)
                .into_iter()
                .map(|(key, value)| {
                    let mut l = Line::new(w);
                    l.push(ink(MUTED), &format!("{key:<12}"));
                    l.push(ink(TEXT), &value);
                    (l.done(), PANEL)
                })
                .collect();
            let h = (rows.len() + 2).min(bottom - y);
            f.panel(Area::new(a.x, y, a.w, h), "limits", &[], VIOLET, &rows);
        }
    }

    /// The limits in force, as (what, value).
    fn limits(&mut self, config: &Config) -> Vec<(&'static str, String)> {
        let no_ceilings = self.no_ceilings.get_or_insert_with(job::ceilings).clone();
        let mut limits = Vec::new();
        match &no_ceilings {
            Some(why) => limits.push(("ceilings", format!("none ({why})"))),
            None => {
                limits.push(("per job", format!("{} GB memory", config.mem_gb)));
                let cpu = if config.cpu_percent > 0 && config.cpu_percent < 100 {
                    format!("≤ {}% of all threads", config.cpu_percent)
                } else {
                    "no ceiling".to_string()
                };
                limits.push(("cpu", cpu));
            }
        }
        limits.push(("compiler", format!("{} jobs per command", config.jobs)));
        limits.push((
            "second job",
            format!("waits below {} GB free", config.min_free_gb),
        ));
        limits.push((
            "night",
            format!("{}:00 until {}:00", config.night_from, config.night_until),
        ));
        limits
    }

    /// The machine across the top, for narrower terminals: memory and CPU with their history, and the limits on
    /// one row. Says how many rows it took.
    fn machine_across(&mut self, f: &mut Frame, config: &Config, state: &State, a: Area) -> usize {
        let w = a.w - 4;
        let used = state.total.saturating_sub(state.free);
        let frac = used as f64 / state.total.max(1) as f64;
        let cpu = self.cpu.back().copied().unwrap_or(0.0);
        let gauge = (w / 3).clamp(10, 36);
        let mut rows = Vec::new();

        let mut l = Line::new(w);
        l.push(ink(MUTED), "memory ");
        l.meter(frac, gauge, MEMORY_LEVELS);
        l.push(
            ink(TEXT),
            &format!("  {} / {}", gate::size(used), gate::size(state.total)),
        );
        l.push(
            ink(if state.low { RED } else { MUTED }),
            &format!("  {} free", gate::size(state.free)),
        );
        let spark = w.saturating_sub(l.width + 12).min(HISTORY);
        l.right_many(&[
            (ink(MUTED), &sparkline(&self.memory, spark)),
            (ink(MUTED), &format!("  {:>7}", span(spark))),
        ]);
        rows.push(l.done());

        let mut l = Line::new(w);
        l.push(ink(MUTED), "cpu    ");
        l.meter(cpu, gauge, CPU_LEVELS);
        l.push(ink(TEXT), &format!("  {:>3.0}%", cpu * 100.0));
        l.right_many(&[
            (ink(CYAN), &sparkline(&self.cpu, spark)),
            (ink(MUTED), &format!("  {:>7}", span(spark))),
        ]);
        rows.push(l.done());

        // The limits, short enough for one row.
        let no_ceilings = self.no_ceilings.get_or_insert_with(job::ceilings).clone();
        let mut limits = vec![match &no_ceilings {
            Some(why) => format!("no memory or CPU ceilings ({why})"),
            None => format!("{} GB per job", config.mem_gb),
        }];
        if no_ceilings.is_none() && config.cpu_percent > 0 && config.cpu_percent < 100 {
            limits.push(format!("CPU ≤ {}%", config.cpu_percent));
        }
        limits.push(format!("{} jobs", config.jobs));
        limits.push(format!(
            "a second job waits below {} GB free",
            config.min_free_gb
        ));
        let mut l = Line::new(w);
        l.push(ink(MUTED), &limits.join(" · "));
        rows.push(l.done());

        let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
        let status = vec![(ink(MUTED), format!("{threads} threads"))];
        let rows: Rows = rows.into_iter().map(|r| (r, PANEL)).collect();
        let h = (rows.len() + 2).min(a.h);
        f.panel(
            Area::new(a.x, a.y, a.w, h),
            "machine",
            &status,
            VIOLET,
            &rows,
        );
        h
    }
}

/// The header's chips: how many run and wait, free memory, and recent failures (only when there are some).
fn summary(running: usize, waiting: usize, free: u64, low: bool, failed: usize) -> Spans {
    let mut chips = vec![
        (format!("{running} running"), (running > 0).then_some(GREEN)),
        (format!("{waiting} waiting"), (waiting > 0).then_some(AMBER)),
        (format!("{} free", gate::size(free)), low.then_some(RED)),
    ];
    if failed > 0 {
        chips.push((
            format!("{failed} failed in {} min", FAILED_WITHIN / 60),
            Some(RED),
        ));
    }
    let mut spans = Spans::new();
    for (text, color) in chips {
        spans.push((chip(color), format!(" {text} ")));
        spans.push((ink(TEXT), " ".to_string()));
    }
    spans
}

/// The line panel's status: how many wait.
fn line_status(state: &State, config: &Config) -> Spans {
    if state.waiting.is_empty() {
        return vec![(ink(MUTED), "nobody waiting".to_string())];
    }
    let mut status = vec![(ink(AMBER), format!("{} waiting", state.waiting.len()))];
    if state.held {
        status.push((
            ink(RED),
            format!(" · held for {} GB free", config.min_free_gb),
        ));
    }
    status
}

/// The free slots of a kind, on one row: `○ 0  ○ 1  free`.
fn free_row(holders: &[Option<Entry>], width: usize) -> Option<Spans> {
    let free: Vec<usize> = holders
        .iter()
        .enumerate()
        .filter(|(_, h)| h.is_none())
        .map(|(i, _)| i)
        .collect();
    if free.is_empty() {
        return None;
    }
    let mut l = Line::new(width);
    for i in free {
        l.push(ink(GREEN), "○ ");
        l.push(ink(MUTED), &format!("{i}  "));
    }
    l.push(ink(MUTED), "free");
    Some(l.done())
}

/// A slot's holder: who and how long, the command, what its job uses, and its newest line of output when kept.
/// The figures sit next to the name; the command and the output line take the width.
fn holder_rows(
    e: &Entry,
    index: usize,
    tick: usize,
    ceiling: u64,
    share: Option<f64>,
    latest: Option<&str>,
    width: usize,
) -> Vec<Spans> {
    let mut rows = Vec::new();
    let mut l = Line::new(width);
    l.push(ink(ACCENT), &format!("{} ", SPINNER[tick % SPINNER.len()]));
    l.push(ink(MUTED), &format!("slot {index}  "));
    l.push(bold(CYAN), &format!("{:<PLACE$} ", e.place));
    l.push(bold(TEXT), &gate::duration(e.age()));
    rows.push(l.done());

    let mut l = Line::new(width);
    l.push(
        ink(TEXT),
        &format!("  {}", clip(&e.command, width.saturating_sub(2))),
    );
    rows.push(l.done());

    match e.usage {
        None => {
            let mut l = Line::new(width);
            l.push(
                ink(MUTED),
                "  (memory and CPU show once this fgate is the new version)",
            );
            rows.push(l.done());
        }
        Some(usage) => {
            let now = if usage.memory > 0 {
                usage.memory
            } else {
                usage.peak
            };
            let frac = now as f64 / ceiling.max(1) as f64;
            let mut l = Line::new(width);
            l.push(ink(MUTED), "  mem ");
            l.meter(frac, (width / 4).clamp(8, 27), JOB_LEVELS);
            l.push(
                ink(TEXT),
                &format!("  {} of {}", gate::size(now), gate::size(ceiling)),
            );
            l.push(ink(MUTED), &format!("  peak {}", gate::size(usage.peak)));
            if let Some(share) = share {
                l.push(ink(MUTED), "  cpu ");
                l.push(
                    ink(level(share, CPU_LEVELS)),
                    &format!("{:.0}%", share * 100.0),
                );
            }
            l.push(ink(MUTED), &format!("  {} procs", usage.processes));
            rows.push(l.done());
        }
    }

    if let Some(latest) = latest {
        let mut l = Line::new(width);
        l.push(ink(MUTED), "  › ");
        l.push(
            ink(MUTED),
            &clip(latest.trim_start(), width.saturating_sub(4)),
        );
        rows.push(l.done());
    }
    rows
}

/// A place in line on one row (no sidebar): its number, class and checkout, how long it has waited, the command.
fn waiting_row(n: usize, class: Class, e: &Entry, width: usize) -> Spans {
    let mut l = Line::new(width);
    l.push(ink(AMBER), &format!("{:>2} ", n + 1));
    l.push(ink(MUTED), &format!("{:<6}", class.name()));
    l.push(
        ink(CYAN),
        &format!("{:<PLACE$} ", clip(&e.place, PLACE - 1)),
    );
    l.push(ink(AMBER), &format!("{:>7}  ", gate::duration(e.age())));
    l.push(ink(TEXT), &clip(&e.command, width.saturating_sub(l.width)));
    l.done()
}

/// A place in line in the sidebar, on two rows: number, class, checkout and wait, then the command.
fn queue_rows(n: usize, class: Class, e: &Entry, width: usize) -> [Spans; 2] {
    let mut l = Line::new(width);
    l.push(ink(AMBER), &format!("{:>2} ", n + 1));
    l.push(ink(MUTED), &format!("{:<6}", class.name()));
    let wait = gate::duration(e.age());
    l.push(
        ink(CYAN),
        &clip(
            &e.place,
            width.saturating_sub(l.width + width_of(&wait) + 1),
        ),
    );
    l.right(ink(AMBER), &wait);
    let first = l.done();
    let mut l = Line::new(width);
    l.push(
        ink(MUTED),
        &format!("   {}", clip(&e.command, width.saturating_sub(3))),
    );
    [first, l.done()]
}

/// Which columns the recent runs show, and how wide, for the rows shown and the room there is: the checkout and
/// the result as wide as their longest, and the class, then the peak, left out before the command gets too
/// little room.
#[derive(Debug, PartialEq, Eq)]
struct RecentColumns {
    class: bool,
    peak: bool,
    place: usize,
    /// 0: no row has a result to show, and the column is left out.
    note: usize,
}

impl RecentColumns {
    fn fit(runs: &[Done], width: usize) -> RecentColumns {
        let place = runs
            .iter()
            .map(|d| width_of(&d.place))
            .max()
            .unwrap_or(0)
            .clamp(8, PLACE);
        let note = runs.iter().map(|d| width_of(&note(d).0)).max().unwrap_or(0);
        let mut cols = RecentColumns {
            class: true,
            peak: true,
            place,
            note: if note > 0 { note.max(6) } else { 0 },
        };
        if width.saturating_sub(cols.fixed()) < COMMAND_ROOM {
            cols.class = false;
        }
        if width.saturating_sub(cols.fixed()) < COMMAND_ROOM {
            cols.peak = false;
        }
        cols
    }

    /// The columns before the command.
    fn fixed(&self) -> usize {
        5 + 2
            + if self.class { 6 } else { 0 }
            + self.place
            + 1
            + 6
            + if self.peak { 9 } else { 0 }
            + 2
            + if self.note > 0 { self.note + 2 } else { 0 }
    }
}

/// A run's result to show: its exit code when it failed, how long it waited when that was long enough to say.
fn note(d: &Done) -> (String, Rgb) {
    if d.code != 0 {
        (format!("exit {}", d.code), RED)
    } else if d.waited.as_secs() >= 1 {
        (format!("waited {}", gate::duration(d.waited)), MUTED)
    } else {
        (String::new(), MUTED)
    }
}

/// The recent runs' column names, over `recent_row`'s columns.
fn recent_heading(cols: &RecentColumns, width: usize) -> Spans {
    let mut text = format!("{:>4}   ", "WHEN");
    if cols.class {
        text += &format!("{:<6}", "CLASS");
    }
    text += &format!("{:<w$} {:>6}", "CHECKOUT", "RAN", w = cols.place);
    if cols.peak {
        text += &format!("{:>9}", "PEAK");
    }
    text += "  ";
    if cols.note > 0 {
        text += &format!("{:<w$}  ", "RESULT", w = cols.note);
    }
    text += "COMMAND";
    let mut l = Line::new(width);
    l.push(ink(MUTED), &text);
    l.done()
}

/// A finished run: how long ago, how it ended, its class and checkout, run time and peak memory, the exit code or
/// the wait, then the command, which takes the rest of the width.
fn recent_row(d: &Done, now: u64, cols: &RecentColumns, width: usize) -> Spans {
    let mut l = Line::new(width);
    let ago = Duration::from_secs(now.saturating_sub(d.ended));
    l.push(ink(MUTED), &format!("{:>4} ", short_age(ago)));
    if d.code == 0 {
        l.push(ink(GREEN), "✓ ");
    } else {
        l.push(bold(RED), "✗ ");
    }
    if cols.class {
        l.push(ink(MUTED), &format!("{:<6}", d.class));
    }
    l.push(
        ink(TEXT),
        &format!("{:<w$} ", clip(&d.place, cols.place), w = cols.place),
    );
    l.push(ink(MUTED), &format!("{:>6}", gate::duration(d.ran)));
    if cols.peak {
        let peak = if d.peak > 0 {
            gate::size(d.peak)
        } else {
            String::new()
        };
        l.push(ink(MUTED), &format!("{peak:>9}"));
    }
    l.push(ink(MUTED), "  ");
    if cols.note > 0 {
        let (text, color) = note(d);
        l.push(ink(color), &format!("{text:<w$}  ", w = cols.note));
    }
    l.push(ink(TEXT), &clip(&d.command, width.saturating_sub(l.width)));
    l.done()
}

fn push(history: &mut VecDeque<f64>, value: f64) {
    history.push_back(value.clamp(0.0, 1.0));
    while history.len() > HISTORY {
        history.pop_front();
    }
}

/// Green, amber or red for a share, by its two thresholds.
fn level(frac: f64, (warn, bad): (f64, f64)) -> Rgb {
    if frac >= bad {
        RED
    } else if frac >= warn {
        AMBER
    } else {
        GREEN
    }
}

/// The last `n` samples as a one-row sparkline.
fn sparkline(history: &VecDeque<f64>, n: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    history
        .iter()
        .skip(history.len().saturating_sub(n))
        .map(|v| BARS[((v * 7.0).round() as usize).min(7)])
        .collect()
}

/// The last `n` samples as a graph two rows tall (top row first), newest on the right, each column coloured by its
/// level. Columns without a sample yet, and empty ones, show a flat baseline.
fn graph(history: &VecDeque<f64>, n: usize, levels: (f64, f64)) -> [Spans; 2] {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut top = Line::new(n);
    let mut low = Line::new(n);
    let shown: Vec<f64> = history
        .iter()
        .skip(history.len().saturating_sub(n))
        .copied()
        .collect();
    for _ in shown.len()..n {
        top.push(ink(TRACK), " ");
        low.push(ink(TRACK), "▁");
    }
    for v in shown {
        let eighths = (v * 16.0).round() as usize;
        let color = if eighths == 0 {
            TRACK
        } else {
            level(v, levels)
        };
        let lower = match eighths {
            0 => '▁',
            1..=8 => BARS[eighths - 1],
            _ => '█',
        };
        let upper = if eighths > 8 {
            BARS[(eighths - 9).min(7)]
        } else {
            ' '
        };
        top.push(ink(color), &upper.to_string());
        low.push(ink(color), &lower.to_string());
    }
    [top.done(), low.done()]
}

/// The time `n` one-second samples cover: `34s`, `2 min`.
fn span(n: usize) -> String {
    if n < 120 {
        format!("{n}s")
    } else {
        format!("{} min", n / 60)
    }
}

/// At most `n` columns, with an ellipsis where it was cut.
fn clip(text: &str, n: usize) -> String {
    if width_of(text) <= n {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = cell_width(c);
        if used + w + 1 > n {
            break;
        }
        out.push(c);
        used += w;
    }
    if n > 0 {
        out.push('…');
    }
    out
}

/// `40s`, `12m`, `3h`, `2d`.
fn short_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// How many terminal columns a character takes: two for East Asian wide characters and most emoji, none for
/// combining marks and joiners, one otherwise.
fn cell_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F => 0,
        0x1100..=0x115F
        | 0x2E80..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => 2,
        _ if c.is_control() => 0,
        _ => 1,
    }
}

/// How many terminal columns a text takes.
fn width_of(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// One row of a panel: pieces of text in their inks, cut at the row's width. Pieces in the same ink are joined.
struct Line {
    spans: Spans,
    width: usize,
    max: usize,
}

impl Line {
    fn new(max: usize) -> Line {
        Line {
            spans: Vec::new(),
            width: 0,
            max,
        }
    }

    fn push(&mut self, ink: Ink, piece: &str) {
        let room = self.max.saturating_sub(self.width);
        if room == 0 || piece.is_empty() {
            return;
        }
        let mut cut = String::new();
        let mut used = 0;
        for c in piece.chars() {
            let w = cell_width(c);
            if used + w > room {
                break;
            }
            cut.push(c);
            used += w;
        }
        self.width += used;
        match self.spans.last_mut() {
            Some((last, text)) if *last == ink => text.push_str(&cut),
            _ => self.spans.push((ink, cut)),
        }
    }

    /// A piece set against the right edge.
    fn right(&mut self, ink: Ink, piece: &str) {
        self.right_many(&[(ink, piece)]);
    }

    fn right_many(&mut self, pieces: &[(Ink, &str)]) {
        let len: usize = pieces.iter().map(|(_, p)| width_of(p)).sum();
        // When the row is full, the pieces are cut at the edge instead.
        let gap = self.max.saturating_sub(self.width + len);
        self.push(ink(TEXT), &" ".repeat(gap));
        for (ink, piece) in pieces {
            self.push(*ink, piece);
        }
    }

    /// A meter `cells` wide: a thin line filled to `frac`, each filled cell green, amber or red by where it sits,
    /// so a fuller meter runs into the warning colours. Anything above nothing shows at least one cell.
    fn meter(&mut self, frac: f64, cells: usize, levels: (f64, f64)) {
        let frac = frac.clamp(0.0, 1.0);
        let filled = if frac > 0.0 {
            ((frac * cells as f64).round() as usize).clamp(1, cells)
        } else {
            0
        };
        for i in 0..cells {
            let color = if i < filled {
                level((i as f64 + 0.5) / cells as f64, levels)
            } else {
                TRACK
            };
            self.push(ink(color), "━");
        }
    }

    fn done(self) -> Spans {
        self.spans
    }
}

/// A rectangle of the screen, in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Area {
    fn new(x: usize, y: usize, w: usize, h: usize) -> Area {
        Area { x, y, w, h }
    }
}

/// The second half of a wide character, which takes the cell after it.
const WIDE_HALF: char = '\0';

/// One screen: each cell's character, ink and background.
struct Frame {
    w: usize,
    h: usize,
    cells: Vec<(char, Ink, Rgb)>,
}

impl Frame {
    fn new(w: usize, h: usize) -> Frame {
        Frame {
            w,
            h,
            cells: vec![(' ', ink(TEXT), BASE); w * h],
        }
    }

    /// Writes one character, keeping the cell's background unless the ink brings its own. A wide character that
    /// doesn't fit is left out.
    fn put(&mut self, x: usize, y: usize, c: char, ink: Ink) {
        let w = cell_width(c);
        if w == 0 || y >= self.h || x + w > self.w {
            return;
        }
        let at = y * self.w + x;
        // A wide character partly written over loses its other half.
        if self.cells[at].0 == WIDE_HALF && x > 0 {
            self.cells[at - 1].0 = ' ';
        }
        if x + w < self.w && self.cells[at + w].0 == WIDE_HALF {
            self.cells[at + w].0 = ' ';
        }
        for i in 0..w {
            let cell = &mut self.cells[at + i];
            cell.0 = if i == 0 { c } else { WIDE_HALF };
            cell.1 = ink;
            if let Some(bg) = ink.bg {
                cell.2 = bg;
            }
        }
    }

    /// Writes a row of spans from `x`, at most `max` columns.
    fn spans(&mut self, x: usize, y: usize, max: usize, spans: &[(Ink, String)]) {
        let mut col = x;
        for (ink, text) in spans {
            for c in text.chars() {
                let w = cell_width(c);
                if col + w > x + max {
                    return;
                }
                self.put(col, y, c, *ink);
                col += w;
            }
        }
    }

    /// Paints an area's background, emptying it.
    fn fill(&mut self, a: Area, bg: Rgb) {
        for y in a.y..(a.y + a.h).min(self.h) {
            for x in a.x..(a.x + a.w).min(self.w) {
                self.cells[y * self.w + x] = (' ', ink(TEXT), bg);
            }
        }
    }

    /// A panel: a rounded border in a dimmed `accent`, the title set into its top edge on the left and a status on
    /// the right, then the rows inside, each on its own background, as many as fit.
    fn panel(
        &mut self,
        a: Area,
        title: &str,
        status: &[(Ink, String)],
        accent: Rgb,
        rows: &[(Spans, Rgb)],
    ) {
        if a.w < 8 || a.h < 2 {
            return;
        }
        self.fill(a, PANEL);
        let border = ink(blend(accent, PANEL, 0.45));
        let rule = "─".repeat(a.w - 2);
        self.spans(a.x, a.y, a.w, &[(border, format!("╭{rule}╮"))]);
        self.spans(a.x, a.y + a.h - 1, a.w, &[(border, format!("╰{rule}╯"))]);
        for y in a.y + 1..a.y + a.h - 1 {
            self.put(a.x, y, '│', border);
            self.put(a.x + a.w - 1, y, '│', border);
        }
        let title = format!(" {title} ");
        let title_w = width_of(&title);
        self.spans(a.x + 2, a.y, a.w - 4, &[(bold(accent), title)]);
        let status_w: usize = status.iter().map(|(_, s)| width_of(s)).sum();
        if status_w > 0 && title_w + status_w + 7 <= a.w {
            let x = a.x + a.w - 3 - status_w - 1;
            self.put(x, a.y, ' ', border);
            self.spans(x + 1, a.y, status_w, status);
            self.put(x + 1 + status_w, a.y, ' ', border);
        }
        for (i, (spans, bg)) in rows.iter().take(a.h - 2).enumerate() {
            let y = a.y + 1 + i;
            self.fill(Area::new(a.x + 1, y, a.w - 2, 1), *bg);
            self.spans(a.x + 2, y, a.w - 4, spans);
        }
    }

    /// The frame as terminal output: each row placed by the cursor, colours set only where they change. The last
    /// column is filled by erasing to the end of the row in the background colour, so no terminal wraps or
    /// scrolls.
    fn render(&self) -> String {
        let mut out = String::new();
        for y in 0..self.h {
            out += &format!("\x1b[{};1H", y + 1);
            let mut last: Option<(Rgb, bool, Rgb)> = None;
            for x in 0..self.w.saturating_sub(1) {
                let (c, ink, bg) = self.cells[y * self.w + x];
                if c == WIDE_HALF {
                    continue;
                }
                if last != Some((ink.fg, ink.bold, bg)) {
                    let (r, g, b) = ink.fg;
                    let (br, bgr, bb) = bg;
                    let weight = if ink.bold { ";1" } else { "" };
                    out += &format!("\x1b[0;38;2;{r};{g};{b};48;2;{br};{bgr};{bb}{weight}m");
                    last = Some((ink.fg, ink.bold, bg));
                }
                out.push(c);
            }
            let (br, bgr, bb) = self.cells[y * self.w + self.w - 1].2;
            out += &format!("\x1b[0;48;2;{br};{bgr};{bb}m\x1b[K");
        }
        out + "\x1b[0m"
    }

    /// The text of one row, as it shows (for tests).
    #[cfg(test)]
    fn row(&self, y: usize) -> String {
        self.cells[y * self.w..(y + 1) * self.w]
            .iter()
            .filter(|c| c.0 != WIDE_HALF)
            .map(|c| c.0)
            .collect()
    }
}

/// `a` mixed with `b`: `share` of `a`.
fn blend(a: Rgb, b: Rgb, share: f64) -> Rgb {
    let mix = |x: u8, y: u8| (x as f64 * share + y as f64 * (1.0 - share)).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row as it shows: its text without inks.
    fn shown(spans: &Spans) -> String {
        spans.iter().map(|(_, s)| s.as_str()).collect()
    }

    fn done(code: i32, waited: u64, command: &str) -> Done {
        Done {
            ended: 1_000,
            class: "build".into(),
            place: "spaceport-stage7".into(),
            command: command.into(),
            ran: Duration::from_secs(68),
            waited: Duration::from_secs(waited),
            peak: 11 << 30,
            code,
        }
    }

    const LONG: &str = "python -I C:/Users/alexander/.claude/jobs/2233184a/tmp/shots.py C:/Users/alexander/.claude/jobs/2233184a/out";

    /// The column a text starts at in a row, in characters.
    fn at(row: &str, what: &str) -> usize {
        row[..row.find(what).unwrap_or_else(|| panic!("{what} in {row}"))]
            .chars()
            .count()
    }

    #[test]
    fn recent_rows_sit_under_their_headings_and_fit() {
        let runs = [done(101, 0, LONG), done(0, 0, LONG), done(0, 156, LONG)];
        for width in [76, 106, 206] {
            let cols = RecentColumns::fit(&runs, width);
            let heading = shown(&recent_heading(&cols, width));
            let rows: Vec<String> = runs
                .iter()
                .map(|d| shown(&recent_row(d, 1_032, &cols, width)))
                .collect();
            for row in rows.iter().chain([&heading]) {
                assert!(width_of(row) <= width, "{width}: {row}");
                assert!(!row.contains("ago"), "{row}");
            }
            for row in &rows {
                assert_eq!(at(row, "python"), at(&heading, "COMMAND"), "{row}");
                assert_eq!(at(row, "spaceport"), at(&heading, "CHECKOUT"), "{row}");
            }
            assert_eq!(at(&rows[0], "exit 101"), at(&heading, "RESULT"));
            assert_eq!(at(&rows[2], "waited 2m36s"), at(&heading, "RESULT"));
        }
        let wide = RecentColumns::fit(&runs, 206);
        assert_eq!(
            wide,
            RecentColumns {
                class: true,
                peak: true,
                place: 16,
                note: 12
            }
        );
        let row = shown(&recent_row(&runs[1], 1_032, &wide, 206));
        assert!(
            row.starts_with(" 32s ✓ build spaceport-stage7  1m08s  11.0 GB  "),
            "{row}"
        );
        assert!(row.ends_with(LONG), "{row}");
        // Narrow: the class goes first, then the peak.
        let narrow = RecentColumns::fit(&runs, 76);
        assert!(!narrow.class && !narrow.peak, "{narrow:?}");
        let middle = RecentColumns::fit(&runs, 100);
        assert!(!middle.class && middle.peak, "{middle:?}");
    }

    #[test]
    fn the_result_column_goes_when_no_run_has_one() {
        let runs = [done(0, 0, "cargo build")];
        let cols = RecentColumns::fit(&runs, 206);
        assert_eq!(cols.note, 0);
        assert!(!shown(&recent_heading(&cols, 206)).contains("RESULT"));
    }

    #[test]
    fn the_summary_shows_failures_only_when_there_are_some() {
        let text = |s: Spans| shown(&s);
        assert_eq!(
            text(summary(3, 0, 34 << 30, false, 0)),
            " 3 running   0 waiting   34.0 GB free  "
        );
        let busy = summary(2, 1, 3 << 30, true, 2);
        assert!(text(busy.clone()).contains(" 2 failed in 10 min "));
        // Waiting and low memory are tinted; nothing waiting is a plain chip.
        assert_eq!(busy[2].0.fg, AMBER);
        assert_eq!(busy[4].0.fg, RED);
        assert_eq!(summary(0, 0, 0, false, 0)[2].0, chip(None));
    }

    #[test]
    fn meters_fill_from_green_into_the_warning_colours() {
        let mut l = Line::new(10);
        l.meter(0.95, 10, MEMORY_LEVELS);
        let spans = l.done();
        assert_eq!(
            spans
                .iter()
                .map(|(i, s)| (i.fg, s.chars().count()))
                .collect::<Vec<_>>(),
            [(GREEN, 7), (AMBER, 2), (RED, 1)]
        );
        let mut l = Line::new(10);
        l.meter(0.001, 10, MEMORY_LEVELS);
        let spans = l.done();
        assert_eq!((spans[0].0.fg, spans[0].1.chars().count()), (GREEN, 1));
        assert_eq!((spans[1].0.fg, spans[1].1.chars().count()), (TRACK, 9));
    }

    #[test]
    fn graphs_are_two_rows_tall_with_the_newest_on_the_right() {
        let history: VecDeque<f64> = [0.0, 0.5, 1.0].into();
        let [top, low] = graph(&history, 4, MEMORY_LEVELS);
        assert_eq!(shown(&top), "   █");
        assert_eq!(shown(&low), "▁▁██");
        assert_eq!(low.last().unwrap().0.fg, RED);
        assert_eq!(span(34), "34s");
        assert_eq!(span(150), "2 min");
    }

    #[test]
    fn holder_rows_keep_the_figures_together_and_show_the_newest_line() {
        let e = Entry {
            since: gate::now() - 61,
            pid: 1,
            place: "geothermal".into(),
            command: LONG.into(),
            usage: Some(crate::sys::Usage {
                memory: 401 << 20,
                peak: 2 << 30,
                cpu: 0,
                processes: 9,
            }),
        };
        for width in [76, 106, 206] {
            let rows: Vec<String> = holder_rows(
                &e,
                0,
                0,
                16 << 30,
                Some(0.11),
                Some("   Compiling bevy_render v0.15.1"),
                width,
            )
            .iter()
            .map(shown)
            .collect();
            assert_eq!(rows.len(), 4);
            for row in &rows {
                assert!(width_of(row) <= width, "{width}: {row}");
            }
            // The time stays next to the name, not at the far edge.
            assert!(
                rows[0].contains("geothermal           1m01s"),
                "{}",
                rows[0]
            );
            assert_eq!(rows[3], "  › Compiling bevy_render v0.15.1");
        }
        let wide = holder_rows(&e, 0, 0, 16 << 30, None, None, 206);
        assert_eq!(wide.len(), 3);
        assert_eq!(shown(&wide[1]), format!("  {LONG}"));
    }

    #[test]
    fn free_slots_share_one_row() {
        let holders = [None, Some(entry("x", 5)), None];
        assert_eq!(shown(&free_row(&holders, 40).unwrap()), "○ 0  ○ 2  free");
        assert!(free_row(&[Some(entry("x", 5))], 40).is_none());
    }

    fn entry(place: &str, age: u64) -> Entry {
        Entry {
            since: gate::now() - age,
            pid: 1,
            place: place.into(),
            command: LONG.into(),
            usage: None,
        }
    }

    #[test]
    fn waiting_rows_fit() {
        let e = entry("a-checkout-with-a-very-long-name", 95);
        for width in [36, 76, 206] {
            let row = shown(&waiting_row(0, Class::Build, &e, width));
            assert!(width_of(&row) <= width, "{width}: {row}");
        }
        let row = shown(&waiting_row(0, Class::Build, &e, 206));
        assert!(
            row.starts_with(" 1 build a-checkout-with-a-…    1m35s  python"),
            "{row}"
        );
        let [first, second] = queue_rows(0, Class::Build, &e, 34);
        assert_eq!(shown(&first), " 1 build a-checkout-with-a-… 1m35s");
        assert_eq!(width_of(&shown(&second)), 34);
        assert!(shown(&second).starts_with("   python -I"));
    }

    #[test]
    fn panels_have_a_titled_border_and_their_rows_inside() {
        let mut f = Frame::new(40, 6);
        let rows = vec![
            (vec![(ink(TEXT), "first".to_string())], PANEL),
            (vec![(ink(TEXT), "x".repeat(60))], STRIPE),
        ];
        let status = vec![(ink(MUTED), "2/2 busy".to_string())];
        f.panel(Area::new(1, 1, 30, 4), "build", &status, ACCENT, &rows);
        assert_eq!(f.row(0), " ".repeat(40));
        assert_eq!(
            f.row(1),
            format!(" ╭─ build ───────── 2/2 busy ─╮{}", " ".repeat(9))
        );
        assert_eq!(
            f.row(2),
            format!(" │ first{}│{}", " ".repeat(22), " ".repeat(9))
        );
        assert_eq!(
            f.row(3),
            format!(" │ {} │{}", "x".repeat(26), " ".repeat(9))
        );
        assert_eq!(f.row(4), format!(" ╰{}╯{}", "─".repeat(28), " ".repeat(9)));
        // The second row sits on the stripe background, inside the border only.
        assert_eq!(f.cells[3 * 40 + 2].2, STRIPE);
        assert_eq!(f.cells[3 * 40 + 1].2, PANEL);
        assert_eq!(f.cells[3 * 40].2, BASE);
    }

    #[test]
    fn chips_bring_their_own_background() {
        let mut f = Frame::new(10, 1);
        f.spans(0, 0, 10, &[(ink(TEXT), "a".into()), (KEYCAP, " k ".into())]);
        assert_eq!(f.cells[0].2, BASE);
        assert!(f.cells[1..4].iter().all(|c| c.2 == STRIPE));
        assert_eq!(f.cells[4].2, BASE);
    }

    #[test]
    fn wide_characters_take_two_cells_and_never_split() {
        let mut f = Frame::new(6, 1);
        f.spans(0, 0, 6, &[(ink(TEXT), "a漢b".to_string())]);
        assert_eq!(f.row(0), "a漢b  ");
        // A wide character that doesn't fit is left out.
        let mut f = Frame::new(3, 1);
        f.spans(0, 0, 3, &[(ink(TEXT), "ab漢".to_string())]);
        assert_eq!(f.row(0), "ab ");
        assert_eq!(clip("漢字漢字", 5), "漢字…");
        assert_eq!(width_of("✓ ✗ ◆ › … ━"), 11);
    }

    #[test]
    fn rendering_sets_colours_only_where_they_change_and_never_touches_the_last_column() {
        let mut f = Frame::new(4, 2);
        f.spans(
            0,
            0,
            4,
            &[
                (bold(ACCENT), "ab".to_string()),
                (ink(TEXT), "cd".to_string()),
            ],
        );
        let out = f.render();
        assert_eq!(
            out.matches("\x1b[0;38;2;255;140;60;48;2;15;19;25;1m")
                .count(),
            1
        );
        assert!(
            out.contains("ab\x1b[0;38;2;218;226;235;48;2;15;19;25mc\x1b[0;48;2;15;19;25m\x1b[K")
        );
        assert!(!out.contains('d'));
    }
}
