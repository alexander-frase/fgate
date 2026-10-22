//! `fgate watch`: a live view of the gate in the terminal. Who holds each slot, what its job uses, who waits,
//! the machine's memory and CPU, and the last commands that ran. It only reads what the gate keeps on disk,
//! so it can be started and stopped at any time without touching anyone's place.

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
/// The widest the view gets. Beyond the fixed columns, the room goes to commands and the sparklines.
const WIDEST: usize = 250;
/// The checkout-name column.
const PLACE: usize = 20;
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

    fn draw(&mut self, config: &Config, tick: usize, cols: usize, rows: usize) -> Vec<String> {
        let width = cols.clamp(40, WIDEST);
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
        // Gauges keep the size they have at 110 columns: a longer bar reads no better.
        let gauge = (width / 3).clamp(10, 36);
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

        let no_ceilings = self.no_ceilings.get_or_insert_with(job::ceilings).clone();
        let ceilings = match &no_ceilings {
            Some(why) => format!("no memory or CPU ceilings ({why})"),
            None => format!("{} GB per job", config.mem_gb),
        };
        let mut l = Line::new(width);
        l.push(
            DIM,
            &format!(
                " limits   {ceilings} · {} jobs · a second job waits below {} GB free{}",
                config.jobs,
                config.min_free_gb,
                if no_ceilings.is_none() && config.cpu_percent > 0 && config.cpu_percent < 100 {
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
            // `now` runs are short and the slots many: shown only while one runs, without the free ones.
            if *class == Class::Now && taken == 0 {
                continue;
            }
            let mut l = Line::new(width);
            l.push(&format!("{BOLD}{TEXT}"), &format!(" {}", heading(*class)));
            l.push(DIM, &format!("  {taken} of {} in use", holders.len()));
            out.push(l.done());
            for (i, holder) in holders.iter().enumerate() {
                match holder {
                    None if *class == Class::Now => {}
                    None => {
                        let mut l = Line::new(width);
                        l.push(GREEN, "   ○ ");
                        l.push(DIM, &format!("slot {i}  free"));
                        out.push(l.done());
                    }
                    Some(e) => {
                        let latest = output::kept(config, *class, i, e)
                            .and_then(|k| k.latest().map(str::to_string));
                        let mem_gb = config.mem_gb_for(*class);
                        let ceiling = if mem_gb > 0 {
                            mem_gb as u64 * (1 << 30)
                        } else {
                            job::memory().0
                        };
                        let share = e
                            .usage
                            .and_then(|u| self.job_cpu((e.place.clone(), e.since), u.cpu));
                        out.extend(holder_rows(
                            e,
                            i,
                            tick,
                            ceiling,
                            share,
                            latest.as_deref(),
                            width,
                        ));
                    }
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
            out.push(waiting_row(n, *class, e, width));
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
                out.push(recent_row(&d, gate::now(), width));
            }
        }

        // Footer, on the last row.
        while out.len() + 2 < rows {
            out.push(String::new());
        }
        out.push(rule(width));
        let mut l = Line::new(width);
        let path = format!(" {}", config.file().display());
        l.push(DIM, &path);
        let close = "Ctrl+C to close ";
        let hint = "fgate peek <name> follows a job's output";
        if path.chars().count() + hint.chars().count() + close.chars().count() + 6 <= width {
            l.right_many(&[(DIM, hint), (DIM, "   ·   "), (DIM, close)]);
        } else {
            l.right(DIM, close);
        }
        out.push(l.done());
        out
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
) -> Vec<String> {
    let mut rows = Vec::new();
    let mut l = Line::new(width);
    l.push(ACCENT, &format!("   {} ", SPINNER[tick % SPINNER.len()]));
    l.push(DIM, &format!("slot {index}  "));
    l.push(&format!("{BOLD}{CYAN}"), &format!("{:<PLACE$} ", e.place));
    l.push(&format!("{BOLD}{TEXT}"), &gate::duration(e.age()));
    rows.push(l.done());

    let mut l = Line::new(width);
    l.push(
        TEXT,
        &format!("     {}", clip(&e.command, width.saturating_sub(5))),
    );
    rows.push(l.done());

    match e.usage {
        None => {
            let mut l = Line::new(width);
            l.push(
                DIM,
                "     (memory and CPU show once this fgate is the new version)",
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
            l.push(DIM, "     mem ");
            l.bar(frac, (width / 4).clamp(8, 27), level(frac, 0.75, 0.92));
            l.push(
                TEXT,
                &format!("  {} of {}", gate::size(now), gate::size(ceiling)),
            );
            l.push(DIM, &format!("  peak {}", gate::size(usage.peak)));
            if let Some(share) = share {
                l.push(DIM, "  cpu ");
                l.push(level(share, 0.6, 0.85), &format!("{:.0}%", share * 100.0));
            }
            l.push(DIM, &format!("  {} procs", usage.processes));
            rows.push(l.done());
        }
    }

    if let Some(latest) = latest {
        let mut l = Line::new(width);
        l.push(DIM, "     › ");
        l.push(DIM, &clip(latest.trim_start(), width.saturating_sub(7)));
        rows.push(l.done());
    }
    rows
}

/// A place in line: its number, class and checkout, how long it has waited, then the command.
fn waiting_row(n: usize, class: Class, e: &Entry, width: usize) -> String {
    let mut l = Line::new(width);
    l.push(AMBER, &format!("   {:>2} ", n + 1));
    l.push(DIM, &format!("{:<6}", class.name()));
    l.push(CYAN, &format!("{:<PLACE$} ", clip(&e.place, PLACE - 1)));
    l.push(AMBER, &format!("{:>7}  ", gate::duration(e.age())));
    l.push(TEXT, &clip(&e.command, width.saturating_sub(l.width)));
    l.done()
}

/// A finished run: when, how it ended, its class and checkout, run time and peak memory, the exit code or the
/// wait, then the command, which takes the rest of the width.
fn recent_row(d: &gate::Done, now: u64, width: usize) -> String {
    let mut l = Line::new(width);
    let ago = Duration::from_secs(now.saturating_sub(d.ended));
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
    l.push(CYAN, &format!("{:<PLACE$} ", clip(&d.place, PLACE - 1)));
    let peak = if d.peak > 0 {
        gate::size(d.peak)
    } else {
        String::new()
    };
    l.push(DIM, &format!("{:>6} {:>8}  ", gate::duration(d.ran), peak));
    let (note, color) = if d.code != 0 {
        (format!("exit {}", d.code), RED)
    } else if d.waited.as_secs() >= 1 {
        (format!("waited {}", gate::duration(d.waited)), DIM)
    } else {
        (String::new(), DIM)
    };
    l.push(color, &format!("{note:<13}  "));
    l.push(TEXT, &clip(&d.command, width.saturating_sub(l.width)));
    l.done()
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
        Class::Now => "NOW",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A row as it shows: without its colour codes.
    fn shown(row: &str) -> String {
        let mut out = String::new();
        let mut chars = row.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
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
    fn recent_rows_put_the_command_last_and_fit() {
        for width in [80, 110, 210] {
            let failed = shown(&recent_row(&done(101, 0, LONG), 1_032, width));
            let fine = shown(&recent_row(&done(0, 0, LONG), 1_032, width));
            let waited = shown(&recent_row(&done(0, 156, LONG), 1_032, width));
            for row in [&failed, &fine, &waited] {
                assert!(row.chars().count() <= width, "{width}: {row}");
            }
            // The command starts in the same column, whatever the note says.
            let at = |row: &str| row.find("python").unwrap();
            assert_eq!(at(&failed), at(&fine));
            assert_eq!(at(&failed), at(&waited));
            assert!(failed.contains("exit 101") && waited.contains("waited 2m36s"));
        }
        let wide = shown(&recent_row(&done(0, 0, LONG), 1_032, 210));
        assert!(wide.ends_with(LONG), "{wide}");
        assert!(
            wide.starts_with("   32s ago  ✓ build spaceport-stage7      1m08s  11.0 GB"),
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
        for width in [80, 110, 210] {
            let rows: Vec<String> = holder_rows(
                &e,
                0,
                0,
                16 << 30,
                Some(0.11),
                Some("Compiling bevy_render v0.15.1"),
                width,
            )
            .iter()
            .map(|r| shown(r))
            .collect();
            assert_eq!(rows.len(), 4);
            for row in &rows {
                assert!(row.chars().count() <= width, "{width}: {row}");
            }
            // The time stays next to the name, not at the far edge.
            assert!(
                rows[0].contains("geothermal           1m01s"),
                "{}",
                rows[0]
            );
            assert_eq!(rows[3], "     › Compiling bevy_render v0.15.1");
        }
        let wide = holder_rows(&e, 0, 0, 16 << 30, None, None, 210);
        assert_eq!(wide.len(), 3);
        assert_eq!(shown(&wide[1]), format!("     {LONG}"));
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
        for width in [40, 80, 210] {
            let row = shown(&waiting_row(0, Class::Build, &e, width));
            assert!(row.chars().count() <= width, "{width}: {row}");
        }
        let row = shown(&waiting_row(0, Class::Build, &e, 210));
        assert!(
            row.starts_with("    1 build a-checkout-with-a-…    1m35s  python"),
            "{row}"
        );
    }
}
