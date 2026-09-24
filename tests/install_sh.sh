#!/bin/sh
# tests/install_sh.sh — exercise install.sh end to end in a throwaway HOME
# and prefix, against fake releases served from file:// URLs.
#
#   sh tests/install_sh.sh            # standalone
#   cargo test -p heard-install --test install_script   # the same, from cargo
#
# Nothing here touches the real HOME, ~/.local/bin, ~/.claude or any socket:
# HOME, the prefix and HEARD_CLI_HOME all point into a temp dir, and the
# `heard` / `heard-hook` in the fake releases are stub scripts that only log.

set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ORIG_PATH=$PATH
ORIG_CARGO_HOME=${CARGO_HOME:-"$HOME/.cargo"}
ORIG_RUSTUP_HOME=${RUSTUP_HOME:-"$HOME/.rustup"}
T=$(mktemp -d "${TMPDIR:-/tmp}/heard-install-test.XXXXXX")
trap 'rm -rf "$T"' EXIT

unset ANTHROPIC_API_KEY ELEVENLABS_API_KEY || true
# Every other credential-shaped variable (*_API_KEY, *_TOKEN).
for _v in $(env | sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' | grep -E '_(API_KEY|TOKEN)$' || true); do
	unset "$_v" || true
done
unset _v
unset HEARD_INSTALL_BASE_URL HEARD_INSTALL_UNAME || true

export HOME="$T/home"
export HEARD_CLI_HOME="$T/home/state"
export NO_COLOR=1
mkdir -p "$HOME"
PREFIX="$T/prefix/bin"
LOG="$T/stub.log"
export HEARD_STUB_LOG="$LOG"
BASE_PATH="/usr/bin:/bin:/usr/sbin:/sbin"

# Run the installer from a copy outside the clone, so it never decides to
# build from source.
cp "$ROOT/install.sh" "$T/install.sh"

PASS=0
FAIL=0
ok() {
	PASS=$((PASS + 1))
	printf 'ok   %s\n' "$1"
}
bad() {
	FAIL=$((FAIL + 1))
	printf 'FAIL %s\n' "$1"
	if [ -f "$T/out" ]; then
		sed 's/^/       | /' "$T/out"
	fi
}
check() {
	# check <description> <command...>
	d=$1
	shift
	if "$@"; then ok "$d"; else bad "$d"; fi
}
contains() { grep -F -- "$2" "$1" >/dev/null 2>&1; }

# A stub pair whose `heard --version` prints $1.
make_stubs() {
	dir=$1
	ver=$2
	mkdir -p "$dir"
	cat >"$dir/heard" <<EOF
#!/bin/sh
echo "heard \$*" >> "\${HEARD_STUB_LOG:-/dev/null}"
case "\$1" in
--version) echo "heard $ver" ;;
esac
exit 0
EOF
	cat >"$dir/heard-hook" <<EOF
#!/bin/sh
echo "heard-hook $ver"
EOF
	chmod 0755 "$dir/heard" "$dir/heard-hook"
}

make_release() {
	# make_release <out dir> <version>
	make_stubs "$T/stubs-$2" "$2"
	sh "$ROOT/packaging/make-release.sh" --from-dir "$T/stubs-$2" --out "$1" >/dev/null
}

install_sh() {
	# install_sh <base url dir> [args...]; output in $T/out, status in $RC
	base=$1
	shift
	set +e
	PATH="$BASE_PATH" HEARD_INSTALL_BASE_URL="file://$base" \
		sh "$T/install.sh" --prefix "$PREFIX" "$@" </dev/null >"$T/out" 2>&1
	RC=$?
	set -e
}

make_release "$T/rel1" 0.1.0
make_release "$T/rel2" 0.2.0

