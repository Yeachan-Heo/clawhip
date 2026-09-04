#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INVOCATION_CWD="$(pwd -P)"
GITHUB_REPO="Yeachan-Heo/clawhip"
INSTALLER_URL="${CLAWHIP_INSTALLER_URL:-https://github.com/${GITHUB_REPO}/releases/latest/download/clawhip-installer.sh}"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export CARGO_HOME
SYSTEMD=0
SKIP_STAR_PROMPT=0

usage() {
  cat <<'EOF'
Usage: ./install.sh [--systemd] [--skip-star-prompt]

Options:
  --systemd            Install and start the bundled systemd service.
  --skip-star-prompt   Disable the optional post-install GitHub star prompt.
  -h, --help           Show this help text.

Environment:
  CLAWHIP_SKIP_STAR_PROMPT=1
      Disable the optional post-install GitHub star prompt.
EOF
}

parse_args() {
  for arg in "$@"; do
    case "$arg" in
      --systemd) SYSTEMD=1 ;;
      --skip-star-prompt) SKIP_STAR_PROMPT=1 ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        echo "unknown arg: $arg" >&2
        usage >&2
        exit 1
        ;;
    esac
  done
}

log() {
  echo "[clawhip] $*"
}

is_truthy() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

star_prompt_disabled() {
  is_truthy "${CLAWHIP_SKIP_STAR_PROMPT:-}" || is_truthy "${SKIP_STAR_PROMPT:-}"
}

