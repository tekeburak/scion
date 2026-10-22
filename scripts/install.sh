#!/bin/sh
# Installs the Scion binaries for this machine from a release.
#
#   curl -fsSL https://raw.githubusercontent.com/tekeburak/scion/main/scripts/install.sh | sh
#
# Picks the archive for this OS and architecture (Linux x86_64 / aarch64 get
# scn, scion-server, scion-ingest and scion-mcp; macOS arm64 / x86_64 get scn),
# checks it against the release's SHA256SUMS and copies the binaries into
# ~/.local/bin. Environment:
#
#   SCION_VERSION      a release such as 0.1.0 (default: the latest release)
#   SCION_INSTALL_DIR  where the binaries go (default: ~/.local/bin)
#   SCION_RELEASE_URL  where releases live (default: the GitHub releases page);
#                      a file:// URL serves a release directory from disk
#
# Needs curl, tar and sha256sum or shasum.
set -eu

release_url="${SCION_RELEASE_URL:-https://github.com/tekeburak/scion/releases}"
install_dir="${SCION_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
	echo "scion install: $*" >&2
	exit 1
}

case "$(uname -s)" in
Linux) os=unknown-linux-musl ;;
Darwin) os=apple-darwin ;;
*) fail "no release for $(uname -s); build from source: cargo install --locked --path crates/scn" ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*) fail "no release for $(uname -m); build from source: cargo install --locked --path crates/scn" ;;
esac
target="$arch-$os"

# GitHub serves a release's files under download/v<version>/ and the latest
# release's under latest/download/; the archive name comes from SHA256SUMS, so
# the latest version need not be looked up first.
if [ -n "${SCION_VERSION:-}" ]; then
	base="$release_url/download/v${SCION_VERSION#v}"
else
	base="$release_url/latest/download"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "no SHA256SUMS at $base"
archive="$(sed -n "s/^[0-9a-f]\{64\}  \(scion-[^ ]*-$target\.tar\.gz\)$/\1/p" "$tmp/SHA256SUMS")"
[ -n "$archive" ] || fail "the release at $base has no archive for $target"
curl -fsSL "$base/$archive" -o "$tmp/$archive" || fail "cannot download $base/$archive"

grep "  $archive\$" "$tmp/SHA256SUMS" >"$tmp/expected"
if command -v sha256sum >/dev/null 2>&1; then
	check="sha256sum -c"
else
	check="shasum -a 256 -c"
fi
(cd "$tmp" && $check expected >/dev/null 2>&1) || fail "$archive does not match its SHA256SUMS entry; nothing was installed"

tar -C "$tmp" -xzf "$tmp/$archive"
dir="$tmp/${archive%.tar.gz}"
mkdir -p "$install_dir"
for file in "$dir"/*; do
	name="$(basename "$file")"
	case "$name" in LICENSE | NOTICE) continue ;; esac
	cp "$file" "$install_dir/$name.new"
	chmod 755 "$install_dir/$name.new"
	mv "$install_dir/$name.new" "$install_dir/$name"
	echo "installed $install_dir/$name"
done

case ":$PATH:" in
*":$install_dir:"*) ;;
*) echo "add $install_dir to PATH to run scn" ;;
esac
