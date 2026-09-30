# Run `just` to list recipes. Extra args after a recipe name are passed through.

db := "data/career.db"

[private]
default:
    @just --list

# Build all crates
build:
    cargo build --workspace

# Run the crawler (e.g. `just crawl --seeds seeds.txt`)
crawl *args:
    cargo run -p career-crawler -- {{args}}

# Run the UI (e.g. `just ui --db data/career.db`)
ui *args:
    cargo run -p career-ui -- {{args}}

# Run tests; optional filter (e.g. `just test seeds::`)
test *filter:
    cargo test --workspace {{filter}}

# Run tests for one crate: core | llm | crawler | ui
test-crate crate *filter:
    cargo test -p career-{{crate}} {{filter}}

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

# Everything CI would run
check: fmt-check lint test

# Open the local database in the sqlite3 shell
sql:
    sqlite3 {{db}}

# Show the latest crawl events
events n="20":
    sqlite3 -header -column {{db}} "SELECT id, datetime(ts / 1000, 'unixepoch') AS time, kind, payload FROM events ORDER BY id DESC LIMIT {{n}}"

# Delete the local database (asks first)
[confirm("Delete the local database in data/?")]
reset-db:
    rm -f {{db}} {{db}}-wal {{db}}-shm

# Create config.toml from the example if it doesn't exist
init-config:
    @test -f config.toml && echo "config.toml already exists" || (cp config.example.toml config.toml && echo "created config.toml")
