# token-burn の開発タスク。引数なしの `make` でターゲット一覧を表示する。
#
# ツールの版は mise.toml で固定する。mise があればすべて `mise exec --` 経由で実行し、
# IDE や GUI から起動してシェルで mise が有効でなくても固定版を使う。
# SYSTEM_TOOLS=1 は PATH 上のツールを使う（版は保証しない）。
#
# macOS 標準の GNU Make 3.81 で使える機能に限る。
# .ONESHELL、.SHELLFLAGS、$(file ...)、!= は使わない。

.DEFAULT_GOAL := help

BINARY_NAME := token-burn
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock をコミットするため、CI と同じ依存関係に固定して解決する。
CARGO_FLAGS ?= --locked

# ---- ツールチェーン --------------------------------------------------------------
# まず PATH、次に通常のインストール先から mise を探す。GUI 起動の make は
# シェルの PATH を継承しない場合がある。make MISE=/path/to/mise で上書きできる。
# mise が無い場合の動作確認では MISE_CANDIDATES= で候補を空にする。
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise was not found. Install it from https://mise.jdx.dev, or add SYSTEM_TOOLS=1 to use the tools on PATH)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: help setup build release run test lint fmt fmt-check check ci install uninstall clean

## 準備

setup: ## Install the toolchain (mise) and dependencies
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## ビルド

build: ## Build a debug binary
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## Build a release binary
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## Run the debug binary (arguments via ARGS="...")
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

## 検査

test: ## Run the tests
	$(RUN) cargo test $(CARGO_FLAGS)

lint: ## Run clippy with warnings as errors
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings

fmt: ## Format the code (rewrites files)
	$(RUN) cargo fmt --all

fmt-check: ## Check the formatting (no changes)
	$(RUN) cargo fmt --all -- --check

check: fmt-check lint ## Run fmt-check and lint (no changes)

ci: check test ## Run the same checks as CI (no changes)

## インストール

# 実行ファイルは直接上書きせず、同じディレクトリの一時ファイルから rename で置き換える。
# macOS はコード署名の検査結果を inode ごとにキャッシュする。実行中または直前に実行した
# バイナリを上書きすると、起動直後に SIGKILL（終了コード 137）で落ちる。
# リンカーが既に ad hoc 署名を付けるため、再署名は不要。固定識別子を付けても
# バージョンをまたいで権限が維持されるわけではない。
install: release ## Install the release binary to INSTALL_PATH (default /usr/local/bin)
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## Remove the binary from INSTALL_PATH
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## Remove build artifacts
	$(RUN) cargo clean

## ヘルプ

help: ## Show this help
	@echo "Development tasks for $(BINARY_NAME)"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Tool versions are pinned in mise.toml. Run make setup first."
	@echo "Release: GitHub Actions > Release > Run workflow"
