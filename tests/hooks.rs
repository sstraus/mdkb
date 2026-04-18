//! Integration tests for `mdkb hook` lifecycle dispatch.
//!
//! Bundles submodules under `tests/hooks/` so cargo's top-level test discovery
//! can pick them up. Each submodule file holds its own `#[test]` functions.

#[path = "hooks/dispatch_test.rs"]
mod dispatch_test;

#[path = "hooks/session_start_test.rs"]
mod session_start_test;

#[path = "hooks/user_prompt_submit_test.rs"]
mod user_prompt_submit_test;
