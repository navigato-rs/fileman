# FileMan — Comprehensive Test Plan

## Test Infrastructure

FileMan uses a **replay-based headless testing system**. Tests are RON files in
`tests/cases/` that specify a starting directory, a sequence of keyboard events,
and assertions on panel state, filesystem state, file contents, or screenshots.

Run all tests: `scripts/replay_runner.sh`
Run one test:  `cargo run --release -- --replay tests/cases/<name>.ron`

---

## Feature Coverage (12 test files)

| Test File | Features Covered |
|-----------|-----------------|
| `navigation.ron` | Cursor Up/Down, Home/End, PageUp/PageDown, Enter to open dir, Backspace to parent |
| `panels.ron` | Tab panel switching, Ctrl+U panel swap, Ctrl+Right cross-panel navigation |
| `selection.ron` | Insert to mark, Insert to unmark (toggle) |
| `selection_shift_range.ron` | Shift+Down extends the marked range while moving |
| `selection_shift_toggle.ron` | Shift+Up/Down toggles marks, unmarking on reverse |
| `delete_keeps_cursor.ron` | Cursor stays at the same position after deleting an entry |
| `help.ron` | F1 open help, Escape close help |
| `preview.ron` | F3 preview with snapshot comparison |
| `preview_escape.ron` | F3 preview open, Escape close, panel mode assertions |
| `edit.ron` | F4 editor with snapshot comparison |
| `file_ops.ron` | F5 copy, Shift+F6 rename, F8 delete, Shift+F4 new file, F6 move, F4+Ctrl+S editor save |
| `mkdir.ron` | F7 create directory |
| `search.ron` | Alt+F7 search by filename, Escape to return |
| `search_navigate.ron` | Shift+Alt+F7 search by content, result verification |
| `history.ron` | Alt+Left back, Alt+Right forward, Backspace parent |

## GUI Smoke Test

`tests/gui_smoke_test.sh` — launches the real GUI under Xvfb and exercises
keyboard interactions via xdotool to verify the app starts, renders, and stays
responsive.
