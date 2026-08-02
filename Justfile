[private]
default:
    @just --list

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

fmt:
    @cargo fmt

# Everything CI runs.
check:
    @cargo test --workspace
    @cargo clippy --workspace --all-targets -- -D warnings
    @cargo fmt --check

clean:
    @cargo clean
