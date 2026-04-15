# Thoth — dogfooding Makefile.
#
# Wires common cargo flows + a self-indexing demo so you can try Thoth on its
# own source tree without remembering the CLI surface.
#
#   make help            # list targets
#   make demo            # full happy path: build → init → index → sample queries
#   make mcp             # run the MCP stdio server against .thoth
#
# Everything that Thoth writes goes to $(THOTH_ROOT) (default .thoth/) so
# it stays out of git and clippy's sight.

SHELL            := /usr/bin/env bash
.SHELLFLAGS      := -eu -o pipefail -c
.DEFAULT_GOAL    := help

# --- configuration ---------------------------------------------------------

CARGO            ?= cargo
PROFILE          ?= release
CARGO_PROFILE    := $(if $(filter release,$(PROFILE)),--release,)
TARGET_DIR       ?= target
BIN_DIR          := $(TARGET_DIR)/$(PROFILE)

THOTH            := $(BIN_DIR)/thoth
THOTH_MCP        := $(BIN_DIR)/thoth-mcp

# Where the self-index lives. Override with `make index THOTH_ROOT=...`.
THOTH_ROOT       ?= .thoth
# Source tree we point Thoth at. Defaults to this repo.
SRC              ?= .

export RUST_LOG  ?= thoth=info,tantivy=warn,warn

# --- phony targets ---------------------------------------------------------

.PHONY: help build release debug check clippy fmt fmt-check test test-mcp \
        clean clean-self init index query watch mcp demo doctor eval \
        memory-show memory-forget skills-list print-root

# --- help ------------------------------------------------------------------

help: ## Show this help (grep of ## comments)
	@awk 'BEGIN { FS = ":.*?## "; printf "\nUsage: make <target>\n\nTargets:\n" } \
	      /^[a-zA-Z0-9_-]+:.*?## / { printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2 }' \
	      $(MAKEFILE_LIST)
	@printf "\nConfig:\n  THOTH_ROOT=%s\n  PROFILE=%s\n  SRC=%s\n\n" \
	        "$(THOTH_ROOT)" "$(PROFILE)" "$(SRC)"

# --- build / lint / test ---------------------------------------------------

build: ## Build every workspace binary in the current profile
	$(CARGO) build --workspace $(CARGO_PROFILE)

release: ## Alias: PROFILE=release build
	$(MAKE) PROFILE=release build

debug: ## Alias: PROFILE=debug build
	$(MAKE) PROFILE=debug build

check: ## cargo check --all-targets
	$(CARGO) check --workspace --all-targets

clippy: ## cargo clippy with -D warnings (what CI runs)
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## cargo fmt (write)
	$(CARGO) fmt --all

fmt-check: ## cargo fmt --check (CI gate)
	$(CARGO) fmt --all -- --check

test: ## Run the full workspace test suite
	$(CARGO) test --workspace --all-targets

test-mcp: ## Run only the MCP integration tests
	$(CARGO) test -p thoth-mcp --test rpc

# --- dogfood workflow ------------------------------------------------------

init: $(THOTH) ## Initialise a fresh $(THOTH_ROOT) next to the repo
	@mkdir -p $(THOTH_ROOT)
	$(THOTH) --root $(THOTH_ROOT) init

index: $(THOTH) init ## Index this repo's source tree into $(THOTH_ROOT)
	$(THOTH) --root $(THOTH_ROOT) index $(SRC)

query: $(THOTH) ## Run a sample query — override with Q="..."
	@test -n "$${Q:-}" || { echo "Q=... is required, e.g. make query Q='hybrid recall'"; exit 1; }
	$(THOTH) --root $(THOTH_ROOT) query "$$Q"

watch: $(THOTH) ## Watch $(SRC) and reindex on change (ctrl-c to stop)
	$(THOTH) --root $(THOTH_ROOT) watch $(SRC)

mcp: $(THOTH_MCP) ## Run the MCP stdio server against $(THOTH_ROOT)
	THOTH_ROOT=$(THOTH_ROOT) $(THOTH_MCP)

memory-show: $(THOTH) ## Dump MEMORY.md + LESSONS.md
	$(THOTH) --root $(THOTH_ROOT) memory show

memory-forget: $(THOTH) ## Run the forgetting pass (TTL + capacity eviction)
	$(THOTH) --root $(THOTH_ROOT) memory forget

skills-list: $(THOTH) ## List installed skills under $(THOTH_ROOT)/skills/
	$(THOTH) --root $(THOTH_ROOT) skills list

eval: $(THOTH) index ## Run precision@k evaluation over eval/gold.toml
	$(THOTH) --root $(THOTH_ROOT) eval --gold eval/gold.toml -k 8

# The headline dogfood target: build, init, index, and fire a batch of
# real questions at the store so you can see recall in action.
demo: $(THOTH) init ## Full happy path: index this repo + run a few real queries
	@echo "── indexing $(SRC) → $(THOTH_ROOT) ────────────────────────────"
	@$(THOTH) --root $(THOTH_ROOT) index $(SRC)
	@echo
	@for q in \
	    "hybrid recall RRF fusion" \
	    "watch debounce reindex" \
	    "tantivy fts index writer" \
	    "store root open" \
	    "graph bfs callers callees" \
	    "MCP tool call json-rpc"; do \
	    echo "── Q: $$q ────────────────────────────"; \
	    $(THOTH) --root $(THOTH_ROOT) query -k 4 "$$q" || true; \
	    echo; \
	done

# --- housekeeping ----------------------------------------------------------

clean: ## cargo clean
	$(CARGO) clean

clean-self: ## Remove the self-index ($(THOTH_ROOT))
	rm -rf $(THOTH_ROOT)

doctor: ## Print versions of the toolchain pieces we lean on
	@$(CARGO) --version
	@rustc --version
	@printf "profile: %s\nbin dir: %s\nroot:    %s\n" "$(PROFILE)" "$(BIN_DIR)" "$(THOTH_ROOT)"

print-root: ## Print the resolved $(THOTH_ROOT)
	@echo "$(THOTH_ROOT)"

# --- binary rules ----------------------------------------------------------
# These build the CLI / MCP binaries on demand so `make demo` works from a
# clean checkout without a separate `make build`.

$(THOTH): $(shell find crates -name '*.rs' 2>/dev/null) Cargo.toml
	$(CARGO) build $(CARGO_PROFILE) -p thoth-cli

$(THOTH_MCP): $(shell find crates -name '*.rs' 2>/dev/null) Cargo.toml
	$(CARGO) build $(CARGO_PROFILE) -p thoth-mcp
