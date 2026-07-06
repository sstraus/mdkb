//! Integration tests for hook-log size rotation (story 047): oversized
//! hook-events.jsonl / hook-slow.jsonl are halved (newest kept) on next append.

use mdkb::mcp::dispatch::{HOOK_LOG_CAP_BYTES, append_hook_log};

#[test]
fn oversized_log_is_halved_keeping_newest_on_append() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hook-events.jsonl");

    // Write enough lines to exceed the 1 MiB cap. Each line is uniquely numbered
    // so we can verify which half survives.
    let line_len = 64usize;
    let n = (HOOK_LOG_CAP_BYTES as usize / line_len) + 500; // comfortably over cap
    let mut content = String::with_capacity(n * line_len);
    for i in 0..n {
        content.push_str(&format!("{i:0width$}\n", width = line_len - 1));
    }
    std::fs::write(&path, &content).unwrap();
    assert!(
        std::fs::metadata(&path).unwrap().len() > HOOK_LOG_CAP_BYTES,
        "precondition: file exceeds cap"
    );

    // The append triggers rotation.
    let marker = format!("{:0width$}\n", 9_999_999, width = line_len - 1);
    append_hook_log(&path, &marker);

    let after = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = after.lines().collect();

    // Roughly half the original lines survive (+ the new marker line).
    assert!(
        lines.len() <= n / 2 + 2,
        "log must be roughly halved: {} of {n}",
        lines.len()
    );
    // The NEWEST original line (n-1) survived; the OLDEST (0) was dropped.
    assert!(
        lines
            .iter()
            .any(|l| l.trim_start_matches('0') == (n - 1).to_string()),
        "newest pre-rotation line must be retained"
    );
    assert!(
        !lines.iter().any(|l| l.trim_start_matches('0').is_empty()),
        "oldest line (all zeros) must be dropped"
    );
    // The just-appended marker is present and last.
    assert_eq!(
        lines.last().unwrap().trim_start_matches('0'),
        "9999999",
        "the appended line is preserved at the tail"
    );
}

#[test]
fn small_log_is_not_rotated() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hook-slow.jsonl");
    append_hook_log(&path, "{\"a\":1}\n");
    append_hook_log(&path, "{\"a\":2}\n");
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        content, "{\"a\":1}\n{\"a\":2}\n",
        "under-cap logs append verbatim"
    );
}

#[test]
fn append_creates_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("hook-events.jsonl");
    append_hook_log(&path, "{\"first\":true}\n");
    assert!(path.exists());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\"first\":true}\n"
    );
}
