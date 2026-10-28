.PHONY: check check-backend check-frontend fmt fmt-check test build backend-dev web-dev

check: check-backend check-frontend

check-backend: fmt-check
	python3 scripts/check-licenses.py
	cargo clippy --manifest-path backend/Cargo.toml --workspace --all-targets -- -D warnings
	cargo test --manifest-path backend/Cargo.toml --workspace

check-frontend:
	pnpm --dir frontend typecheck
	pnpm --dir frontend build

fmt:
	cargo fmt --manifest-path backend/Cargo.toml --all

fmt-check:
	cargo fmt --manifest-path backend/Cargo.toml --all -- --check

test:
	cargo test --manifest-path backend/Cargo.toml --workspace

build:
	cargo build --manifest-path backend/Cargo.toml --workspace
	pnpm --dir frontend build

backend-dev:
	cargo run --manifest-path backend/Cargo.toml -p routeweft-daemon -- $(ARGS)

web-dev:
	pnpm --dir frontend dev

.PHONY: release-check release-dry-run release-publish release-test

release-check:
	python3 scripts/private-release.py check

release-dry-run:
	python3 scripts/private-release.py release

release-publish:
	python3 scripts/private-release.py release --execute

.PHONY: release-frontend-dry-run release-frontend-publish

release-frontend-dry-run:
	python3 scripts/frontend-release.py

release-frontend-publish:
	python3 scripts/frontend-release.py --execute

release-test:
	python3 -m unittest discover -s scripts/tests -v
