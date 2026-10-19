# fgate

**The fair gate: a build gate for coding agents that share one machine.**

Run five or ten coding agents (Claude Code, or anything else with a shell) on one Windows PC and sooner or
later they all start `cargo build` at once. Memory fills up, the machine freezes, and builds die with "out of
memory". fgate makes them take turns. Each heavy command waits in a fair line for a free slot, then runs
unchanged inside a Windows job with a memory and CPU ceiling. `fgate watch` shows the whole thing live.

![fgate watch: one build and one GPU run in their slots, five commands waiting in line, and the recent runs](docs/fgate-watch.png)

```text
fgate cargo test -p my_crate        waits for a build slot, then runs the command unchanged
fgate --gpu my_game.exe --shots     waits for the GPU slot (games, renderers, screenshot scripts)
fgate --now cargo publish           skips the line: small commands, within a small memory ceiling
fgate status                        who holds the slots, who is waiting
fgate watch                         the same, live: memory, CPU, each job's usage, recent runs
fgate hook [options]                a Claude Code hook that refuses cargo commands without fgate
```

## Install

fgate runs on Windows 10/11 and needs Rust 1.87 or newer.

```sh
cargo install fgate --locked
```

That puts `fgate.exe` in `~/.cargo/bin`. One install serves every project and every agent on the machine.

## Using it

Put `fgate` in front of any command that compiles or runs something heavy:

```sh
fgate cargo build --release
CARGO_TARGET_DIR=E:/shared/target fgate cargo test --workspace
fgate --gpu python take_screenshots.py      # a script that runs the game is gated as a whole
```

A waiting command prints its place in line and who is running, and tells you again whenever that changes:

```text
fgate: waiting for a build slot: #2 in line; running: smelter-2: cargo build --release (1m40s)
fgate: build slot 0 after 2m05s in line — cargo test --workspace
...
fgate: done in 1m12s, peak memory 6.9 GB
```

### Small commands: `--now`

Waiting ten minutes behind three builds to run `cargo fmt`, a publish or a quick tool is silly. `fgate --now`
runs a small command at once, without waiting in line, but it can't be used to slip a build past the queue:

- It runs within a small memory ceiling, `now_mem_gb` (2 GB). A command that grows past it fails with "out of
  memory", and fgate says it is too big to skip the line and should run again without `--now`.
- It still waits while free memory is below `min_free_gb` and something else is running, like any second command.
- At most `now_slots` (4) run at once. `status` and `watch` list them under `now` while they run.

The command's own output and exit code pass through untouched. A gated command that itself runs `fgate …`
(a script calling cargo, say) doesn't queue a second time: it already holds the slot.

Agents name their place by their folder, or by their worktree for checkouts under `.claude/worktrees/<name>`,
so the status shows `spaceport-sp1: cargo build` rather than a process id.

## How it works

- **Kinds of slot.** Build slots for `cargo` and test runs, a GPU slot for games, renderers and screenshot
  scripts, so a screenshot run doesn't wait behind a build, and `now` slots for small commands that skip the line.
- **A fair line.** Waiters are served in the order they arrived. Places in line and slots are OS file locks
  under `%LOCALAPPDATA%\fgate`, which Windows drops when a process ends however it ends. A crashed or killed agent
  never leaves a slot taken.
- **Waiting for memory.** Memory is usually what runs out first, and a build's memory grows as it goes. A second
  command waits while less than `min_free_gb` is free. At night, every command waits for it, even alone.
- **A Windows job per command.** Once in, the command and everything it starts run in one job object:
  below-normal priority, a hard CPU ceiling, one memory ceiling for the whole process tree, and all of it killed
  together when fgate ends, so no compiler is left running on its own. If the job hits its memory ceiling, fgate
  says so, so an "out of memory" from the gate isn't mistaken for a bug in your code.
- **Compiler jobs.** A gated cargo command gets `CARGO_BUILD_JOBS` from the settings, unless it sets its own.
- **Night limits.** Between `night_from` and `night_until` (local time) stricter limits apply, for agents that
  keep working while nobody watches.
- **History.** Finished commands are logged to `%LOCALAPPDATA%\fgate\history.log` (last 300). `fgate watch`
  shows them with run time, peak memory, time waited and exit code.

## Settings

