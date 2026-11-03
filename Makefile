.PHONY: check check-backend check-frontend fmt fmt-check test build backend-dev web-dev

check: check-backend check-frontend

check-backend: fmt-check
	python3 scripts/check-licenses.py
	cargo clippy --manifest-path backend/Cargo.toml --workspace --all-targets -- -D warnings
	cargo test --manifest-path backend/Cargo.toml --workspace

check-frontend:
	python3 scripts/frontend-version.py --check
	pnpm --dir frontend/packages/routing-client test
	pnpm --dir frontend/packages/maplibre build
	pnpm --dir frontend/packages/maplibre test
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

REGISTRIES ?=
PUBLISH_INTERVAL ?= 60
MAX_RATE_LIMIT_RETRIES ?= 3
RELEASE_PUBLISH_ARGS = --publish-interval $(PUBLISH_INTERVAL) --max-rate-limit-retries $(MAX_RATE_LIMIT_RETRIES)
RELEASE_REGISTRY_ARGS = $(foreach registry,$(REGISTRIES),--registry $(registry))

.PHONY: release-check release-dry-run release-publish release-test release-target-check
release-target-check:
	@test -n "$(strip $(REGISTRIES))" || { echo '请指定目标仓库：REGISTRIES=crates-io（公共）或 REGISTRIES=internal（私有）' >&2; exit 2; }
release-check: release-target-check
	@set -e; for registry in $(REGISTRIES); do python3 scripts/release.py check --registry "$$registry"; done
release-dry-run: release-target-check
	python3 scripts/release.py release $(RELEASE_REGISTRY_ARGS) $(RELEASE_PUBLISH_ARGS)
release-publish: release-target-check
	python3 scripts/release.py release $(RELEASE_REGISTRY_ARGS) $(RELEASE_PUBLISH_ARGS) --execute
release-test:
	python3 -m unittest discover -s scripts/tests -v

FRONTEND_RELEASE_ARGS ?=
FRONTEND_VERSION ?=

.PHONY: frontend-version frontend-version-sync frontend-version-check
frontend-version:
	@test -n "$(FRONTEND_VERSION)" || { echo '请指定 FRONTEND_VERSION，例如 0.1.0-pre.2' >&2; exit 2; }
	python3 scripts/frontend-version.py --set-version "$(FRONTEND_VERSION)"

frontend-version-sync:
	python3 scripts/frontend-version.py --sync

frontend-version-check:
	python3 scripts/frontend-version.py --check

.PHONY: release-frontend-dry-run release-frontend-publish

release-frontend-dry-run:
	python3 scripts/frontend-release.py $(FRONTEND_RELEASE_ARGS)

release-frontend-publish:
	python3 scripts/frontend-release.py $(FRONTEND_RELEASE_ARGS) --execute
