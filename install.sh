#!/bin/sh
# install.sh — install, upgrade or remove heard-cli (`heard` + `heard-hook`).
#
#   curl -fsSL https://raw.githubusercontent.com/heardlabs/heard-rust-cli/main/install.sh | sh
#   ./install.sh                  # from a clone: builds from source
#
# POSIX sh, shellcheck-clean. It never touches anything outside its prefix
# (default ~/.local/bin) and heard-cli's state root
# (~/Library/Application Support/heard-cli, or $HEARD_CLI_HOME).
#
# Hidden knobs, for tests and packagers (not in --help):
#   HEARD_INSTALL_BASE_URL   where the release assets live (file:// or http://
#                            loopback); replaces the GitHub Releases URL.
#   HEARD_INSTALL_UNAME      pretend `uname -s` printed this.
#   --debug-build            with --from-source: a debug build, no fat LTO.

set -eu

REPO="heardlabs/heard-rust-cli"
ASSET="heard-macos-universal.tar.gz"
SUMS="SHA256SUMS"

usage() {
	cat <<'EOF'
Install heard, the voice for your coding agents (Claude Code, Codex CLI).

Usage: install.sh [options]

  --version <x.y.z>   install this release instead of the latest
  --prefix <dir>      install the binaries here (default: ~/.local/bin)
  --from-source       build with cargo instead of downloading a release
                      (the default when run from inside a clone)
  --download          download a release even when run from inside a clone
  --no-setup          do not run `heard setup` afterwards
  --uninstall         remove heard's hooks and binaries
  --purge             with --uninstall: also delete models, config and history
  -h, --help          show this help

Re-running the installer upgrades in place.
EOF
}

# Everything below runs inside main(), called on the last line, so a
# download cut short by `curl | sh` fails to parse instead of running a
# prefix of the script.
main() {

# ---------------------------------------------------------------------------
# Output

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
	BOLD=$(printf '\033[1m')
	DIM=$(printf '\033[2m')
	RED=$(printf '\033[31m')
	YELLOW=$(printf '\033[33m')
	GREEN=$(printf '\033[32m')
	RESET=$(printf '\033[0m')
else
	BOLD=""
	DIM=""
	RED=""
	YELLOW=""
	GREEN=""
	RESET=""
fi

say() { printf '%s\n' "$*"; }
step() { printf '%s==>%s %s\n' "$BOLD" "$RESET" "$*"; }
warn() { printf '%swarning:%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
die() {
	printf '%serror:%s %s\n' "$RED" "$RESET" "$*" >&2
	exit 1
}

# ---------------------------------------------------------------------------
# Arguments

VERSION=""
PREFIX=""
FROM_SOURCE=0
DOWNLOAD=0
NO_SETUP=0
UNINSTALL=0
PURGE=0
DEBUG_BUILD=0

while [ $# -gt 0 ]; do
	case "$1" in
	--version)
		[ $# -ge 2 ] || die "--version needs a value, e.g. --version 0.2.0"
		VERSION=${2#v}
		shift 2
		;;
	--version=*)
		VERSION=${1#--version=}
		VERSION=${VERSION#v}
		shift
		;;
	--prefix)
		[ $# -ge 2 ] || die "--prefix needs a directory"
		PREFIX=$2
		shift 2
		;;
	--prefix=*)
		PREFIX=${1#--prefix=}
		shift
		;;
	--from-source)
		FROM_SOURCE=1
		shift
		;;
	--download)
		DOWNLOAD=1
		shift
		;;
	--no-setup)
		NO_SETUP=1
		shift
		;;
	--uninstall)
		UNINSTALL=1
		shift
		;;
	--purge)
		PURGE=1
		shift
		;;
	--debug-build)
		DEBUG_BUILD=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		usage >&2
		printf '\n' >&2
		die "unknown option: $1"
		;;
	esac
done

[ "$PURGE" -eq 0 ] || [ "$UNINSTALL" -eq 1 ] || die "--purge only goes with --uninstall"
[ "$FROM_SOURCE" -eq 0 ] || [ "$DOWNLOAD" -eq 0 ] || die "--from-source and --download are opposites; pick one"