The settings live in `%LOCALAPPDATA%\fgate\config.txt`, one file per machine, so every agent counts the same slots.
fgate writes the file with defaults on first use. Edit it any time; each command reads it when it starts.

| Setting             | Default             | Meaning                                                                 |
| ------------------- | ------------------- | ----------------------------------------------------------------------- |
| `build_slots`       | 1                   | builds and test runs at once                                            |
| `gpu_slots`         | 1                   | GPU runs at once                                                        |
| `now_slots`         | 4                   | small `--now` commands at once                                          |
| `now_mem_gb`        | 2                   | each `--now` command's memory ceiling                                   |
| `jobs`              | 6                   | `CARGO_BUILD_JOBS` for a gated cargo command that doesn't set its own   |
| `cpu_percent`       | 40                  | each command's hard CPU ceiling, percent of all threads (0 = none)      |
| `mem_gb`            | 40% of RAM, min 8   | each command's memory ceiling, its whole process tree (0 = none)        |
| `min_free_gb`       | 6                   | a second command waits while less than this is free                     |
| `night_from`        | 23                  | night limits start at this hour (`night_from = night_until`: never)     |
| `night_until`       | 7                   | and end at this hour                                                    |
| `night_build_slots` | 1                   | build slots at night                                                    |
| `night_mem_gb`      | 10                  | memory ceiling at night                                                 |
| `night_min_free_gb` | 8                   | at night any command waits while less than this is free                 |

Any setting can be overridden for one command with an environment variable such as `FGATE_MEM_GB=16`.
`FGATE_DIR` points fgate at another folder, for trying it out without touching the real line.

**A starting point.** On a 16-thread, 32 GB machine running about ten agents, memory ran out long before the CPU
did. What worked: `build_slots = 1`, `jobs = 16`, `cpu_percent = 0`, `mem_gb = 12`, `min_free_gb = 6`. Guard
memory first. If a build hits its memory ceiling, lower `jobs` before adding slots.

## The Claude Code hook

Agents forget the prefix. `fgate hook` is a [PreToolUse hook](https://docs.claude.com/en/docs/claude-code/hooks)
that refuses a heavy cargo command (`build`, `check`, `test`, `clippy`, `run`, `doc`, `install`, `nextest`, …)
started without `fgate`, and tells the agent the command to use instead. The agent retries with the prefix. Quoted
text and heredocs don't count, so `git commit -m "cargo test passes"` goes through.

Add it to the project's `.claude/settings.json` (or to `~/.claude/settings.json` for every project;
[examples/settings.json](examples/settings.json) has a complete file):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|PowerShell",
        "hooks": [{ "type": "command", "command": "fgate hook", "timeout": 10 }]
      }
    ]
  }
}
```

A project can add its own rules as options:

| Option                    | Refuses                                                                       |
| ------------------------- | ----------------------------------------------------------------------------- |
| `--gpu name,name`         | these programs (exe names) when started without `fgate --gpu`                 |
| `--protect-branch <name>` | `git push` to this branch, such as `main` (repeatable)                        |
| `--protect-label <name>`  | `gh` commands that name this label, such as an approval label (repeatable)    |
| `--no-admin-merge`        | `gh pr merge --admin`, which skips required checks                            |
| `--note <text>`           | nothing: this text is added to every refusal, to point at the project's rules |

```json
"command": "fgate hook --gpu my_game,my_viewer --protect-branch main --no-admin-merge --note \"See AGENTS.md.\""
```

`FGATE_OFF=1` turns the hook off.

## Telling the agents

Agents use fgate when their instructions say so. Copy the section in [examples/AGENTS.md](examples/AGENTS.md)
into your project's `AGENTS.md` or `CLAUDE.md`, and the hook settings in
[examples/settings.json](examples/settings.json) into `.claude/settings.json`. The instructions mean agents get it
right the first time, and the hook catches the ones that forget.

## Updating fgate while agents use it

Windows won't overwrite a running `fgate.exe`. Rename it first, install, and delete the old one once nothing runs it:

```sh
mv ~/.cargo/bin/fgate.exe ~/.cargo/bin/fgate-old.exe
cargo install fgate --locked
```

## Platform

Windows only for now: the slots use Windows file locks and the ceilings use Windows job objects. On Linux and
macOS, `cargo install fgate` stops with a message saying so. Support for both is planned.

## License

[MIT](LICENSE)
