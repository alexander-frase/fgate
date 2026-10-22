//! `fgate watch`: a live view of the gate in the terminal. Who holds each slot, what its job uses and its newest
//! line of output, who waits, the machine's memory and CPU, and the last commands that ran. It only reads what the
//! gate keeps on disk, so it can be started and stopped at any time without touching anyone's place.
//!
//! The screen is drawn as panels on a dark background. From `WIDE` columns, the jobs, the line and the recent runs
//! fill a column on the left and the machine and the limits sit in a sidebar on the right; narrower, the machine
//! panel goes on top. Each frame is built whole as a grid of cells and written over the last one.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::gate::{self, CLASSES, Class, Entry};
use crate::output;
use crate::sys::{self as job, QUIT};

/// How often the view redraws.
const FRAME: Duration = Duration::from_millis(250);
/// CPU and memory samples kept for the sparklines (one a second): enough to fill a wide terminal.
const HISTORY: usize = 150;
/// The checkout-name column.
const PLACE: usize = 20;
/// From this many columns, the machine and the limits go into a sidebar.
const WIDE: usize = 122;
/// The sidebar's width.
const SIDEBAR: usize = 38;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

type Rgb = (u8, u8, u8);

/// The screen's background, the panels' and every other row's in a list.
const BASE: Rgb = (15, 19, 25);
const PANEL: Rgb = (19, 25, 33);
const STRIPE: Rgb = (24, 31, 41);
const TEXT: Rgb = (218, 226, 235);
const MUTED: Rgb = (139, 156, 175);
/// The empty part of a gauge.
const TRACK: Rgb = (72, 84, 102);
const ACCENT: Rgb = (255, 140, 60);
const GREEN: Rgb = (90, 220, 130);
const AMBER: Rgb = (245, 195, 70);
const RED: Rgb = (245, 90, 90);
const CYAN: Rgb = (95, 200, 235);
const VIOLET: Rgb = (186, 164, 235);
const NIGHT: Rgb = (150, 140, 255);

/// How a piece of text is drawn: its colour, and whether bold.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ink {
    fg: Rgb,
    bold: bool,
}

const fn ink(fg: Rgb) -> Ink {
    Ink { fg, bold: false }
}

