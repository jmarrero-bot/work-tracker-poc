.PHONY: build check demo
build:
	cd web && npm ci && npm run build
	cargo build
check:
	cd web && npm ci && npm run build
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test
	python3 tests/smoke.py target/debug/workboard
	python3 tests/hub_smoke.py target/debug/workboard
demo: build
	./target/debug/workboard --data-dir /tmp/workboard-demo serve --demo
