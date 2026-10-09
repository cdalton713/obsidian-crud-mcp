# List available commands.
default:
    @just --list

# Run end-to-end tests with server output.
e2e:
    cargo test --test e2e -- --nocapture

# Run unit tests.
unit:
    cargo test --lib --bins

# Run all tests.
test:
    cargo test

# Run Clippy for all targets.
lint:
    cargo clippy --all-targets

# Format Rust source files.
format:
    cargo fmt
