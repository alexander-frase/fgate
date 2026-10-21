//! fgate's settings: one file per machine, so every agent counts the same slots. At night (local time) stricter
//! limits take over, so unattended agents can't run the machine out of memory.

use std::path::{Path, PathBuf};

pub struct Config {
    pub dir: PathBuf,
    /// Builds and test runs at once.
    pub build_slots: usize,
    /// GPU runs at once (games, renderers, screenshot scripts).
    pub gpu_slots: usize,
    /// Small commands at once that skip the line (`fgate --now`).
    pub now_slots: usize,
    /// The memory ceiling of each `fgate --now` command.
    pub now_mem_gb: u32,
    /// `CARGO_BUILD_JOBS` for a gated cargo command that doesn't set its own.
    pub jobs: u32,
    /// Each gated command's share of all CPU threads, a hard ceiling (0 or 100: none).
    pub cpu_percent: u32,
    /// Each gated command's memory ceiling, everything it starts together (0: none).
    pub mem_gb: u32,
    /// A second command waits while less memory than this is free (at night, any command).
    pub min_free_gb: u32,
    /// The night's local hours, `from` until `until` (wrapping past midnight).
    pub night_from: u32,
    pub night_until: u32,
    pub night_build_slots: usize,
    pub night_mem_gb: u32,
    pub night_min_free_gb: u32,
    /// Whether the night limits are in force for this command.
    pub night: bool,
    /// Before a compiling cargo command into a shared target directory, clean the workspace's own packages there
    /// when another checkout built that profile last (`switch`; 0: off).
    pub switch_clean: u32,
    /// After a build into a shared target directory, copy the workspace's executables into the checkout's
    /// `target/fgate-bin/<profile>/` (`switch`; 0: off).
    pub keep_bins: u32,
}

const KEYS: [&str; 15] = [
    "build_slots",
    "gpu_slots",
    "now_slots",
    "now_mem_gb",
    "jobs",
    "cpu_percent",
    "mem_gb",
    "min_free_gb",
    "night_from",
    "night_until",
    "night_build_slots",
    "night_mem_gb",
    "night_min_free_gb",
    "switch_clean",
    "keep_bins",
];

