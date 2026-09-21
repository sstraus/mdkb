# An unreadable config.toml falls back whole, and says so

Date: 2026-09-21
Status: accepted

## Context

`Config::load_or_default` was `Self::load(path).unwrap_or_default()`. Any
error — a TOML syntax error, an unknown enum value, a wrong type — discarded
the entire file and returned `Config::default()`. Not the one bad field:
every setting the user had written. Nothing was logged and nothing was
returned, so a user could edit `config.toml`, run `mdkb update`, and watch a
program ignore all of it with no way to find out why.

Measured while adding `graph.relations`: serde correctly rejects
`relations = "sometimes"` and names the three values it accepts.
`load_or_default` threw that message away and yielded `auto`.

`Config` and all of its sections are `#[serde(default)]`, so a key *absent*
from the file already takes the value in the code. The problem is only the
error path.

## Decision

Keep the whole-file fallback. Surface the reason.

- A store must still open when its config does not parse. Making the error
  fatal would let one typo block every command, including the ones that read
  no config at all.
- Per-field recovery was rejected: serde aborts the whole deserialisation on
  the first bad value, so keeping the good fields means deserialising into a
  `toml::Table` and merging twenty sections by hand — and a TOML *syntax*
  error stays global regardless, so the machinery would not even cover the
  whole failure class.
- `Config::load_or_report` returns the defaults **and** the parser message.
  `load_or_default` keeps its infallible signature and logs through `tracing`.
- `mdkb update` and `mdkb stats` — the two commands a user runs to find out
  what the store is doing — print the message on stderr. stderr rather than a
  field on the result, so it appears under `--format json` too without
  changing a shape the TUICommander dashboard plugin parses.

## Consequence

One invalid value still costs the user every setting in the file. That is the
accepted cost; what is no longer accepted is losing the reason. The behaviour
is pinned by `one_invalid_value_loses_the_whole_file_and_the_user_is_told` in
`src/config.rs` and by
`smoke_an_unreadable_config_is_reported_by_update_and_stats` in
`tests/cli_smoke.rs`.
