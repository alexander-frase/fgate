<!--
  An example section for your project's AGENTS.md (or CLAUDE.md), so coding agents use fgate without being told
  each time. Copy the section below into it and adjust the marked parts:
  - the program names after `fgate --gpu`, or delete that part if nothing of yours needs the GPU;
  - the shared target folder, or delete that bullet if each checkout builds on its own.
  The matching hook settings are in examples/settings.json.
-->

## Building and testing: the build gate

Several agents share this machine. Builds must never run it out of memory, so heavy commands take turns through
[fgate](https://github.com/alexander-frase/fgate).

- **Put `fgate` in front of every cargo command that compiles or runs something**: `build`, `check`, `test`,
  `clippy`, `run`, `doc`, `bench`, `install`. For example `fgate cargo test -p my_crate`. fgate waits for a free
  slot, prints your place in line and who is running, then runs the command unchanged. Its output and exit code are
  the command's own.
- **Programs that need the GPU** (here: `my_game`, `my_viewer`; edit this list) run with `fgate --gpu`:
  `fgate --gpu target/debug/my_game --screenshot shot.png`. A script that runs cargo or the game runs under
  fgate as a whole: `fgate --gpu python take_screenshots.py`.
- **Small commands skip the line with `fgate --now`**: `cargo fmt`, `cargo publish`, a quick tool run. They get a
  2 GB memory ceiling; if fgate says a `--now` command was too big to skip the line, run it again without `--now`.
- **Waiting is normal.** When `fgate status` shows a line, start the command in the background and wait for it to
  finish. Don't poll it, don't cancel and retry, and don't work around the gate (another tool, `FGATE_OFF`, or a
  command split up to avoid it).
- **Out of memory?** If fgate ends with "it reached the … GB ceiling", the failure came from the gate's memory
  ceiling, not from the code. Tell the owner instead of changing the code to use less memory.
- **Shared build folder** (edit or delete): build into the shared target so dependencies aren't compiled again in
  every checkout: `CARGO_TARGET_DIR=/path/to/main/checkout/target fgate cargo test`.
- `fgate status` shows the slots and the line; `fgate watch` shows them live.