[ -n "${HOME:-}" ] || die "HOME is not set"
[ -n "$PREFIX" ] || PREFIX="$HOME/.local/bin"
case "$PREFIX" in
/*) ;;
*) PREFIX="$(pwd)/$PREFIX" ;;
esac
PREFIX=${PREFIX%/}
[ -n "$PREFIX" ] || die "refusing to install into /"

STATE_ROOT=${HEARD_CLI_HOME:-"$HOME/Library/Application Support/heard-cli"}

# Pretty-print a path with ~ for the home directory.
TILDE='~'
tildify() {
	case "$1" in
	"$HOME"/*) printf '%s/%s' "$TILDE" "${1#"$HOME"/}" ;;
	*) printf '%s' "$1" ;;
	esac
}

# ---------------------------------------------------------------------------
# Platform

check_platform() {
	os=${HEARD_INSTALL_UNAME:-$(uname -s)}
	case "$os" in
	Darwin) ;;
	Linux)
		die "heard-cli runs on macOS only for now. Linux support is coming; follow https://github.com/$REPO for news."
		;;
	*)
		die "heard-cli runs on macOS (13 or later); this system is $os."
		;;
	esac
	if [ -z "${HEARD_INSTALL_UNAME:-}" ] && command -v sw_vers >/dev/null 2>&1; then
		macos=$(sw_vers -productVersion 2>/dev/null || echo 0)
		major=${macos%%.*}
		case "$major" in
		'' | *[!0-9]*) major=0 ;;
		esac
		[ "$major" -ge 13 ] || die "heard-cli needs macOS 13 or later; this Mac runs $macos."
	fi
	ARCH=$(uname -m)
	case "$ARCH" in
	arm64 | aarch64) ARCH=arm64 ;;
	x86_64 | amd64) ARCH=x86_64 ;;
	*) die "unsupported CPU architecture: $ARCH (heard-cli supports arm64 and x86_64)" ;;
	esac
}

# ---------------------------------------------------------------------------
# Privileges: sudo only when the prefix cannot be written as this user.

SUDO=""

# The nearest existing ancestor of $1.
existing_ancestor() {
	d=$1
	while [ ! -d "$d" ]; do
		d=$(dirname "$d")
	done
	printf '%s' "$d"
}

pick_sudo() {
	anc=$(existing_ancestor "$PREFIX")
	if [ -w "$anc" ] && { [ ! -d "$PREFIX" ] || [ -w "$PREFIX" ]; }; then
		SUDO=""
		return
	fi
	command -v sudo >/dev/null 2>&1 || die "$PREFIX is not writable and sudo is not available; choose another --prefix"
	warn "$PREFIX is not writable by $(id -un); using sudo for the copy"
	SUDO="sudo"
}

run_priv() {
	if [ -n "$SUDO" ]; then
		sudo "$@"
	else
		"$@"
	fi
}

# ---------------------------------------------------------------------------
# Download + verify

TMP=""
cleanup() {
	if [ -n "$TMP" ] && [ -d "$TMP" ]; then
		rm -rf "$TMP"
	fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

make_tmp() {
	[ -n "$TMP" ] || TMP=$(mktemp -d "${TMPDIR:-/tmp}/heard-install.XXXXXX")
}

fetch() {
	# fetch <url> <dest>; fails quietly (the caller explains).
	command -v curl >/dev/null 2>&1 || die "curl is required to download heard"
	curl -fsSL --proto '=https,file,http' --retry 2 -o "$2" "$1" 2>/dev/null
}

sha256_of() {
	if command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | awk '{print $1}'
	elif command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		die "neither shasum nor sha256sum is available to verify the download"
	fi
}

release_base() {
	if [ -n "${HEARD_INSTALL_BASE_URL:-}" ]; then
		printf '%s' "${HEARD_INSTALL_BASE_URL%/}"
	elif [ -n "$VERSION" ]; then
		printf 'https://github.com/%s/releases/download/v%s' "$REPO" "$VERSION"
	else
		printf 'https://github.com/%s/releases/latest/download' "$REPO"
	fi
}

# Sets SRC_DIR to a directory holding heard + heard-hook. Returns 1 when no
# release could be downloaded (so the caller can fall back to source).
download_release() {
	make_tmp
	base=$(release_base)
	label=${VERSION:-latest}
	step "Downloading heard ($label) for macOS $ARCH"
	if ! fetch "$base/$SUMS" "$TMP/$SUMS"; then
		return 1
	fi
	fetch "$base/$ASSET" "$TMP/$ASSET" || die "found $SUMS but could not download $ASSET from $base"
	want=$(awk -v f="$ASSET" '$2 == f || $2 == "*" f {print $1}' "$TMP/$SUMS" | head -n 1)
	[ -n "$want" ] || die "$SUMS has no entry for $ASSET; the release looks incomplete"
	have=$(sha256_of "$TMP/$ASSET")
	if [ "$want" != "$have" ]; then
		die "checksum mismatch for $ASSET (expected $want, got $have). Nothing was installed; try again, and report it if it persists."
	fi
	say "   ${DIM}sha256 ok${RESET}"
	mkdir -p "$TMP/x"
	tar -xzf "$TMP/$ASSET" -C "$TMP/x" || die "could not unpack $ASSET"
	SRC_DIR=""
	for d in "$TMP/x" "$TMP/x"/*; do
		if [ -f "$d/heard" ] && [ -f "$d/heard-hook" ]; then
			SRC_DIR=$d
			break
		fi
	done
	[ -n "$SRC_DIR" ] || die "$ASSET does not contain heard and heard-hook"
	return 0
}

# ---------------------------------------------------------------------------
# Build from source

CLONE_ROOT=""

detect_clone() {
	case "$0" in
	*/install.sh | install.sh) ;;
	*) return 1 ;;
	esac
	here=$(cd "$(dirname "$0")" 2>/dev/null && pwd) || return 1
	if [ -f "$here/Cargo.toml" ] && [ -d "$here/crates/heard-hook" ]; then
		CLONE_ROOT=$here
		return 0
	fi
	return 1
}

