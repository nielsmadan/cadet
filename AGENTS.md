# Cadet
Cadet is a Rust workspace containing the `cadet` CLI and its core, backend, storage, and app crates.

## Commands
| Command | Description |
|---------|-------------|
| `just setup` | Prepare dependencies and hooks, then run doctor. |
| `just doctor` | Verify required tools and Git hooks. |
| `just run <args>` | Run the `cadet` CLI without installing it. |
| `just check` | Run the CI test, clippy, and formatting checks. |

## Gotchas
- Cadet resolves its registry home in order: `$CADET_HOME`, `$XDG_CONFIG_HOME/cadet`, then `~/.config/cadet`.
