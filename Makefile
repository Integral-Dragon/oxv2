CRATES := ox-server ox-herder ox-runner ox-ctl ox-rt ox-cx-watcher

.PHONY: install uninstall build test fmt fmt-check clippy check clean

install:
	@for c in $(CRATES); do cargo install --path $$c --locked --target-dir target || exit 1; done

uninstall:
	@for c in $(CRATES); do cargo uninstall $$c || true; done

build:
	cargo build --workspace

test:
	cargo test --workspace

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

check: fmt-check clippy test

clean:
	cargo clean
