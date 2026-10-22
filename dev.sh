#!/usr/bin/env bash
# Development commands and local MCP setup for this checkout.
set -euo pipefail

SONAR_ROOT="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly SONAR_ROOT

usage() {
    cat <<'EOF'
Usage:
  ./dev.sh <command> [args]

Commands:
  help                 Show these commands (also the default with no arguments).
  doctor               Check the Rust tools, Python, and optional integrations.
  install              Build and install a release executable for your user.
  reinstall            Rebuild and atomically replace the installed executable.
  uninstall            Remove the executable; keep indexes, models, and settings.
  develop              Fetch the development dependencies from Cargo.lock.
  run [args]           Build and run this checkout; forward arguments to Sonar.
  check                Check Rust formatting and Clippy without rewriting files.
  fmt                  Format the Rust code.
  test [args]          Run Cargo tests with these arguments, then installer tests.
  build                Build a release executable in target/release/.
  setup <repo> [opts]   Index a repository and register its MCP connection in Codex.
  refresh <repo> [opts] Update an index without changing Codex settings.

Builds and checks run in this checkout. run keeps your current working directory.
Requires Rust and Cargo; test also requires Python 3.8+.
install and reinstall still accept --prod; both always build a release executable.

Setup / refresh options:
  --name NAME       MCP name (default: sonar-<repository-directory-name>).
  --db PATH         Database (default: <Sonar checkout>/.sonar/mcp/<name>.sqlite).
  --resolve         Also prepare Rust calls/references using rust-analyzer.
  --typescript PATH Also prepare TypeScript calls/references with this compiler module.
  --model PATH      Also embed with this local model; setup enables hybrid MCP.
  --resources MODE  Include 'none' (default) or 'yaml' optional resources.
  --replace         Setup only: replace an existing named Codex MCP entry.

Repeat the same preparation options with refresh, including --model/--resolve/--typescript
and --resources when used. Options are not saved as a separate refresh profile.
Relative repository, model, and --db paths are relative to your working directory.

Environment:
  SONAR_DEV_INSTALL_DIR  Binary directory (default: $CARGO_HOME/bin or ~/.cargo/bin).
  CARGO_TARGET_DIR       Build directory (default: <Sonar checkout>/target).
                        Both overrides must be absolute paths.
  SONAR_PYTHON           Python executable for installer tests (default: python3/python).

Example:
  ./dev.sh doctor
  ./dev.sh install
  ./dev.sh run --help
  ./dev.sh test --lib
  ./dev.sh setup /path/to/repo --resolve
  ./dev.sh refresh /path/to/repo --resolve

Setup requires Codex CLI; --resolve requires rust-analyzer and project dependencies.
--typescript requires Node.js, a TypeScript 5.x/6.x compiler module, and project dependencies.
Models must already exist locally. Restart Codex after setup or reinstall.
Preparation reports are saved beside the database as <database>.<step>.json.
EOF
}

fail() { printf 'sonar dev: %s\n' "$*" >&2; exit 1; }
usage_error() { printf 'sonar dev: %s\n' "$*" >&2; exit 2; }

require_commands() {
    local executable
    for executable in "$@"; do
        command -v "$executable" >/dev/null 2>&1 || fail "required command is unavailable: $executable"
    done
}

