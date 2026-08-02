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
