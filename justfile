set shell := ["bash", "-uc"]

default:
    @just --list

# --- development ---------------------------------------------------------

build:
    cargo build --workspace --release --locked

# Indexing is CPU-bound over ~63k files; a debug build takes minutes longer than
# the release build does, so `run` uses release even for one-off queries.
run *ARGS:
    cargo run -p cellar --release -- {{ARGS}}

dev *ARGS:
    cargo run -p cellar -- {{ARGS}}

test *ARGS:
    cargo test --workspace --locked {{ARGS}}

clippy:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

ci: fmt-check clippy test check-skill

clean:
    cargo clean

# --- common tasks --------------------------------------------------------

# Fetch the current WhatsApp Web revision and index it.
add-latest:
    cargo run -p cellar --release -- bundle add --rev latest

# Diff the two newest stored WhatsApp bundles through the default filter.
latest-diff *ARGS:
    cargo run -p cellar --release -- diff {{ARGS}}

# --- installation --------------------------------------------------------
#
# The agent integrations point at the installed binary rather than at
# `target/release/cellar`, so they keep working from any directory and survive
# this checkout moving.

# Put `cellar` on PATH at ~/.cargo/bin/cellar.
install:
    cargo install --path crates/cellar --locked

# Install the skill into an agent's global skills directory.
#
# A copy, not a symlink: the installed skill has to keep working if this
# checkout is moved or removed. Re-run after editing the skill.
_install-skill DEST:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p "{{DEST}}"
    cp -L .claude/skills/cellar/SKILL.md "{{DEST}}/SKILL.md"
    echo "installed {{DEST}}/SKILL.md"

install-claude-mcp: install
    claude mcp add --scope user cellar -- "$HOME/.cargo/bin/cellar" mcp

install-codex-mcp: install
    codex mcp add cellar -- "$HOME/.cargo/bin/cellar" mcp

install-claude-skill:
    @just _install-skill "$HOME/.claude/skills/cellar"

install-codex-skill:
    @just _install-skill "${CODEX_HOME:-$HOME/.codex}/skills/cellar"

install-claude: install-claude-mcp install-claude-skill

install-codex: install-codex-mcp install-codex-skill

install-agents: install-claude install-codex

# Check that the checked-in Codex skill still resolves to the Claude one. They
# are one file behind a symlink; this catches someone replacing it with a copy
# that then silently drifts.
check-skill:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -L .codex/skills/cellar ]; then
      echo ".codex/skills/cellar must be a symlink to ../../.claude/skills/cellar" >&2
      exit 1
    fi
    diff -q .claude/skills/cellar/SKILL.md .codex/skills/cellar/SKILL.md
    echo "skill is a single source of truth"