offer_rustup() {
	say ""
	say "Building heard needs Rust (cargo), which is not installed."
	say "Install it with rustup (https://rustup.rs):"
	say ""
	say "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
	say ""
	if [ -t 0 ] && [ -r /dev/tty ]; then
		printf 'Run that now? [y/N] '
		read -r answer </dev/tty || answer=""
		case "$answer" in
		y | Y | yes | YES)
			curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path ||
				die "rustup failed"
			PATH="$HOME/.cargo/bin:$PATH"
			export PATH
			return 0
			;;
		esac
	fi
	die "install Rust, then run this installer again"
}

build_from_source() {
	if [ -z "$CLONE_ROOT" ]; then
		detect_clone || die "--from-source must be run from inside a heard-rust-cli clone (git clone https://github.com/$REPO)"
	fi
	if ! command -v cargo >/dev/null 2>&1; then
		if [ -x "$HOME/.cargo/bin/cargo" ]; then
			PATH="$HOME/.cargo/bin:$PATH"
			export PATH
		else
			offer_rustup
		fi
	fi
	if [ "$DEBUG_BUILD" -eq 1 ]; then
		profile=debug
		set --
	else
		profile=release
		set -- --release
	fi
	step "Building heard from source ($profile) in $(tildify "$CLONE_ROOT")"
	(cd "$CLONE_ROOT" && cargo build "$@" -p heard-cli -p heard-hook) ||
		die "cargo build failed; see the output above"
	target=${CARGO_TARGET_DIR:-"$CLONE_ROOT/target"}
	SRC_DIR="$target/$profile"
	[ -x "$SRC_DIR/heard" ] && [ -x "$SRC_DIR/heard-hook" ] ||
		die "the build finished but $SRC_DIR has no heard/heard-hook"
}

# ---------------------------------------------------------------------------
# Install

# Copy to a temp name in the prefix, then rename over the old file: a new
# inode, so a running daemon keeps its old mapping and macOS never sees a
# signed binary rewritten in place.
place() {
	src=$1
	name=$2
	run_priv cp "$src" "$PREFIX/.$name.new.$$"
	run_priv chmod 0755 "$PREFIX/.$name.new.$$"
	run_priv mv -f "$PREFIX/.$name.new.$$" "$PREFIX/$name"
}

path_advice() {
	case ":${PATH:-}:" in
	*":$PREFIX:"*) return 0 ;;
	esac
	shown=$(tildify "$PREFIX")
	case "$PREFIX" in
	"$HOME"/*) shown_env="\$HOME/${PREFIX#"$HOME"/}" ;;
	*) shown_env=$PREFIX ;;
	esac
	say ""
	warn "$shown is not on your PATH. Add it with:"
	case "${SHELL:-}" in
	*/zsh) say "    echo 'export PATH=\"$shown_env:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
	*/bash) say "    echo 'export PATH=\"$shown_env:\$PATH\"' >> ~/.bash_profile && exec bash" ;;
	*/fish) say "    fish_add_path $shown_env" ;;
	*) say "    export PATH=\"$shown_env:\$PATH\"   # add this line to your shell's startup file" ;;
	esac
	return 1
}

