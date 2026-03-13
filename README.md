# Tachyon

A blazing fast, memory-mapped, lazy-rendering JSONL log analyzer.

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/tachyon /path/to/logs.jsonl
```

## Controls

- `q`: Quit
- `f` or `/`: Enter filter mode
- `Enter`: Inspect selected line as pretty JSON
- `g` / `G`: Jump top/bottom
- `↑` / `↓`: Navigate rows