const fn bold(fg: Rgb) -> Ink {
    Ink { fg, bold: true }
}

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
        let busy = slots.iter().any(|(_, s)| s.iter().any(Option::is_some));
        let (total, free) = job::memory();
        let low = free < config.min_free_gb as u64 * (1 << 30);

        // Header: name and what it is on the left, version, limits and clock on the right.
        let margin = 2;
        let inner = cols - 2 * margin;
        let pulse = if busy && tick % 4 < 2 { ACCENT } else { MUTED };
        let mut l = Line::new(inner);
        l.push(ink(pulse), "◆ ");
        l.push(bold(ACCENT), "fgate");
        l.right(
            ink(MUTED),
            &format!("BUILD GATE  {}", env!("CARGO_PKG_VERSION")),
        );
        f.spans(margin, 1, inner, &l.done());
        let (h, m, s) = job::local_time();
        let mode = if config.night {
            (
                NIGHT,
                format!("● night limits until {}:00", config.night_until),
            )
        } else {
            (AMBER, "● day limits".to_string())
        };
        let mut l = Line::new(inner);
        l.push(
            ink(MUTED),
            "Coding agents take turns for builds, within memory and CPU ceilings.",
        );
        l.right_many(&[
            (ink(mode.0), &mode.1),
            (ink(MUTED), "   "),
            (ink(TEXT), &format!("{h:02}:{m:02}:{s:02}")),
        ]);
        f.spans(margin, 2, inner, &l.done());

        // Footer: what else there is, and where the settings are.
        let mut l = Line::new(inner);
        for (key, what) in [
            ("fgate peek <name>", " follow a job's output"),
            ("fgate status", " the same, once, as text"),
            ("Ctrl+C", " close"),
        ] {
            l.push(bold(TEXT), key);
            l.push(ink(MUTED), &format!("{what}    "));
        }
        f.spans(margin, rows - 2, inner, &l.done());
        let mut l = Line::new(inner);
        l.push(ink(MUTED), "settings  ");
        l.push(ink(MUTED), &config.file().display().to_string());
        f.spans(margin, rows - 1, inner, &l.done());

        // The body, between them.
        let top = 4;
        let height = rows.saturating_sub(top + 3);
        let wide = cols >= WIDE;
        let (left, mut y) = if wide {
            let left = inner - SIDEBAR - 2;
            self.sidebar(
                &mut f,
                config,
                Area::new(margin + left + 2, top, SIDEBAR, height),
                low,
                total,
                free,
            );
            (Area::new(margin, top, left, height), top)
        } else {
            let a = Area::new(margin, top, inner, height);
            let used = self.machine_wide(&mut f, config, a, low, total, free);
            (
                Area::new(
                    margin,
                    top + used + 1,
                    inner,
                    height.saturating_sub(used + 1),
                ),
                top + used + 1,
            )
        };
        let bottom = left.y + left.h;

        // A panel for each kind of slot (`now` only while one runs), then the line.
        let mut panels: Vec<(&str, Spans, Rows)> = Vec::new();
        for (class, holders) in &slots {
            let taken = holders.iter().filter(|h| h.is_some()).count();
            if *class == Class::Now && taken == 0 {
                continue;
            }
            let mut rows = Vec::new();
            let mut stripe = false;
            for (i, holder) in holders.iter().enumerate() {
                match holder {
                    None if *class == Class::Now => {}
                    None => {
                        let mut l = Line::new(left.w - 4);
                        l.push(ink(GREEN), "○ ");
                        l.push(ink(MUTED), &format!("slot {i}  free"));
                        rows.push((l.done(), PANEL));
                    }
                    Some(e) => {
                        let latest = output::kept(config, *class, i, e)
                            .and_then(|k| k.latest().map(str::to_string));
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
                        for row in
                            holder_rows(e, i, tick, ceiling, share, latest.as_deref(), left.w - 4)
                        {
                            rows.push((row, bg));
                        }
                    }
                }
            }
            let status = vec![(ink(MUTED), format!("{taken} of {} in use", holders.len()))];
            panels.push((class.name(), status, rows));
        }
        let mut status = Spans::new();
        if waiting.is_empty() {
            status.push((ink(MUTED), "nobody waiting".into()));
        } else {
            status.push((ink(AMBER), format!("{} waiting", waiting.len())));
            if (config.night || busy) && low {
                status.push((
                    ink(RED),
                    format!(" · held for memory (needs {} GB free)", config.min_free_gb),
                ));
            }
        }
        let queue = waiting
            .iter()
            .enumerate()
            .map(|(n, (class, e))| (waiting_row(n, *class, e, left.w - 4), PANEL))
            .collect();
        panels.push(("line", status, queue));

        // Room is kept for the recent runs: one gap row between panels when there is enough.
        let wanted: usize = panels.iter().map(|(_, _, r)| r.len() + 2).sum();
        let gap = usize::from(wanted + panels.len() + 6 <= left.h);
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
            let done = gate::history(config, room - 3);
            let mut rows = vec![(recent_heading(left.w - 4), PANEL)];
            if done.is_empty() {
                let mut l = Line::new(left.w - 4);
                l.push(ink(MUTED), "nothing yet");
                rows.push((l.done(), PANEL));
            }
            let now = gate::now();
            for (i, d) in done.iter().enumerate() {
                let bg = if i % 2 == 1 { STRIPE } else { PANEL };
                rows.push((recent_row(d, now, left.w - 4), bg));
            }
            let status = vec![(ink(MUTED), format!("last {}", done.len()))];
            f.panel(
                Area::new(left.x, y, left.w, room),
                "recent",
                &status,
                ACCENT,
                &rows,
            );
        }
        f
    }

    /// The sidebar: the machine's memory and CPU with their history, then the limits.
    fn sidebar(
        &mut self,
        f: &mut Frame,
        config: &Config,
        a: Area,
        low: bool,
        total: u64,
        free: u64,
    ) {
        let w = a.w - 4;
        let used = total.saturating_sub(free);
        let frac = used as f64 / total.max(1) as f64;
        let cpu = self.cpu.back().copied().unwrap_or(0.0);
        let bar = w.saturating_sub(7 + 9);
        let mut rows = Vec::new();

        let mut l = Line::new(w);
        l.push(ink(MUTED), "memory ");
        l.bar(frac, bar, level(frac, 0.7, 0.88));
        l.right(ink(TEXT), &gate::size(used));
        rows.push(l.done());
        let mut l = Line::new(w);
        l.push(ink(MUTED), &sparkline(&self.memory, w));
        rows.push(l.done());
        let mut l = Line::new(w);
        l.push(
            ink(if low { RED } else { MUTED }),
            &format!("{} free of {}", gate::size(free), gate::size(total)),
        );
        rows.push(l.done());
        rows.push(Spans::new());
        let mut l = Line::new(w);
        l.push(ink(MUTED), "cpu    ");
        l.bar(cpu, bar, level(cpu, 0.6, 0.85));
        l.right(ink(TEXT), &format!("{:.0}%", cpu * 100.0));
        rows.push(l.done());
        let mut l = Line::new(w);
        l.push(ink(CYAN), &sparkline(&self.cpu, w));
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

        let y = a.y + h + 1;
        if y + 3 > a.y + a.h {
            return;
        }
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
        let status = if config.night {
            vec![(ink(NIGHT), format!("night until {}:00", config.night_until))]
        } else {
            vec![(ink(AMBER), "day".to_string())]
        };
        let h = (rows.len() + 2).min(a.y + a.h - y);
        f.panel(Area::new(a.x, y, a.w, h), "limits", &status, VIOLET, &rows);
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
    /// one line. Says how many rows it took.
    fn machine_wide(
        &mut self,
        f: &mut Frame,
        config: &Config,
        a: Area,
        low: bool,
        total: u64,
        free: u64,
    ) -> usize {
        let w = a.w - 4;
        let used = total.saturating_sub(free);
        let frac = used as f64 / total.max(1) as f64;
        let cpu = self.cpu.back().copied().unwrap_or(0.0);
        let gauge = (w / 3).clamp(10, 36);
        let mut rows = Vec::new();

        let mut l = Line::new(w);
        l.push(ink(MUTED), "memory ");
        l.bar(frac, gauge, level(frac, 0.7, 0.88));
        l.push(
            ink(TEXT),
            &format!("  {} / {}", gate::size(used), gate::size(total)),
        );
        l.push(
            ink(if low { RED } else { MUTED }),
            &format!("  {} free", gate::size(free)),
        );
        let spark = w.saturating_sub(l.width + 3).min(HISTORY);
        l.right(ink(MUTED), &sparkline(&self.memory, spark));
        rows.push(l.done());

        let mut l = Line::new(w);
        l.push(ink(MUTED), "cpu    ");
        l.bar(cpu, gauge, level(cpu, 0.6, 0.85));
        l.push(ink(TEXT), &format!("  {:>3.0}%", cpu * 100.0));
        let spark = w.saturating_sub(l.width + 3).min(HISTORY);
        l.right(ink(CYAN), &sparkline(&self.cpu, spark));
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
            l.bar(frac, (width / 4).clamp(8, 27), level(frac, 0.75, 0.92));
            l.push(
                ink(TEXT),
                &format!("  {} of {}", gate::size(now), gate::size(ceiling)),
            );
            l.push(ink(MUTED), &format!("  peak {}", gate::size(usage.peak)));
            if let Some(share) = share {
                l.push(ink(MUTED), "  cpu ");
                l.push(
                    ink(level(share, 0.6, 0.85)),
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

/// A place in line: its number, class and checkout, how long it has waited, then the command.
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

/// The recent runs' column names, over `recent_row`'s columns.
fn recent_heading(width: usize) -> Spans {
    let mut l = Line::new(width);
    l.push(
        ink(MUTED),
        &format!(
            "{:>7}   {:<6}{:<PLACE$} {:>6} {:>8}  {:<13}  {}",
            "WHEN", "CLASS", "CHECKOUT", "RAN", "PEAK", "RESULT", "COMMAND"
        ),
    );
    l.done()
}

/// A finished run: when, how it ended, its class and checkout, run time and peak memory, the exit code or the
/// wait, then the command, which takes the rest of the width.
fn recent_row(d: &gate::Done, now: u64, width: usize) -> Spans {
    let mut l = Line::new(width);
    let ago = Duration::from_secs(now.saturating_sub(d.ended));
    l.push(
        ink(MUTED),
        &format!("{:>7} ", format!("{} ago", short_age(ago))),
    );
    if d.code == 0 {
        l.push(ink(GREEN), "✓ ");
    } else {
        l.push(ink(RED), "✗ ");
    }
    l.push(ink(MUTED), &format!("{:<6}", d.class));
    l.push(
        ink(CYAN),
        &format!("{:<PLACE$} ", clip(&d.place, PLACE - 1)),
    );
    let peak = if d.peak > 0 {
        gate::size(d.peak)
    } else {
        String::new()
    };
    l.push(
        ink(MUTED),
        &format!("{:>6} {:>8}  ", gate::duration(d.ran), peak),
    );
    let (note, color) = if d.code != 0 {
        (format!("exit {}", d.code), RED)
    } else if d.waited.as_secs() >= 1 {
        (format!("waited {}", gate::duration(d.waited)), MUTED)
    } else {
        (String::new(), MUTED)
    };
    l.push(ink(color), &format!("{note:<13}  "));
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
fn level(frac: f64, warn: f64, bad: f64) -> Rgb {
    if frac >= bad {
        RED
    } else if frac >= warn {
        AMBER
    } else {
        GREEN
    }
}

/// The last `n` samples as a sparkline.
fn sparkline(history: &VecDeque<f64>, n: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    history
        .iter()
        .skip(history.len().saturating_sub(n))
        .map(|v| BARS[((v * 7.0).round() as usize).min(7)])
        .collect()
}

/// At most `n` columns, with an ellipsis where it was cut.
fn clip(text: &str, n: usize) -> String {
    if width(text) <= n {
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

fn width(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// One row of a panel: pieces of text in their inks, cut at the row's width.
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
        self.spans.push((ink, cut));
    }

    /// A piece set against the right edge.
    fn right(&mut self, ink: Ink, piece: &str) {
        self.right_many(&[(ink, piece)]);
    }

    fn right_many(&mut self, pieces: &[(Ink, &str)]) {
        let len: usize = pieces.iter().map(|(_, p)| width(p)).sum();
        // When the row is full, the pieces are cut at the edge instead.
        let gap = self.max.saturating_sub(self.width + len);
        self.push(ink(TEXT), &" ".repeat(gap));
        for (ink, piece) in pieces {
            self.push(*ink, piece);
        }
    }

    /// A gauge `cells` wide, filled to `frac` in eighths of a cell.
    fn bar(&mut self, frac: f64, cells: usize, color: Rgb) {
        const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
        let eighths = (frac.clamp(0.0, 1.0) * cells as f64 * 8.0).round() as usize;
        let full = eighths / 8;
        let mut filled = "█".repeat(full);
        let mut rest = cells - full;
        if full < cells && !eighths.is_multiple_of(8) {
            filled.push(EIGHTHS[eighths % 8]);
            rest -= 1;
        }
        self.push(ink(color), &filled);
        self.push(ink(TRACK), &"░".repeat(rest));
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

    /// Writes one character, keeping the cell's background. A wide character that doesn't fit is left out.
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
        self.cells[at].0 = c;
        self.cells[at].1 = ink;
        if w == 2 {
            self.cells[at + 1].0 = WIDE_HALF;
            self.cells[at + 1].1 = ink;
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
        let title_w = width(&title);
        self.spans(a.x + 2, a.y, a.w - 4, &[(bold(accent), title)]);
        let status_w: usize = status.iter().map(|(_, s)| width(s)).sum();
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
            let mut last: Option<(Ink, Rgb)> = None;
            for x in 0..self.w.saturating_sub(1) {
                let (c, ink, bg) = self.cells[y * self.w + x];
                if c == WIDE_HALF {
                    continue;
                }
                if last != Some((ink, bg)) {
                    let (r, g, b) = ink.fg;
                    let (br, bgr, bb) = bg;
                    let weight = if ink.bold { ";1" } else { "" };
                    out += &format!("\x1b[0;38;2;{r};{g};{b};48;2;{br};{bgr};{bb}{weight}m");
                    last = Some((ink, bg));
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

    fn done(code: i32, waited: u64, command: &str) -> gate::Done {
        gate::Done {
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

    #[test]
    fn recent_rows_put_the_command_last_under_their_heading_and_fit() {
        for width in [76, 106, 206] {
            let failed = shown(&recent_row(&done(101, 0, LONG), 1_032, width));
            let fine = shown(&recent_row(&done(0, 0, LONG), 1_032, width));
            let waited = shown(&recent_row(&done(0, 156, LONG), 1_032, width));
            let heading = shown(&recent_heading(width));
            for row in [&failed, &fine, &waited, &heading] {
                assert!(super::width(row) <= width, "{width}: {row}");
            }
            // The command starts in the same column, whatever the note says, under its heading.
            let at = |row: &str, what: &str| row[..row.find(what).unwrap()].chars().count();
            assert_eq!(at(&failed, "python"), at(&fine, "python"));
            assert_eq!(at(&failed, "python"), at(&waited, "python"));
            assert_eq!(at(&failed, "python"), at(&heading, "COMMAND"));
            assert_eq!(at(&failed, "exit 101"), at(&heading, "RESULT"));
            assert_eq!(at(&failed, "spaceport"), at(&heading, "CHECKOUT"));
        }
        let wide = shown(&recent_row(&done(0, 0, LONG), 1_032, 206));
        assert!(wide.ends_with(LONG), "{wide}");
        assert!(
            wide.starts_with("32s ago ✓ build spaceport-stage7      1m08s  11.0 GB"),
            "{wide}"
        );
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
                assert!(super::width(row) <= width, "{width}: {row}");
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
    fn waiting_rows_fit() {
        let e = Entry {
            since: gate::now() - 95,
            pid: 1,
            place: "a-checkout-with-a-very-long-name".into(),
            command: LONG.into(),
            usage: None,
        };
        for width in [36, 76, 206] {
            let row = shown(&waiting_row(0, Class::Build, &e, width));
            assert!(super::width(&row) <= width, "{width}: {row}");
        }
        let row = shown(&waiting_row(0, Class::Build, &e, 206));
        assert!(
            row.starts_with(" 1 build a-checkout-with-a-…    1m35s  python"),
            "{row}"
        );
    }

    #[test]
    fn panels_have_a_titled_border_and_their_rows_inside() {
        let mut f = Frame::new(40, 6);
        let rows = vec![
            (vec![(ink(TEXT), "first".to_string())], PANEL),
            (vec![(ink(TEXT), "x".repeat(60))], STRIPE),
        ];
        let status = vec![(ink(MUTED), "2 of 2 in use".to_string())];
        f.panel(Area::new(1, 1, 30, 4), "build", &status, ACCENT, &rows);
        assert_eq!(f.row(0), " ".repeat(40));
        assert_eq!(
            f.row(1),
            format!(" ╭─ build ──── 2 of 2 in use ─╮{}", " ".repeat(9))
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
    fn wide_characters_take_two_cells_and_never_split() {
        let mut f = Frame::new(6, 1);
        f.spans(0, 0, 6, &[(ink(TEXT), "a漢b".to_string())]);
        assert_eq!(f.row(0), "a漢b  ");
        // A wide character that doesn't fit is left out.
        let mut f = Frame::new(3, 1);
        f.spans(0, 0, 3, &[(ink(TEXT), "ab漢".to_string())]);
        assert_eq!(f.row(0), "ab ");
        assert_eq!(clip("漢字漢字", 5), "漢字…");
        assert_eq!(width("✓ ✗ ◆ › …"), 9);
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