shadow_check() {
	found=$(command -v heard 2>/dev/null || true)
	if [ -n "$found" ] && [ "$found" != "$PREFIX/heard" ]; then
		warn "another heard at $found comes first on your PATH (an older or Python install?). Remove it, or put $(tildify "$PREFIX") ahead of it."
	fi
}

do_install() {
	check_platform
	upgrading=0
	[ -x "$PREFIX/heard" ] && upgrading=1
	old_version=""
	if [ "$upgrading" -eq 1 ]; then
		old_version=$("$PREFIX/heard" --version 2>/dev/null | head -n 1 || true)
	fi

	use_source=$FROM_SOURCE
	if [ "$use_source" -eq 0 ] && [ "$DOWNLOAD" -eq 0 ] && [ -z "$VERSION" ] &&
		[ -z "${HEARD_INSTALL_BASE_URL:-}" ] && detect_clone; then
		use_source=1
	fi
	if [ "$use_source" -eq 1 ]; then
		build_from_source
	elif ! download_release; then
		if detect_clone; then
			warn "no release found at $(release_base); building from source instead"
			build_from_source
		else
			die "could not download heard from $(release_base). Check your connection, pass --version, or build from a clone with ./install.sh --from-source"
		fi
	fi

	pick_sudo
	step "Installing to $(tildify "$PREFIX")"
	run_priv mkdir -p "$PREFIX"
	place "$SRC_DIR/heard-hook" heard-hook
	place "$SRC_DIR/heard" heard

	new_version=$("$PREFIX/heard" --version 2>/dev/null | head -n 1 || true)
	[ -n "$new_version" ] || new_version="heard"

	path_advice || true
	shadow_check

	say ""
	if [ "$upgrading" -eq 1 ]; then
		say "${GREEN}Upgraded${RESET} ${old_version:-heard} -> ${BOLD}$new_version${RESET} in $(tildify "$PREFIX")"
		say "   ${DIM}run \`heard restart\` if the daemon is running, so it picks up the new version${RESET}"
	else
		say "${GREEN}Installed${RESET} ${BOLD}$new_version${RESET} in $(tildify "$PREFIX") (heard, heard-hook)"
	fi

	if [ "$NO_SETUP" -eq 1 ] || [ ! -t 0 ]; then
		if [ "$upgrading" -eq 0 ]; then
			say ""
			say "Next: run ${BOLD}heard setup${RESET} to download the voice model and hook up Claude Code / Codex."
		fi
	else
		say ""
		"$PREFIX/heard" setup || warn "heard setup did not finish; run \`heard setup\` again any time"
	fi
}

# ---------------------------------------------------------------------------
# Uninstall

do_uninstall() {
	step "Uninstalling heard from $(tildify "$PREFIX")"
	if [ -x "$PREFIX/heard" ]; then
		"$PREFIX/heard" stop >/dev/null 2>&1 || true
		if ! "$PREFIX/heard" uninstall all; then
			warn "\`heard uninstall all\` failed; your agents' settings may still name heard-hook. Run \`heard uninstall all\` from a working install, or remove the hooks by hand."
		fi
	else
		warn "no heard in $(tildify "$PREFIX"); skipping hook removal (use the same --prefix you installed with)"
	fi
	pick_sudo
	removed=0
	for name in heard heard-hook; do
		if [ -e "$PREFIX/$name" ] || [ -L "$PREFIX/$name" ]; then
			run_priv rm -f "$PREFIX/$name"
			removed=$((removed + 1))
		fi
	done
	if [ "$PURGE" -eq 1 ]; then
		case "$STATE_ROOT" in
		"" | "/" | "$HOME" | "$HOME/")
			die "refusing to delete $STATE_ROOT"
			;;
		esac
		if [ -d "$STATE_ROOT" ]; then
			rm -rf "$STATE_ROOT"
			say "   removed $(tildify "$STATE_ROOT") (models, config, history)"
		fi
	fi
	say ""
	if [ "$removed" -gt 0 ]; then
		say "${GREEN}Removed${RESET} heard and heard-hook from $(tildify "$PREFIX")."
	else
		say "Nothing to remove in $(tildify "$PREFIX")."
	fi
	if [ "$PURGE" -eq 0 ] && [ -d "$STATE_ROOT" ]; then
		say "   ${DIM}models and settings are still in $(tildify "$STATE_ROOT"); re-run with --uninstall --purge to delete them${RESET}"
	fi
}

if [ "$UNINSTALL" -eq 1 ]; then
	do_uninstall
else
	do_install
fi
}

main "$@"
