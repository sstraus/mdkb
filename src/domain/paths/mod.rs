//! Spelling a path for whoever has to read it.
//!
//! # The one idea
//!
//! A path is not one string. It is one *location* with several spellings, and
//! each consumer accepts only its own. mdkb hands paths to four consumers, and
//! all four disagree:
//!
//! | Consumer | Wants | Rejects |
//! |---|---|---|
//! | The OS filesystem API | any native form | — |
//! | The index (keys, globs, module addresses) | `src/store/db.rs` | `src\store\db.rs` |
//! | SQLite `ATTACH` / `open`, and MCP resources | `file:///C:/dir/db` | `C:\dir\db` |
//! | The `git` CLI | `C:\dir` | `\\?\C:\dir` |
//!
//! # Why this only broke on Windows
//!
//! On Linux and macOS the four spellings **coincide**. Both are POSIX: one
//! root, `/` separators, no drive letters, no verbatim prefix. So
//! `path.to_string_lossy()` is already an index key, already a URI path, and
//! already what git wants. The conversion is the identity function, and code
//! that never converts is indistinguishable from code that converts correctly.
//!
//! On Windows they diverge, all four at once:
//!
//! - separators are `\`, so an index key does not match a `/` glob;
//! - a path starts at a drive letter, so `file:` + path is not a URI;
//! - `canonicalize()` returns the verbatim `\\?\C:\…` form, which the OS
//!   accepts and external tools do not.
//!
//! Every defect this module fixes has the same shape: **an implicit conversion
//! that happens to be the identity on the developer's platform.** That is what
//! made them invisible in review and invisible in CI — the tests were right,
//! the code was right, and the coincidence was doing the work.
//!
//! # What that means for the design
//!
//! One rule: **a path crossing a boundary is converted explicitly, by a named
//! function, at that boundary.** Never concatenated, never assumed.
//!
//! The consequence that matters more than the bug fixes: the flow above these
//! functions is now the SAME on every platform. There is no `cfg!(windows)` in
//! indexing, in salvage, in the MCP surface, or in the hook path — the platform
//! knowledge is confined to this module, and every layer above it reads one
//! way. A reader who understands the Linux flow understands the Windows flow,
//! because they are the same flow.
//!
//! # The layers
//!
//! Each submodule owns one boundary, and inside each the same three layers
//! repeat:
//!
//! - **Transformation** — pure, total, reversible where that makes sense.
//! - **Policy** — a decision expressed as a name ([`file_uri::read_only_uri`]),
//!   so callers state intent rather than assemble syntax.
//! - **Rule** — the small predicate a transformation turns on, testable alone.

pub mod file_uri;
pub mod index_key;
pub mod portable;
