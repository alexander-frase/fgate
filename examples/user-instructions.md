<!--
  An example section for instructions that apply to every project on the machine: ~/.claude/CLAUDE.md for
  Claude Code, ~/.copilot/copilot-instructions.md for GitHub Copilot. Drop the commands you don't use.
  For a single project's AGENTS.md, examples/AGENTS.md has more detail on cargo.
-->

## Building and testing: the build gate

Several agents share this machine, so heavy commands take turns through
[fgate](https://github.com/alexander-frase/fgate). This applies in every project.

- **Put `fgate` in front of every command that compiles or runs tests**: `fgate cargo test`,
  `fgate cmake --build build`, `fgate ctest --test-dir build`, `fgate ninja -C build`,
  `fgate conan build . --build=missing`, `fgate uv run conan build ..`. fgate waits for a free slot, prints your place
  in line, then runs the command unchanged; output and exit code are the command's own. Configuring
  (`cmake -S . -B build`) needs no gate.
- **A script that builds runs under fgate as a whole**: `fgate python build.py`. Commands it starts don't queue again.
- **Small commands skip the line with `fgate --now`** (2 GB memory ceiling). If fgate says it was too big, run it
  again without `--now`.
- **Waiting is normal.** When `fgate status` shows a line, start the command in the background and wait for it.
  Don't poll, cancel and retry, or work around the gate (`FGATE_OFF`, another tool, splitting the command).
- **Out of memory?** If fgate ends with "it reached the … GB ceiling", the gate's ceiling stopped it, not the code.
  Tell the user instead of changing the code to use less memory.
