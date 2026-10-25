# Build scripts for iPhone Photo Sync. Requires GNU Make (`winget install ezwinports.make`).
# Recipes only call cargo and powershell so they behave the same whether make
# runs them through cmd.exe or a POSIX sh (e.g. from Git Bash).
#
#   make            debug build
#   make run        build and launch the debug app
#   make release    optimized build
#   make dist       release build copied to dist/iphone-photo-sync.exe
#   make test       unit tests
#   make fmt        format the code
#   make lint       clippy, warnings are errors
#   make check      fmt-check + lint + test (run before committing)
#   make clean      remove target/ and dist/
#
# Releases: set the version in Cargo.toml and push to the `release` branch;
# .github/workflows/release.yml builds iphone-photo-sync.exe and creates a draft GitHub Release.
# Installed copies update themselves once the draft is published.

BIN  := iphone-photo-sync.exe
DIST := dist

.DEFAULT_GOAL := build
.PHONY: build run release dist test fmt fmt-check lint check clean

build:
	cargo build

run:
	cargo run

release:
	cargo build --release

dist: release
	powershell -NoProfile -Command "New-Item -ItemType Directory -Force $(DIST) | Out-Null; Copy-Item -Force target/release/$(BIN) $(DIST)/"

test:
	cargo test

fmt:
	cargo fmt

fmt-check:
	cargo fmt --check

lint:
	cargo clippy --all-targets -- -D warnings

check: fmt-check lint test

clean:
	cargo clean
	powershell -NoProfile -Command "Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $(DIST)"