selected_config_path() {
  local config_path="${CLAWHIP_CONFIG:-}"
  if [[ -z "$config_path" ]]; then
    config_path="$HOME/.clawhip/config.toml"
  elif [[ "$config_path" != /* ]]; then
    config_path="$INVOCATION_CWD/$config_path"
  fi
  printf '%s\n' "$config_path"
}

validate_source_checkout() {
  if [[ -L "$REPO_ROOT/Cargo.toml" || ! -f "$REPO_ROOT/Cargo.toml" ]]; then
    log "source checkout Cargo.toml is missing or is a symlink"
    return 1
  fi
  if [[ -L "$REPO_ROOT/src" || ! -d "$REPO_ROOT/src" ]]; then
    log "source checkout src marker is missing or is a symlink"
    return 1
  fi

  local git_root
  git_root="$(
    env -u GIT_DIR -u GIT_WORK_TREE -u GIT_COMMON_DIR -u GIT_INDEX_FILE \
      -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES \
      -u GIT_CONFIG -u GIT_CONFIG_GLOBAL -u GIT_CONFIG_SYSTEM \
      -u GIT_CONFIG_PARAMETERS -u GIT_CONFIG_COUNT -u GIT_EXEC_PATH \
      -u GIT_SSH -u GIT_SSH_COMMAND -u GIT_ASKPASS -u GIT_PROXY_COMMAND \
      git -C "$REPO_ROOT" rev-parse --show-toplevel
  )" || {
    log "source checkout is not a git worktree"
    return 1
  }
  if [[ "$git_root" != "$REPO_ROOT" ]]; then
    log "source checkout is not the git worktree root"
    return 1
  fi

  local package_id
  package_id="$(cargo pkgid --manifest-path "$REPO_ROOT/Cargo.toml")" || {
    log "unable to inspect source checkout Cargo package"
    return 1
  }
  case "$package_id" in
    *'#clawhip@'*) ;;
    *)
      log "source checkout Cargo package is not clawhip"
      return 1
      ;;
  esac
}

is_interactive_install() {
  [[ -t 0 && -t 1 ]]
}

can_use_github_cli_for_star() {
  command -v gh >/dev/null 2>&1 && gh auth status &>/dev/null
}

star_repo_with_gh() {
  gh api --method PUT "/user/starred/${GITHUB_REPO}" --silent &>/dev/null
}

prompt_to_star_repo() {
  local response
  printf '[clawhip] Would you like to star %s on GitHub with gh? [y/N]: ' "$GITHUB_REPO"
  read -r response || return 0

  case "$response" in
    [yY]|[yY][eE][sS])
      if star_repo_with_gh; then
        log "thanks for starring ${GITHUB_REPO}"
      else
        log "unable to star ${GITHUB_REPO} with gh; continuing without it"
      fi
      ;;
    *)
      log "skipping GitHub star step"
      ;;
  esac
}

maybe_prompt_to_star_repo() {
  if star_prompt_disabled; then
    log "skipping GitHub star prompt (--skip-star-prompt or CLAWHIP_SKIP_STAR_PROMPT)"
    return 0
  fi

  if ! is_interactive_install; then
    return 0
  fi

  if ! can_use_github_cli_for_star; then
    return 0
  fi

  log "optional: star ${GITHUB_REPO} on GitHub to support the project"
  prompt_to_star_repo
}

install_prebuilt_binary() {
  if ! command -v curl >/dev/null 2>&1; then
    log "curl is not installed; skipping prebuilt binary download"
    return 1
  fi

  mkdir -p "$CARGO_HOME/bin"

  log "attempting prebuilt binary install from ${INSTALLER_URL}"

  local installer
  installer="$(mktemp)"

  if ! curl --proto '=https' --tlsv1.2 -LsSf "$INSTALLER_URL" -o "$installer"; then
    log "no downloadable release installer found; falling back to cargo install"
    rm -f "$installer"
    return 1
  fi

  if sh "$installer"; then
    rm -f "$installer"
    return 0
  else
    local status=$?
    log "prebuilt installer failed with status ${status}; falling back to cargo install"
    rm -f "$installer"
    return 1
  fi
}

install_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    cat >&2 <<'MSG'
[clawhip] A prebuilt binary was not available and Cargo is not installed.
[clawhip] Install Rust with rustup, then rerun this installer:
[clawhip]   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
[clawhip]   source "$HOME/.cargo/env"
MSG
    exit 1
  fi

  validate_source_checkout
  log "building from source with cargo install --path . --force"
  cd "$REPO_ROOT"
  cargo install --path . --force

  record_source_checkout
}

record_source_checkout() {
  local binary_path
  local config_path
  binary_path="$(installed_binary_path)" || {
    log "unable to find installed clawhip binary for source-checkout persistence"
    return 1
  }
  log "recording validated source checkout"
  config_path="$(selected_config_path)"
  (
    cd "$REPO_ROOT"
    "$binary_path" --config "$config_path" install --record-source-checkout-only
  )
}

sync_plugins() {
  local source_dir="$REPO_ROOT/plugins"
  local target_dir
  target_dir="$(dirname "$(selected_config_path)")/plugins"

  if [[ ! -d "$source_dir" ]]; then
    return 0
  fi

  rm -rf "$target_dir"
  mkdir -p "$(dirname "$target_dir")"
  cp -R "$source_dir" "$target_dir"
  log "synced plugins to $target_dir"
}

installed_binary_path() {
  if [[ -x "$CARGO_HOME/bin/clawhip" ]]; then
    printf '%s\n' "$CARGO_HOME/bin/clawhip"
    return 0
  fi

  if command -v clawhip >/dev/null 2>&1; then
    command -v clawhip
    return 0
  fi

  return 1
}

setup_quick_start() {
  local binary_path
  binary_path="$(installed_binary_path)" || return 0

  local config_path
  config_path="$(selected_config_path)"
  if [[ -f "$config_path" ]]; then
    log "existing config found at $config_path; skipping quick-start scaffold"
    return 0
  fi

  local webhook_url="${CLAWHIP_WEBHOOK_URL:-}"
  if [[ -z "${webhook_url// }" && -t 0 ]]; then
    printf '[clawhip] Discord webhook URL (recommended quick start; press Enter to skip): '
    read -r webhook_url || true
  fi

  if [[ -n "${webhook_url// }" ]]; then
    log "scaffolding webhook quick-start config"
    "$binary_path" --config "$config_path" setup --webhook "$webhook_url"
    log "webhook config scaffolded at $config_path"
  else
    log "recommended quick start: clawhip setup --webhook 'https://discord.com/api/webhooks/...'"
    log "bot-token mode is still supported via ~/.clawhip/config.toml"
  fi
}

install_systemd_binary() {
  local binary_path
  binary_path="$(installed_binary_path)" || {
    log "unable to find installed clawhip binary for systemd setup"
    exit 1
  }

  log "installing $binary_path to /usr/local/bin/clawhip for systemd"
  sudo install -m 755 "$binary_path" /usr/local/bin/clawhip
}

main() {
  parse_args "$@"

  log "install flow: prebuilt binary -> cargo fallback -> SKILL attach -> config scaffold -> optional post-install GitHub star prompt -> verification"
  log "repo root: $REPO_ROOT"

  if install_prebuilt_binary; then
    log "prebuilt binary installed successfully"
  else
    install_from_source
  fi

  local config_path
  config_path="$(selected_config_path)"
  mkdir -p "$(dirname "$config_path")"
  log "ensured config dir $(dirname "$config_path")"
  sync_plugins
  log "next: read SKILL.md and attach the skill surface"
  setup_quick_start

  if [[ "$SYSTEMD" == "1" ]]; then
    install_systemd_binary
    sudo cp deploy/clawhip.service /etc/systemd/system/clawhip.service
    sudo systemctl daemon-reload
    sudo systemctl enable --now clawhip
    log "systemd unit installed and started"
  fi

  maybe_prompt_to_star_repo

  log "recommended verification: scripts/live-verify-default-presets.sh <mode>"
  log "install complete"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