impl Config {
    pub fn load() -> Config {
        let dir = std::env::var_os("FGATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(crate::sys::settings_dir);
        let total_gb = (crate::sys::memory().0 as f64 / (1u64 << 30) as f64).round() as u32;
        let mut c = Config {
            dir,
            build_slots: 1,
            gpu_slots: 1,
            now_slots: 4,
            now_mem_gb: 2,
            jobs: 6,
            cpu_percent: 40,
            mem_gb: (total_gb * 2 / 5).max(8),
            min_free_gb: 6,
            night_from: 23,
            night_until: 7,
            night_build_slots: 1,
            night_mem_gb: 10,
            night_min_free_gb: 8,
            night: false,
            switch_clean: 1,
            keep_bins: 1,
        };
        let file = c.file();
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.split('#').next().unwrap_or("").trim();
                    if let Some((key, value)) = line.split_once('=') {
                        c.set(key.trim(), value.trim());
                    }
                }
                let mut more = String::new();
                if !text.contains("night_from") {
                    more += &c.night_text();
                }
                if !text.contains("now_slots") {
                    more += &c.now_text();
                }
                if !text.contains("switch_clean") {
                    more += &c.switch_text();
                }
                if !text.contains("keep_bins") {
                    more += &c.keep_text();
                }
                if !more.is_empty() {
                    let gap = if text.ends_with('\n') { "" } else { "\n" };
                    let _ = std::fs::write(&file, format!("{text}{gap}{more}"));
                }
            }
            Err(_) => c.write_default(&file),
        }
        for key in KEYS {
            if let Ok(value) = std::env::var(format!("FGATE_{}", key.to_uppercase())) {
                c.set(key, &value);
            }
        }
        c.night = is_night(crate::sys::local_hour(), c.night_from, c.night_until);
        if c.night {
            c.build_slots = c.night_build_slots;
            c.mem_gb = c.night_mem_gb;
            c.min_free_gb = c.night_min_free_gb;
        }
        c.build_slots = c.build_slots.max(1);
        c.gpu_slots = c.gpu_slots.max(1);
        c.now_slots = c.now_slots.max(1);
        c
    }

    /// The memory ceiling of each command of `class`, in GB (0: none).
    pub fn mem_gb_for(&self, class: crate::gate::Class) -> u32 {
        match class {
            crate::gate::Class::Now => self.now_mem_gb,
            _ => self.mem_gb,
        }
    }

    pub fn file(&self) -> PathBuf {
        self.dir.join("config.txt")
    }

    fn set(&mut self, key: &str, value: &str) {
        let Ok(n) = value.parse::<u32>() else {
            warn(&format!(
                "ignoring setting {key} = {value} (not a whole number)"
            ));
            return;
        };
        match key {
            "build_slots" => self.build_slots = n as usize,
            "gpu_slots" => self.gpu_slots = n as usize,
            "now_slots" => self.now_slots = n as usize,
            "now_mem_gb" => self.now_mem_gb = n,
            "jobs" => self.jobs = n.max(1),
            "cpu_percent" => self.cpu_percent = n.min(100),
            "mem_gb" => self.mem_gb = n,
            "min_free_gb" => self.min_free_gb = n,
            "night_from" => self.night_from = n.min(24),
            "night_until" => self.night_until = n.min(24),
            "night_build_slots" => self.night_build_slots = n as usize,
            "night_mem_gb" => self.night_mem_gb = n,
            "night_min_free_gb" => self.night_min_free_gb = n,
            "switch_clean" => self.switch_clean = n.min(1),
            "keep_bins" => self.keep_bins = n.min(1),
            // A setting from a newer fgate, or a typo.
            _ => warn(&format!("unknown setting {key}")),
        }
    }

    fn night_text(&self) -> String {
        format!(
            "# Night limits, local hours night_from until night_until (0 and 24: always). At night a command\n\
             # also waits for free memory when it would run alone, so unattended agents can't run out of memory.\n\
             night_from = {}\n\
             night_until = {}\n\
             night_build_slots = {}\n\
             night_mem_gb = {}\n\
             night_min_free_gb = {}\n",
            self.night_from,
            self.night_until,
            self.night_build_slots,
            self.night_mem_gb,
            self.night_min_free_gb
        )
    }

    fn now_text(&self) -> String {
        format!(
            "# Small commands that skip the line (`fgate --now`): how many at once, and each one's memory ceiling\n\
             # in GB. One that needs more is stopped by the ceiling and belongs in the line.\n\
             now_slots = {}\n\
             now_mem_gb = {}\n",
            self.now_slots, self.now_mem_gb
        )
    }

    fn switch_text(&self) -> String {
        format!(
            "# Checkouts sharing one cargo target directory (CARGO_TARGET_DIR): before a build, clean this workspace's\n\
             # own packages there when another checkout built that profile last, so cargo never reuses its crates\n\
             # (1 = on, 0 = off):\n\
             switch_clean = {}\n",
            self.switch_clean
        )
    }

    fn keep_text(&self) -> String {
        format!(
            "# After a build into a shared target directory, copy this workspace's executables into the checkout,\n\
             # target/fgate-bin/<profile>/, where the next checkout's clean can't delete them (1 = on, 0 = off):\n\
             keep_bins = {}\n",
            self.keep_bins
        )
    }

    fn write_default(&self, file: &Path) {
        let text = format!(
            "# fgate settings for this machine (https://github.com/alexander-frase/fgate). Every agent reads this file,\n\
             # so all count the same slots.\n\
             # Builds and test runs at once:\n\
             build_slots = {}\n\
             # GPU runs at once (games, renderers, screenshot scripts):\n\
             gpu_slots = {}\n\
             # Compiler jobs for each gated cargo command (CARGO_BUILD_JOBS) unless it sets its own:\n\
             jobs = {}\n\
             # Each gated command's hard CPU ceiling, percent of all threads (0 = none):\n\
             cpu_percent = {}\n\
             # Each gated command's memory ceiling in GB, everything it starts together (0 = none):\n\
             mem_gb = {}\n\
             # A second command waits while less than this many GB are free:\n\
             min_free_gb = {}\n{}{}{}{}",
            self.build_slots,
            self.gpu_slots,
            self.jobs,
            self.cpu_percent,
            self.mem_gb,
            self.min_free_gb,
            self.night_text(),
            self.now_text(),
            self.switch_text(),
            self.keep_text()
        );
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(file, text);
    }
}

/// Says something about the settings once per run: `watch` and waiting commands read them again and again, and
/// `watch` would otherwise write over its own screen.
fn warn(text: &str) {
    static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    if let Ok(mut said) = SAID.lock() {
        if !said.iter().any(|s| s == text) {
            eprintln!("fgate: {text}");
            said.push(text.to_string());
        }
    }
}

/// Whether `hour` falls in the night `from`..`until`, which may wrap past midnight.
fn is_night(hour: u32, from: u32, until: u32) -> bool {
    if from == until {
        false
    } else if from < until {
        (from..until).contains(&hour)
    } else {
        hour >= from || hour < until
    }
}

#[cfg(test)]
mod tests {
    use super::is_night;

    #[test]
    fn night_hours_wrap_past_midnight() {
        assert!(is_night(23, 23, 7) && is_night(0, 23, 7) && is_night(6, 23, 7));
        assert!(!is_night(7, 23, 7) && !is_night(22, 23, 7) && !is_night(12, 23, 7));
        assert!(is_night(12, 0, 24) && !is_night(5, 5, 5));
    }
}