# --- help and bad options ---------------------------------------------------
set +e
sh "$T/install.sh" --help >"$T/out" 2>&1
RC=$?
set -e
check "--help exits 0 and prints usage" test "$RC" -eq 0
check "--help lists --prefix and --uninstall" sh -c "grep -q -- '--prefix' '$T/out' && grep -q -- '--uninstall' '$T/out'"
check "--help hides the test knobs" sh -c "! grep -q -- 'debug-build\|BASE_URL' '$T/out'"
set +e
sh "$T/install.sh" --bogus >"$T/out" 2>&1
RC=$?
set -e
check "an unknown option fails" test "$RC" -ne 0
check "an unknown option is named" contains "$T/out" "unknown option: --bogus"

# --- platform ---------------------------------------------------------------
set +e
HEARD_INSTALL_UNAME=Linux sh "$T/install.sh" --prefix "$PREFIX" </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "Linux is refused" test "$RC" -ne 0
check "Linux gets the 'coming' message" contains "$T/out" "Linux support is coming"
check "Linux installs nothing" test ! -e "$PREFIX/heard"

# --- fresh install ----------------------------------------------------------
: >"$LOG"
install_sh "$T/rel1" --no-setup
check "fresh install exits 0" test "$RC" -eq 0
check "heard installed and executable" test -x "$PREFIX/heard"
check "heard-hook installed and executable" test -x "$PREFIX/heard-hook"
check "installed version is 0.1.0" sh -c "'$PREFIX/heard' --version | grep -q 'heard 0.1.0'"
check "summary says Installed" contains "$T/out" "Installed heard 0.1.0"
check "checksum verified" contains "$T/out" "sha256 ok"
check "--no-setup does not run setup" sh -c "! grep -q 'heard setup' '$LOG'"
check "--no-setup prints the next step" contains "$T/out" "run heard setup"
check "PATH advice names the exact line" contains "$T/out" "export PATH=\"$PREFIX:\$PATH\""
check "no temp files left in the prefix" sh -c "[ \"\$(ls -A '$PREFIX' | sort | tr '\n' ' ')\" = 'heard heard-hook ' ]"

# Non-TTY stdin without --no-setup: still no setup, prints the hint.
: >"$LOG"
rm -rf "$PREFIX"
install_sh "$T/rel1"
check "non-TTY install exits 0" test "$RC" -eq 0
check "non-TTY stdin skips setup" sh -c "! grep -q 'heard setup' '$LOG'"
check "non-TTY stdin prints the hint" contains "$T/out" "run heard setup"

# On PATH: no advice.
set +e
PATH="$PREFIX:$BASE_PATH" HEARD_INSTALL_BASE_URL="file://$T/rel1" \
	sh "$T/install.sh" --prefix "$PREFIX" --no-setup </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "prefix on PATH: no PATH advice" sh -c "! grep -q 'is not on your PATH' '$T/out'"

# --- upgrade (re-run = upgrade) ---------------------------------------------
install_sh "$T/rel2" --no-setup
check "upgrade exits 0" test "$RC" -eq 0
check "upgrade replaced the binary" sh -c "'$PREFIX/heard' --version | grep -q 'heard 0.2.0'"
check "upgrade summary names both versions" contains "$T/out" "Upgraded heard 0.1.0 -> heard 0.2.0"
install_sh "$T/rel2" --no-setup
check "re-running the same version is fine" test "$RC" -eq 0

# --- shadowing --------------------------------------------------------------
make_stubs "$T/other" 9.9.9
set +e
PATH="$T/other:$PREFIX:$BASE_PATH" HEARD_INSTALL_BASE_URL="file://$T/rel2" \
	sh "$T/install.sh" --prefix "$PREFIX" --no-setup </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "a shadowing heard is warned about" contains "$T/out" "another heard at $T/other/heard comes first"

# --- bad checksum -----------------------------------------------------------
cp -R "$T/rel1" "$T/relbad"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" \
	heard-macos-universal.tar.gz >"$T/relbad/SHA256SUMS"
install_sh "$T/relbad" --no-setup
check "a bad checksum fails" test "$RC" -ne 0
check "a bad checksum says so" contains "$T/out" "checksum mismatch"
check "a bad checksum leaves the old install alone" sh -c "'$PREFIX/heard' --version | grep -q 'heard 0.2.0'"

