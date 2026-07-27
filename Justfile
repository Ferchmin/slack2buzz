# slack2buzz task runner.
#
# Hermit pins the toolchain: run `. ./bin/activate-hermit` once per shell, or
# prefix commands with `./bin/`. Every recipe here assumes cargo is on PATH.

_default:
    @just --list

# Everything CI runs, in the order CI runs it.
ci: fmt-check lint test

# Format the workspace.
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Clippy with warnings as errors, tests and all targets included.
lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all-features

# Regenerate the golden IR files, then READ THE DIFF before committing.
update-golden:
    UPDATE_GOLDEN=1 cargo test --test golden
    @echo
    @echo "Golden files rewritten. Review before committing:"
    @echo "  git diff tests/golden"

# Inspect the bundled synthetic export.
probe-fixture:
    cargo run -- probe fixtures/basic-export

# Parse the bundled synthetic export to stdout.
parse-fixture:
    cargo run -- parse fixtures/basic-export --all -o -

# Build with the same profile CI publishes.
build:
    cargo build --release
