//! `fgate watch`: a live view of the gate in the terminal. Who holds each slot, what its job uses, who waits,
//! the machine's memory and CPU, and the last commands that ran. It only reads what the gate keeps on disk,
//! so it can be started and stopped at any time without touching anyone's place.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
    GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
    SetConsoleMode,
};

use crate::config::Config;
use crate::gate::{self, CLASSES, Class, Entry};
use crate::job;

/// How often the view redraws.
const FRAME: Duration = Duration::from_millis(250);
/// CPU and memory samples kept for the sparklines (one a second).
const HISTORY: usize = 40;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[38;2;105;110;125m";
const TEXT: &str = "\x1b[38;2;215;218;226m";
const ACCENT: &str = "\x1b[38;2;255;140;60m";
const GREEN: &str = "\x1b[38;2;90;220;130m";
const AMBER: &str = "\x1b[38;2;245;195;70m";
const RED: &str = "\x1b[38;2;245;90;90m";
const CYAN: &str = "\x1b[38;2;95;200;235m";
const NIGHT: &str = "\x1b[38;2;150;140;255m";
const TRACK: &str = "\x1b[38;2;55;58;70m";

static QUIT: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(_: u32) -> i32 {
    QUIT.store(true, Ordering::SeqCst);
    1
}

pub fn run() -> ExitCode {
    // SAFETY: plain console calls on this process's own output handle.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(out, &mut mode) != 0 {
            SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
    let mut stdout = std::io::stdout();
    // Own screen, no cursor, a window title.
    let _ = write!(stdout, "\x1b[?1049h\x1b[?25l\x1b]0;fgate watch\x07");

    let mut view = View::default();
    let mut tick = 0usize;
    while !QUIT.load(Ordering::SeqCst) {
        let config = Config::load();
        view.sample();
        let (cols, rows) = console_size();
        let frame = view.draw(&config, tick, cols, rows);
        let _ = write!(stdout, "\x1b[H{}\x1b[K\x1b[J", frame.join("\x1b[K\r\n"));
        let _ = stdout.flush();
        tick += 1;
        std::thread::sleep(FRAME);
    }
    let _ = write!(stdout, "{RESET}\x1b[?25h\x1b[?1049l");
    let _ = stdout.flush();
    ExitCode::SUCCESS
}

/// The console window's (columns, rows).
fn console_size() -> (usize, usize) {
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

/// What the view remembers between frames: the CPU and memory history, and each job's last CPU reading.
#[derive(Default)]
struct View {
    cpu: VecDeque<f64>,
    memory: VecDeque<f64>,
    last_cpu: Option<(u64, u64)>,
    last_sample: Option<Instant>,
    /// Per running job (slot file text's start): its CPU time and when it was read, and its share since.
    jobs: HashMap<(String, u64), (u64, Instant, f64)>,
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
        if let Some((b0, a0)) = self.last_cpu
            && all > a0
        {
            push(&mut self.cpu, (busy - b0) as f64 / (all - a0) as f64);
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
                    return Some(share);
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

    fn draw(&mut self, config: &Config, tick: usize, cols: usize, rows: usize) -> Vec<String> {
        let width = cols.clamp(40, 110);
        let mut out = Vec::new();
        let slots: Vec<(Class, Vec<Option<Entry>>)> = CLASSES
            .iter()
            .map(|&c| (c, gate::slots(config, c)))
            .collect();
        let waiting: Vec<(Class, Entry)> = CLASSES
            .iter()
            .flat_map(|&c| gate::line(config, c).into_iter().map(move |e| (c, e)))
            .collect();
        let busy = slots.iter().any(|(_, s)| s.iter().any(Option::is_some));

        // Title bar.
        let (h, m, s) = job::local_time();
        let mut l = Line::new(width);
        let pulse = if busy && tick % 4 < 2 { ACCENT } else { DIM };
        l.push(pulse, " ◆ ");
        l.push(&format!("{BOLD}{ACCENT}"), "FGATE");
        l.push(DIM, "  the build gate");
        let mode = if config.night {
            (
                NIGHT,
                format!("night limits until {}:00", config.night_until),
            )
        } else {
            (AMBER, "day limits".to_string())
        };
        let clock = format!("{h:02}:{m:02}:{s:02} ");
        l.right_many(&[
            (mode.0, &format!("● {}", mode.1)),
            (DIM, "   "),
            (TEXT, &clock),
        ]);
        out.push(l.done());
        out.push(rule(width));

        // The machine.
        let (total, free) = job::memory();
        let used = total.saturating_sub(free);
        let frac = if total > 0 {
            used as f64 / total as f64
        } else {
            0.0
        };
        let gauge = (width / 3).max(10);
        let mut l = Line::new(width);
        l.push(DIM, " MEMORY ");
        l.bar(frac, gauge, level(frac, 0.7, 0.88));
        l.push(
            TEXT,
            &format!("  {} / {}", gate::size(used), gate::size(total)),
        );
        let low = free < config.min_free_gb as u64 * (1 << 30);
        l.push(
            if low { RED } else { DIM },
            &format!("  {} free", gate::size(free)),
        );
        l.right(
            DIM,
            &sparkline(&self.memory, width.saturating_sub(l.width + 4).min(HISTORY)),
        );
        out.push(l.done());

        let cpu = self.cpu.back().copied().unwrap_or(0.0);
        let mut l = Line::new(width);
        l.push(DIM, " CPU    ");
        l.bar(cpu, gauge, level(cpu, 0.6, 0.85));
        l.push(TEXT, &format!("  {:>3.0}%", cpu * 100.0));
        l.push(
            DIM,
            &format!(
                "  {} threads",
                std::thread::available_parallelism().map_or(0, |n| n.get())
            ),
        );
        l.right(
            CYAN,
            &sparkline(&self.cpu, width.saturating_sub(l.width + 4).min(HISTORY)),
        );
        out.push(l.done());

        let mut l = Line::new(width);
        l.push(
            DIM,
            &format!(
                " limits   {} GB per job · {} jobs · a second job waits below {} GB free{}",
                config.mem_gb,
                config.jobs,
                config.min_free_gb,
                if config.cpu_percent > 0 && config.cpu_percent < 100 {
                    format!(" · CPU ≤ {}%", config.cpu_percent)
                } else {
                    String::new()
                }
            ),
        );
        out.push(l.done());
        out.push(String::new());

        // The slots.
        for (class, holders) in &slots {
            let taken = holders.iter().filter(|h| h.is_some()).count();
            let mut l = Line::new(width);
            l.push(&format!("{BOLD}{TEXT}"), &format!(" {}", heading(*class)));
            l.push(DIM, &format!("  {taken} of {} in use", holders.len()));
            out.push(l.done());
            for (i, holder) in holders.iter().enumerate() {
                match holder {
                    None => {
                        let mut l = Line::new(width);
                        l.push(GREEN, "   ○ ");
                        l.push(DIM, &format!("slot {i}  free"));
                        out.push(l.done());
                    }
                    Some(e) => self.draw_holder(&mut out, config, e, i, tick, width),
                }
            }
            out.push(String::new());
        }

        // The line.
        let mut l = Line::new(width);
        l.push(&format!("{BOLD}{TEXT}"), " LINE");
        if waiting.is_empty() {
            l.push(DIM, "  nobody waiting");
        } else {
            l.push(AMBER, &format!("  {} waiting", waiting.len()));
            let night_or_second = config.night || busy;
            if night_or_second && low {
                l.push(
                    RED,
                    &format!("  · held for memory (needs {} GB free)", config.min_free_gb),
                );
            }
        }
        out.push(l.done());
        for (n, (class, e)) in waiting.iter().enumerate() {
            let mut l = Line::new(width);
            l.push(AMBER, &format!("   {:>2} ", n + 1));
            l.push(DIM, &format!("{:<6}", class.name()));
            l.push(CYAN, &format!("{:<20} ", clip(&e.place, 19)));
            let age = format!("  {}", gate::duration(e.age()));
            l.push(
                TEXT,
                &clip(&e.command, width.saturating_sub(l.width + age.len() + 1)),
            );
            l.right(AMBER, &age);
            out.push(l.done());
        }
        out.push(String::new());

        // What ran last, as much as fits.
        let room = rows.saturating_sub(out.len() + 3);
        if room > 0 {
            let mut l = Line::new(width);
            l.push(&format!("{BOLD}{TEXT}"), " RECENT");
            out.push(l.done());
            let done = gate::history(config, room);
            if done.is_empty() {
                let mut l = Line::new(width);
                l.push(
                    DIM,
                    "   nothing yet (recorded from this version of fgate on)",
                );
                out.push(l.done());
            }
            for d in done {
                let mut l = Line::new(width);
                let ago = Duration::from_secs(gate::now().saturating_sub(d.ended));
                l.push(
                    DIM,
                    &format!("   {:>7} ", format!("{} ago", short_age(ago))),
                );
                if d.code == 0 {
                    l.push(GREEN, " ✓ ");
                } else {
                    l.push(RED, " ✗ ");
                }
                l.push(DIM, &format!("{:<6}", d.class));
                l.push(CYAN, &format!("{:<20} ", clip(&d.place, 19)));
                // Fixed columns: run time, peak memory, then the exit code or the wait.
                let peak = if d.peak > 0 {
                    gate::size(d.peak)
                } else {
                    String::new()
                };
                let note = if d.code != 0 {
                    format!("exit {}", d.code)
                } else if d.waited.as_secs() >= 1 {
                    format!("waited {}", gate::duration(d.waited))
                } else {
                    String::new()
                };
                let figures = format!("  {:>6} {:>8}", gate::duration(d.ran), peak);
                let note = format!("  {note:<14}");
                l.push(
                    TEXT,
                    &clip(
                        &d.command,
                        width.saturating_sub(
                            l.width + 1 + figures.chars().count() + note.chars().count(),
                        ),
                    ),
                );
                l.right_many(&[
                    (DIM, &figures),
                    (if d.code == 0 { DIM } else { RED }, &note),
                ]);
                out.push(l.done());
            }
        }

        // Footer, on the last row.
        while out.len() + 2 < rows {
            out.push(String::new());
        }
        out.push(rule(width));
        let mut l = Line::new(width);
        l.push(DIM, &format!(" {}", config.file().display()));
        l.right(DIM, "Ctrl+C to close ");
        out.push(l.done());
        out
    }

    fn draw_holder(
        &mut self,
        out: &mut Vec<String>,
        config: &Config,
        e: &Entry,
        index: usize,
        tick: usize,
        width: usize,
    ) {
        let mut l = Line::new(width);
        l.push(ACCENT, &format!("   {} ", SPINNER[tick % SPINNER.len()]));
        l.push(DIM, &format!("slot {index}  "));
        l.push(&format!("{BOLD}{CYAN}"), &e.place);
        l.right(&format!("{BOLD}{TEXT}"), &gate::duration(e.age()));
        out.push(l.done());

        let mut l = Line::new(width);
        l.push(
            TEXT,
            &format!("     {}", clip(&e.command, width.saturating_sub(6))),
        );
        out.push(l.done());

        let Some(usage) = e.usage else {
            let mut l = Line::new(width);
            l.push(
                DIM,
                "     (memory and CPU show once this fgate is the new version)",
            );
            out.push(l.done());
            return;
        };
        let ceiling = if config.mem_gb > 0 {
            config.mem_gb as u64 * (1 << 30)
        } else {
            job::memory().0
        };
        let now = if usage.memory > 0 {
            usage.memory
        } else {
            usage.peak
        };
        let frac = now as f64 / ceiling.max(1) as f64;
        let mut l = Line::new(width);
        l.push(DIM, "     mem ");
        l.bar(frac, (width / 4).max(8), level(frac, 0.75, 0.92));
        l.push(
            TEXT,
            &format!("  {} of {}", gate::size(now), gate::size(ceiling)),
        );
        l.push(DIM, &format!("  peak {}", gate::size(usage.peak)));
        let share = self.job_cpu((e.place.clone(), e.since), usage.cpu);
        if let Some(share) = share {
            l.push(DIM, "  cpu ");
            l.push(level(share, 0.6, 0.85), &format!("{:.0}%", share * 100.0));
        }
        l.push(DIM, &format!("  {} procs", usage.processes));
        out.push(l.done());
    }
}

fn push(history: &mut VecDeque<f64>, value: f64) {
    history.push_back(value.clamp(0.0, 1.0));
    while history.len() > HISTORY {
        history.pop_front();
    }
}

fn heading(class: Class) -> &'static str {
    match class {
        Class::Build => "BUILD",
        Class::Gpu => "GPU",
    }
}

/// Green, amber or red for a share, by its two thresholds.
fn level(frac: f64, warn: f64, bad: f64) -> &'static str {
    if frac >= bad {
        RED
    } else if frac >= warn {
        AMBER
    } else {
        GREEN
    }
}

fn rule(width: usize) -> String {
    format!("{TRACK}{}{RESET}", "─".repeat(width))
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

/// At most `n` characters, with an ellipsis where it was cut.
fn clip(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_string()
    } else if n == 0 {
        String::new()
    } else {
        text.chars().take(n - 1).collect::<String>() + "…"
    }
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

/// One row of the view: coloured pieces, cut at the view's width.
struct Line {
    text: String,
    width: usize,
    max: usize,
}

impl Line {
    fn new(max: usize) -> Line {
        Line {
            text: String::new(),
            width: 0,
            max,
        }
    }

    fn push(&mut self, color: &str, piece: &str) {
        let room = self.max.saturating_sub(self.width);
        if room == 0 {
            return;
        }
        let piece: String = piece.chars().take(room).collect();
        self.width += piece.chars().count();
        self.text += color;
        self.text += &piece;
        self.text += RESET;
    }

    /// A piece set against the right edge.
    fn right(&mut self, color: &str, piece: &str) {
        self.right_many(&[(color, piece)]);
    }

    fn right_many(&mut self, pieces: &[(&str, &str)]) {
        let len: usize = pieces.iter().map(|(_, p)| p.chars().count()).sum();
        // When the row is full, the pieces are cut at the edge instead.
        let gap = self.max.saturating_sub(self.width + len);
        self.push("", &" ".repeat(gap));
        for (color, piece) in pieces {
            self.push(color, piece);
        }
    }

    /// A gauge `cells` wide, filled to `frac` in eighths of a cell.
    fn bar(&mut self, frac: f64, cells: usize, color: &str) {
        const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
        let eighths = (frac.clamp(0.0, 1.0) * cells as f64 * 8.0).round() as usize;
        let full = eighths / 8;
        let mut filled = "█".repeat(full);
        let mut rest = cells - full;
        if full < cells && !eighths.is_multiple_of(8) {
            filled.push(EIGHTHS[eighths % 8]);
            rest -= 1;
        }
        self.push(color, &filled);
        self.push(TRACK, &"░".repeat(rest));
    }

    fn done(self) -> String {
        self.text
    }
}
