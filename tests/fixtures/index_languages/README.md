# Recorded source fixtures

Recorded on 2026-10-24 from the main McpMacControl checkout:

- `notify.m`: `internal/notify/notify_darwin.m`, from the first line through the
  complete `VirtualCursorView` implementation before `showVirtualCursor`.
- `pasteboard.go`: complete `internal/input/pasteboard_darwin.go`.

These are parsing inputs only. Tests do not compile or run Cocoa code.
