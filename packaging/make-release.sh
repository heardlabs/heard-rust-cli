#!/bin/sh
# make-release.sh — build the macOS release tarball install.sh downloads.
#
#   packaging/make-release.sh                 # build both arches, lipo, tar
#   packaging/make-release.sh --out dist
#   packaging/make-release.sh --from-dir DIR  # package DIR/heard + DIR/heard-hook
#                                             # as they are (no build; tests)
#
# Output, in --out (default: dist/):
#   heard-macos-universal.tar.gz   heard-macos-universal/{heard,heard-hook,VERSION,…}
#   SHA256SUMS                     "<sha256>  heard-macos-universal.tar.gz"
#
# The asset names carry no version, so install.sh can fetch
# releases/latest/download/<name> without asking the GitHub API.

set -eu

NAME="heard-macos-universal"
TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
BINS="heard heard-hook"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/dist"
FROM_DIR=""

while [ $# -gt 0 ]; do
	case "$1" in
	--out)
		[ $# -ge 2 ] || { echo "--out needs a directory" >&2; exit 2; }
		OUT=$2
		shift 2
		;;
	--from-dir)
		[ $# -ge 2 ] || { echo "--from-dir needs a directory" >&2; exit 2; }
		FROM_DIR=$2
		shift 2
		;;
	-h | --help)
		sed -n '2,13p' "$0"
		exit 0
		;;
	*)
		echo "unknown option: $1" >&2
		exit 2
		;;
	esac
done

case "$OUT" in
/*) ;;
*) OUT="$(pwd)/$OUT" ;;
esac

sha256_of() {
	if command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | awk '{print $1}'
	else
		sha256sum "$1" | awk '{print $1}'
	fi
}

version() {
	# The workspace version: the first `version = "…"` under [workspace.package].
	awk '
		/^\[workspace\.package\]/ { inpkg = 1; next }
		/^\[/ { inpkg = 0 }
		inpkg && /^version *=/ { gsub(/.*= *"|".*/, ""); print; exit }
	' "$ROOT/Cargo.toml"
}

STAGE=$(mktemp -d "${TMPDIR:-/tmp}/heard-release.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/$NAME"
mkdir -p "$PKG"

if [ -n "$FROM_DIR" ]; then
	for b in $BINS; do
		[ -x "$FROM_DIR/$b" ] || { echo "$FROM_DIR/$b is missing or not executable" >&2; exit 1; }
		cp "$FROM_DIR/$b" "$PKG/$b"
	done
else
	[ "$(uname -s)" = Darwin ] || { echo "universal builds need macOS (lipo)" >&2; exit 1; }
	command -v cargo >/dev/null 2>&1 || { echo "cargo not found" >&2; exit 1; }
	for t in $TARGETS; do
		rustup target add "$t" >/dev/null 2>&1 || true
		(cd "$ROOT" && cargo build --release --locked --target "$t" -p heard-cli -p heard-hook)
	done
	target_dir=${CARGO_TARGET_DIR:-"$ROOT/target"}
	for b in $BINS; do
		set --
		for t in $TARGETS; do
			set -- "$@" "$target_dir/$t/release/$b"
		done
		lipo -create -output "$PKG/$b" "$@"
		# lipo keeps each slice's linker signature; re-sign the fat file
		# ad hoc so Gatekeeper-free local runs (curl | sh) never see an
		# invalid one. Developer ID signing + notarisation is a separate,
		# owner-run step.
		codesign --force --sign - "$PKG/$b" >/dev/null 2>&1 || true
		lipo -info "$PKG/$b"
	done
fi

chmod 0755 "$PKG/heard" "$PKG/heard-hook"
version >"$PKG/VERSION"
for f in LICENSE README.md THIRD-PARTY-NOTICES.md; do
	if [ -f "$ROOT/$f" ]; then
		cp "$ROOT/$f" "$PKG/$f"
	fi
done

mkdir -p "$OUT"
# Deterministic-ish archive: fixed order, no macOS extended attributes.
(cd "$STAGE" && COPYFILE_DISABLE=1 tar -czf "$OUT/$NAME.tar.gz" "$NAME")
(cd "$OUT" && printf '%s  %s\n' "$(sha256_of "$NAME.tar.gz")" "$NAME.tar.gz" >SHA256SUMS)

echo "wrote $OUT/$NAME.tar.gz"
echo "wrote $OUT/SHA256SUMS"
