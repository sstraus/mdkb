//! Integration tests for `mdkb hook` lifecycle dispatch.
//!
//! Bundles submodules under `tests/hooks/` so cargo's top-level test discovery
//! can pick them up. Each submodule file holds its own `#[test]` functions.

// Unix-only: every test here dispatches `mdkb hook <event>`, which refuses
// off-Unix ("Hook commands require Unix domain sockets"), so no run can
// pass on other platforms.
#![cfg(unix)]

#[path = "hooks/dispatch_test.rs"]
mod dispatch_test;

#[path = "hooks/session_start_test.rs"]
mod session_start_test;

#[path = "hooks/user_prompt_submit_test.rs"]
mod user_prompt_submit_test;

#[path = "hooks/post_tool_use_test.rs"]
mod post_tool_use_test;

#[path = "hooks/pre_tool_use_test.rs"]
mod pre_tool_use_test;