# A tampered tarball with the original SUMS: same result.
cp -R "$T/rel2" "$T/reltamper"
printf 'x' >>"$T/reltamper/heard-macos-universal.tar.gz"
install_sh "$T/reltamper" --no-setup
check "a tampered tarball fails" test "$RC" -ne 0

# SUMS without our asset.
cp -R "$T/rel2" "$T/relnosum"
printf '%s  %s\n' deadbeef other.tar.gz >"$T/relnosum/SHA256SUMS"
install_sh "$T/relnosum" --no-setup
check "missing SUMS entry fails" sh -c "[ $RC -ne 0 ] && grep -q 'no entry' '$T/out'"

# --- no release, outside a clone ---------------------------------------------
mkdir -p "$T/empty"
install_sh "$T/empty" --no-setup
check "no release outside a clone fails" test "$RC" -ne 0
check "no release explains the way out" contains "$T/out" "--from-source"
set +e
PATH="$BASE_PATH" sh "$T/install.sh" --from-source --prefix "$PREFIX" </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "--from-source outside a clone is refused" sh -c "[ $RC -ne 0 ] && grep -q 'inside a heard-rust-cli clone' '$T/out'"

# --- uninstall --------------------------------------------------------------
mkdir -p "$HEARD_CLI_HOME/models"
echo keep >"$HEARD_CLI_HOME/models/kokoro.onnx"
echo mine >"$PREFIX/other-tool"
: >"$LOG"
set +e
PATH="$BASE_PATH" sh "$T/install.sh" --prefix "$PREFIX" --uninstall </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "--uninstall exits 0" test "$RC" -eq 0
check "--uninstall runs heard uninstall all" sh -c "grep -qx 'heard uninstall all' '$LOG'"
check "--uninstall stops the daemon first" sh -c "head -n 1 '$LOG' | grep -qx 'heard stop'"
check "--uninstall removes heard" test ! -e "$PREFIX/heard"
check "--uninstall removes heard-hook" test ! -e "$PREFIX/heard-hook"
check "--uninstall leaves other files in the prefix" test -f "$PREFIX/other-tool"
check "--uninstall keeps models and state" test -f "$HEARD_CLI_HOME/models/kokoro.onnx"
check "--uninstall mentions --purge" contains "$T/out" "--purge"

set +e
PATH="$BASE_PATH" sh "$T/install.sh" --prefix "$PREFIX" --uninstall </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "--uninstall twice is harmless" test "$RC" -eq 0

install_sh "$T/rel2" --no-setup
set +e
PATH="$BASE_PATH" sh "$T/install.sh" --prefix "$PREFIX" --uninstall --purge </dev/null >"$T/out" 2>&1
RC=$?
set -e
check "--uninstall --purge exits 0" test "$RC" -eq 0
check "--purge removes the state root" test ! -e "$HEARD_CLI_HOME"
check "--purge keeps HOME itself" test -d "$HOME"
set +e
sh "$T/install.sh" --purge >"$T/out" 2>&1
RC=$?
set -e
check "--purge alone is a usage error" test "$RC" -ne 0

# --- from source (only where heard-cli exists and it was asked for) ----------
if [ "${HEARD_TEST_FROM_SOURCE:-0}" = 1 ] && [ -d "$ROOT/crates/heard-cli" ]; then
	set +e
	PATH="$ORIG_PATH" CARGO_HOME="$ORIG_CARGO_HOME" RUSTUP_HOME="$ORIG_RUSTUP_HOME" \
		sh "$ROOT/install.sh" --from-source --debug-build --prefix "$T/src/bin" --no-setup </dev/null >"$T/out" 2>&1
	RC=$?
	set -e
	check "--from-source --debug-build installs" sh -c "[ $RC -eq 0 ] && [ -x '$T/src/bin/heard' ] && [ -x '$T/src/bin/heard-hook' ]"
else
	printf 'skip --from-source (set HEARD_TEST_FROM_SOURCE=1 once crates/heard-cli exists)\n'
fi

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