bounded_absolute_path() {
    [[ "$2" == /* && "$2" != / && "$2" != *$'\n'* ]] ||
        fail "$1 must be an absolute path other than /, without newlines"
}

build_directory() {
    SONAR_TARGET_DIRECTORY="${CARGO_TARGET_DIR:-$SONAR_ROOT/target}"
    bounded_absolute_path CARGO_TARGET_DIR "$SONAR_TARGET_DIRECTORY"
}

cargo_project() {
    (cd -- "$SONAR_ROOT" && cargo "$@")
}

build_executable() {
    local profile="$1"
    require_commands cargo
    build_directory
    local -a arguments=(build --locked --bin sonar --target-dir "$SONAR_TARGET_DIRECTORY")
    if [[ "$profile" == release ]]; then arguments+=(--release); fi
    cargo_project "${arguments[@]}"
}

find_python() {
    local candidate
    local -a candidates=(python3 python)
    if [[ -n "${SONAR_PYTHON:-}" ]]; then candidates=("$SONAR_PYTHON"); fi
    for candidate in "${candidates[@]}"; do
        if command -v "$candidate" >/dev/null 2>&1 &&
            "$candidate" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' >/dev/null 2>&1; then
            SONAR_PYTHON_COMMAND="$(command -v "$candidate")"
            if [[ "$SONAR_PYTHON_COMMAND" != /* ]]; then
                SONAR_PYTHON_COMMAND="$PWD/$SONAR_PYTHON_COMMAND"
            fi
            return 0
        fi
    done
    return 1
}

doctor() {
    local executable location status=0
    printf 'Checkout: %s\n' "$SONAR_ROOT"
    for executable in Cargo.toml Cargo.lock; do
        if [[ -f "$SONAR_ROOT/$executable" ]]; then
            printf '%s: found\n' "$executable"
        else
            printf '%s: MISSING\n' "$executable"
            status=1
        fi
    done
    for executable in cargo rustc; do
        if location="$(command -v "$executable")"; then
            printf '%s: %s\n' "$executable" "$location"
            (cd -- "$SONAR_ROOT" && "$executable" --version) || status=1
        else
            printf '%s: MISSING (install the Rust toolchain)\n' "$executable"
            status=1
        fi
    done
    if command -v cargo >/dev/null 2>&1; then
        for executable in fmt clippy; do
            if ! cargo_project "$executable" --version; then
                printf '%s: unavailable (install the rustfmt and clippy components)\n' "$executable"
                status=1
            fi
        done
    fi
    if find_python; then
        printf 'Python for installer tests: %s\n' "$(command -v "$SONAR_PYTHON_COMMAND")"
    else
        printf '%s\n' 'Python 3.8+: MISSING (needed for installer tests; set SONAR_PYTHON)'
        status=1
    fi
    for executable in codex rust-analyzer node; do
        printf '%s (optional): %s\n' "$executable" "$(command -v "$executable" || printf 'not found')"
    done
    printf 'sonar on PATH: %s\n' "$(command -v sonar || printf 'not installed or not on PATH')"
    return "$status"
}

check_project() {
    local status=0 result
    require_commands cargo
    build_directory
    cargo_project fmt --all -- --check || status=$?
    cargo_project clippy --all-targets --locked --target-dir "$SONAR_TARGET_DIRECTORY" -- -D warnings || {
        result=$?
        if ((status == 0)); then status=$result; fi
    }
    return "$status"
}

test_project() {
    local status=0 result
    require_commands cargo
    build_directory
    find_python || fail 'Python 3.8+ is required for installer tests; install it or set SONAR_PYTHON'
    cargo_project test --locked --target-dir "$SONAR_TARGET_DIRECTORY" "$@" || status=$?
    (cd -- "$SONAR_ROOT" && "$SONAR_PYTHON_COMMAND" -B -m unittest discover -s scripts -p 'test_*.py') || {
        result=$?
        if ((status == 0)); then status=$result; fi
    }
    return "$status"
}

install_directory() {
    local cargo_root="${CARGO_HOME:-${HOME:?HOME is required}/.cargo}"
    INSTALL_DIRECTORY="${SONAR_DEV_INSTALL_DIR:-$cargo_root/bin}"
    bounded_absolute_path SONAR_DEV_INSTALL_DIR "$INSTALL_DIRECTORY"
    [[ ! -L "$INSTALL_DIRECTORY" ]] || fail "install directory is a symbolic link: $INSTALL_DIRECTORY"
    [[ ! -e "$INSTALL_DIRECTORY" || -d "$INSTALL_DIRECTORY" ]] || fail "not a directory: $INSTALL_DIRECTORY"
    SONAR_BINARY="$INSTALL_DIRECTORY/sonar"
}

regular_destination() {
    [[ ! -L "$SONAR_BINARY" ]] || fail "refusing a symbolic-link executable: $SONAR_BINARY"
    [[ ! -e "$SONAR_BINARY" || -f "$SONAR_BINARY" ]] || fail "not a regular file: $SONAR_BINARY"
}

verify_binary() {
    local version
    [[ -f "$1" && ! -L "$1" && -x "$1" ]] || fail "missing regular executable: $1"
    version="$("$1" --version)" || fail "executable version check failed: $1"
    [[ "$version" == 'sonar '* ]] || fail "unexpected executable identity: $version"
}

install_product() {
    local replace="$1"
    require_commands cargo install mktemp mv rm rmdir
    regular_destination
    if [[ -e "$SONAR_BINARY" ]]; then
        [[ "$replace" == true ]] || fail "$SONAR_BINARY already exists; use './dev.sh reinstall'"
        verify_binary "$SONAR_BINARY"
    fi
    build_executable release
    verify_binary "$SONAR_TARGET_DIRECTORY/release/sonar"
    mkdir -p -- "$INSTALL_DIRECTORY"
    (
        staging="$(mktemp -d "$INSTALL_DIRECTORY/.sonar-dev-install.XXXXXX")"
        trap 'rm -f -- "$staging/sonar"; rmdir -- "$staging"' EXIT
        install -m 0755 -- "$SONAR_TARGET_DIRECTORY/release/sonar" "$staging/sonar"
        verify_binary "$staging/sonar"
        regular_destination
        if [[ "$replace" != true && -e "$SONAR_BINARY" ]]; then
            fail "$SONAR_BINARY appeared during the build; installation aborted"
        fi
        # Rename within one filesystem; running MCP processes keep their old inode.
        mv -f -- "$staging/sonar" "$SONAR_BINARY"
    )
    printf 'Installed Sonar: %s\n' "$SONAR_BINARY"
    printf '%s\n' 'Restart existing Codex sessions to use this build.'
}

uninstall_product() {
    regular_destination
    if [[ -e "$SONAR_BINARY" ]]; then
        verify_binary "$SONAR_BINARY"
        rm -- "$SONAR_BINARY"
        printf 'Removed %s\n' "$SONAR_BINARY"
    else
        printf 'Sonar is not installed at %s\n' "$SONAR_BINARY"
    fi
    printf '%s\n' 'Indexes, models, and Codex settings were preserved.' \
        'Remove an unused connection separately with: codex mcp remove <name>'
}

prepare_step() {
    local db="$1" step="$2"
    shift 2
    printf 'Running sonar %s (report: %s.%s.json)\n' "$step" "$db" "$step"
    "$SONAR_BINARY" --db "$db" "$step" "$@" > "$db.$step.json"
}

prepare_repository() {
    local action="$1" repository name='' db='' model='' typescript='' resolve=false resources=none replace=false
    shift
    (($# > 0)) || fail "$action requires a repository path; see --help"
    repository="$1"
    shift
    while (($#)); do
        case "$1" in
            --name|--db|--model|--resources|--typescript)
                (($# >= 2)) && [[ -n "$2" && "$2" != --* ]] || fail "$1 requires a value"
                case "$1" in
                    --name) name="$2" ;;
                    --db) db="$2" ;;
                    --model) model="$2" ;;
                    --resources) resources="$2" ;;
                    --typescript) typescript="$2" ;;
                esac
                shift 2
                ;;
            --resolve) resolve=true; shift ;;
            --replace) replace=true; shift ;;
            *) fail "unknown $action option: $1" ;;
        esac
    done
    [[ "$action" == setup || "$replace" == false ]] || fail '--replace only applies to setup'
    [[ "$resources" == none || "$resources" == yaml ]] || fail '--resources must be none or yaml'
    [[ -d "$repository" ]] || fail "repository directory does not exist: $repository"
    repository="$(CDPATH= cd -- "$repository" && pwd -P)"
    if [[ -z "$name" ]]; then
        name="sonar-$(printf '%s' "${repository##*/}" | LC_ALL=C tr -c 'a-zA-Z0-9_-' '-')"
    fi
    [[ "$name" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_-]*$ ]] || fail '--name must start with a letter, digit, or underscore and contain only letters, digits, underscores, or hyphens'
    db="${db:-$SONAR_ROOT/.sonar/mcp/$name.sqlite}"
    [[ "$db" == /* ]] || db="$PWD/$db"
    bounded_absolute_path --db "$db"
    [[ ! -L "$db" && ( ! -e "$db" || -f "$db" ) ]] || fail "database must be a regular file path: $db"
    if [[ -n "$model" ]]; then
        [[ -d "$model" ]] || fail "local model directory does not exist: $model"
        model="$(CDPATH= cd -- "$model" && pwd -P)"
    fi
    verify_binary "$SONAR_BINARY"
    if [[ "$resolve" == true ]]; then
        require_commands rust-analyzer cargo rustc
    fi
    if [[ -n "$typescript" ]]; then
        require_commands node
        [[ -e "$typescript" ]] || fail "TypeScript compiler module does not exist: $typescript"
        [[ "$typescript" == /* ]] || typescript="$PWD/$typescript"
    fi
    if [[ "$action" == setup ]]; then
        require_commands codex
        # Validate Codex configuration before preparation; do not conflate a bad
        # config with an absent server or silently overwrite an unrelated entry.
        codex mcp list >/dev/null
        if [[ "$replace" == false ]] && codex mcp get "$name" >/dev/null 2>&1; then
            fail "Codex entry '$name' already exists; use refresh, another --name, or setup --replace"
        fi
    fi
    mkdir -p -- "$(dirname -- "$db")"
    db="$(CDPATH= cd -- "$(dirname -- "$db")" && pwd -P)/$(basename -- "$db")"
    printf 'Preparing %s\nDatabase: %s\n' "$repository" "$db"
    prepare_step "$db" index "$repository" --resources "$resources"
    prepare_step "$db" index-navigation
    if [[ "$resolve" == true ]]; then
        prepare_step "$db" resolve
    fi
    if [[ -n "$typescript" ]]; then
        printf 'Running TypeScript resolution (report: %s.typescript-resolve.json)\n' "$db"
        "$SONAR_BINARY" --db "$db" resolve --typescript "$typescript" > "$db.typescript-resolve.json"
    fi
    if [[ -n "$model" ]]; then
        prepare_step "$db" embed --model "$model" --representation separate
    fi
    if [[ "$action" == setup ]]; then
        local -a server_command=("$SONAR_BINARY" --db "$db" mcp)
        if [[ -n "$model" ]]; then
            server_command+=(--model "$model")
        fi
        # Register only after all requested preparation succeeds.
        codex mcp add "$name" -- "${server_command[@]}"
        printf 'Configured Codex MCP: %s\n' "$name"
        printf '%s\n' 'Start a new Codex session in the repository and check /mcp.'
    else
        printf '%s\n' 'Index refreshed. Existing MCP processes observe the new index on their next call.'
    fi
}

action="${1-help}"
if (($# > 0)); then shift; fi
# run/test own their arguments, including --help; other commands show helper help.
if [[ "$action" != run && "$action" != test ]] &&
    (($# == 1)) && [[ "$1" == --help || "$1" == -h ]]; then
    case "$action" in
        help|doctor|install|reinstall|uninstall|develop|check|fmt|build|setup|refresh) usage; exit 0 ;;
    esac
fi
case "$action" in
    help|--help|-h) (($# == 0)) || usage_error 'help does not accept arguments'; usage ;;
    doctor) (($# == 0)) || usage_error 'usage: ./dev.sh doctor'; doctor ;;
    install|reinstall)
        (($# == 0)) || { (($# == 1)) && [[ "$1" == --prod ]]; } ||
            usage_error "usage: ./dev.sh $action [--prod]"
        install_directory
        if [[ "$action" == reinstall ]]; then install_product true; else install_product false; fi
        ;;
    uninstall)
        (($# == 0)) || usage_error 'usage: ./dev.sh uninstall'
        install_directory
        uninstall_product
        ;;
    develop)
        (($# == 0)) || usage_error 'usage: ./dev.sh develop'
        require_commands cargo
        cargo_project fetch --locked
        ;;
    run)
        build_executable debug
        "$SONAR_TARGET_DIRECTORY/debug/sonar" "$@"
        ;;
    check) (($# == 0)) || usage_error 'usage: ./dev.sh check'; check_project ;;
    fmt)
        (($# == 0)) || usage_error 'usage: ./dev.sh fmt'
        require_commands cargo
        cargo_project fmt --all
        ;;
    test) test_project "$@" ;;
    build)
        (($# == 0)) || usage_error 'usage: ./dev.sh build'
        build_executable release
        printf 'Built Sonar: %s/release/sonar\n' "$SONAR_TARGET_DIRECTORY"
        ;;
    setup|refresh)
        install_directory
        prepare_repository "$action" "$@"
        ;;
    *) usage_error "unknown command: $action; see --help" ;;
esac
