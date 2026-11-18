# Line discipline scenarios

These files come unchanged from Fuchsia's Starnix, `src/starnix/lib/line_discipline/testing/` at
commit 86962cb40d56e1a2bc033ead5b4c89bb02c34ca2 (2026-11-06), under the BSD-style license in
`LICENSE`:

- `trace_generator.py` drives a PTY pair through a scenario and records what each step returns:
  data read from either end, `EAGAIN` and `EIO`, signals sent to a session attached to the
  replica, `FIONREAD`, poll masks, packet-mode headers.
- `scenarios/*.json` are the scenarios (canonical and raw input, echo flavours, erase and kill,
  `VLNEXT`, `EXTPROC`, output processing, flushes, packet mode, hangups, mode switches with
  pending input), and `scenarios_list.json` names them in order.

Starnix records the traces once on Linux and checks its own line discipline against them. Here
the differential case `tests/diff/line-discipline.sh` runs the generator on both sides and
compares the traces, so the reference is the native kernel that the difftest container runs on.
The case receives this directory through its `# difftest-files:` line.

To update, copy the files again from a newer Fuchsia commit and change the commit above.
