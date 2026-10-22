#!/bin/sh
# Runs scripts/install.sh against release directories served from disk: a
# release named by SCION_VERSION, the latest release, and a corrupted archive,
# which must be refused with its name and install nothing.
#
#   scripts/install-test.sh
set -eu

root="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

case "$(uname -s)" in Linux) os=unknown-linux-musl ;; Darwin) os=apple-darwin ;; esac
case "$(uname -m)" in x86_64 | amd64) arch=x86_64 ;; aarch64 | arm64) arch=aarch64 ;; esac

# A release directory laid out as GitHub serves one: an archive per target
# whose binaries print their version, and SHA256SUMS.
release() {
	version="$1"
	dir="$tmp/releases/download/v$version"
	mkdir -p "$dir"
	for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl "$arch-$os"; do
		name="scion-$version-$target"
		[ -e "$dir/$name.tar.gz" ] && continue
		mkdir -p "$tmp/pack/$name"
		printf '#!/bin/sh\necho "scn %s %s"\n' "$version" "$target" >"$tmp/pack/$name/scn"
		chmod 755 "$tmp/pack/$name/scn"
		echo license >"$tmp/pack/$name/LICENSE"
		tar -C "$tmp/pack" -czf "$dir/$name.tar.gz" "$name"
	done
	(cd "$dir" && if command -v sha256sum >/dev/null 2>&1; then sha256sum -- *.tar.gz; else shasum -a 256 -- *.tar.gz; fi >SHA256SUMS)
}

expect() {
	got="$("$tmp/bin/scn")"
	[ "$got" = "$1" ] || { echo "FAIL: scn printed '$got', expected '$1'" >&2; exit 1; }
}

release 0.1.0
release 0.2.0
mkdir -p "$tmp/releases/latest"
cp -R "$tmp/releases/download/v0.2.0" "$tmp/releases/latest/download"

export SCION_RELEASE_URL="file://$tmp/releases" SCION_INSTALL_DIR="$tmp/bin"

SCION_VERSION=0.1.0 sh "$root/scripts/install.sh" >/dev/null
expect "scn 0.1.0 $arch-$os"
[ ! -e "$tmp/bin/LICENSE" ] || { echo "FAIL: LICENSE was installed as a binary" >&2; exit 1; }

sh "$root/scripts/install.sh" >/dev/null
expect "scn 0.2.0 $arch-$os"

archive="scion-0.1.0-$arch-$os.tar.gz"
printf 'corrupt' >>"$tmp/releases/download/v0.1.0/$archive"
if SCION_VERSION=0.1.0 sh "$root/scripts/install.sh" >/dev/null 2>"$tmp/err"; then
	echo "FAIL: a corrupted archive was installed" >&2
	exit 1
fi
grep -q "$archive does not match" "$tmp/err" || { echo "FAIL: refusal does not name $archive:" >&2; cat "$tmp/err" >&2; exit 1; }
expect "scn 0.2.0 $arch-$os"

echo "install.sh: ok"
