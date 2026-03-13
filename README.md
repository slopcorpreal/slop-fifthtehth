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

## Self-update

```bash
# Check for updates (no install)
tachyon --check-update

# Install update (prompts for minor/major updates unless --yes is used)
tachyon --self-update
```

- Installations in `~/.cargo/bin` update via `cargo install --force tachyon`.
- Standalone binaries update by downloading and replacing the platform-specific release artifact.
- Minor version updates are treated as feature-level updates and prompt before applying.
- Major version updates are treated as potentially breaking updates and always prompt before applying.

## Controls

- `q`: Quit
- `f` or `/`: Enter filter mode
- `Enter`: Inspect selected line as pretty JSON
- `g` / `G`: Jump top/bottom
- `↑` / `↓`: Navigate rows

## Release versioning

The release workflow computes versions as:

- `major`: existing Cargo.toml major
- `minor`: git commit count (`git rev-list --count HEAD`)
- `patch`: auto-incremented from existing `v{major}.{minor}.*` tags (or reuses an existing tag on `HEAD`)
