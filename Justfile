[private]
default:
    @just --list

# Prepare this checkout for work: dependencies, hooks, then verify.
setup:
    @cargo fetch
    @lefthook install
    @just doctor

# Verify the tools and checkout state this repo needs.
doctor:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    need() {
        if command -v "$1" >/dev/null 2>&1; then
            printf '  ok       %s\n' "$1"
        else
            printf '  MISSING  %-12s install: %s\n' "$1" "$2"; fail=1
        fi
    }
    need cargo "https://rustup.rs"
    need lefthook "brew install lefthook"
    if [ -f "$(git rev-parse --git-path hooks/pre-commit)" ]; then
        printf '  ok       git hooks\n'
    else
        printf '  MISSING  %-12s run: just setup\n' 'git hooks'; fail=1
    fi
    [ "$fail" -eq 0 ] && printf 'Everything in place.\n'
    exit $fail

# Build and put `cadet` on PATH at ~/.cargo/bin. Re-run to update from dev.
install:
    @# --force: cargo silently skips reinstalling when the version is unchanged.
    @cargo install --path crates/cli --locked --force
    @echo "Installed: $(which cadet)  ($(cadet --version))"

uninstall:
    @cargo uninstall cadet-cli

# Run the dev build without installing: `just run ls --all`
run *ARGS:
    @cargo run -q -p cadet-cli -- {{ARGS}}

# Open the registry in $EDITOR.
conf:
    @# Second copy of the resolution order in `Registry::home`; keep them in step.
    @mkdir -p "${CADET_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}/cadet}"
    @${EDITOR:-vi} "${CADET_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}/cadet}/config.toml"

test:
    @cargo test --workspace

lint:
    @cargo clippy --workspace --all-targets -- -D warnings

format:
    @cargo fmt

# Everything CI runs.
check:
    @cargo test --workspace
    @cargo clippy --workspace --all-targets -- -D warnings
    @cargo fmt --check

clean:
    @cargo clean
