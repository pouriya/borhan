#!/bin/sh
#
# borhan installer.
#
#   curl -fsSL https://raw.githubusercontent.com/pouriya/borhan/master/install.sh | sh
#
# Downloads a release tarball, checks it against its published checksum, and
# puts the binary in a directory on your PATH. It sets nothing up beyond that:
# whether this machine keeps memories or only talks to a server that does is a
# decision, and `borhan --help` is where it is made.
#
# Knobs, all optional:
#   BORHAN_VERSION   version to install, without the leading v  (default: latest)
#   BORHAN_BIN_DIR   where the binary goes                      (default: ~/.local/bin)
#   BORHAN_TARBALL   install this local tarball, skip the download entirely
#   BORHAN_REPO      owner/name to download from                (default: pouriya/borhan)
set -eu

REPO="${BORHAN_REPO:-pouriya/borhan}"
BIN_DIR="${BORHAN_BIN_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
case "$os/$(uname -m)" in
	Linux/x86_64)              target=x86_64-unknown-linux-musl ;;
	Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
	Darwin/arm64)              target=aarch64-apple-darwin ;;
	Darwin/x86_64)             target=x86_64-apple-darwin ;;
	# Git Bash and MSYS report a MINGW64_NT/MSYS_NT uname and will happily run
	# this script, but what it would install is a Linux binary with nowhere to
	# run. Send them to the installer that fetches the Windows build and puts it
	# on the user PATH.
	CYGWIN*|MINGW*|MSYS*)
		echo "borhan: this is Windows — use install.ps1 instead, from PowerShell:" >&2
		echo "  powershell -ExecutionPolicy Bypass -c \"irm https://raw.githubusercontent.com/$REPO/master/install.ps1 | iex\"" >&2
		exit 1
		;;
	*)
		echo "borhan: no release for $os $(uname -m)" >&2
		echo "built for: linux x86_64, linux aarch64, macOS arm64, macOS x86_64, windows x86_64" >&2
		echo "on anything else, build from source: https://github.com/$REPO" >&2
		exit 1
		;;
esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch() {
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL "$1" -o "$2"
	elif command -v wget >/dev/null 2>&1; then
		wget -qO "$2" "$1"
	else
		echo "borhan: need curl or wget" >&2
		exit 1
	fi
}

if [ -n "${BORHAN_TARBALL:-}" ]; then
	echo "borhan: unpacking $BORHAN_TARBALL"
	tar xzf "$BORHAN_TARBALL" -C "$tmp"
else
	version="${BORHAN_VERSION:-}"
	if [ -z "$version" ]; then
		fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp/latest.json"
		version="$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -1)"
		[ -n "$version" ] || { echo "borhan: no release published for $REPO yet" >&2; exit 1; }
	fi

	name="borhan-$version-$target.tar.gz"
	url="https://github.com/$REPO/releases/download/v$version/$name"
	echo "borhan: downloading $name"
	fetch "$url" "$tmp/$name"
	fetch "$url.sha256" "$tmp/$name.sha256"

	# The .sha256 names the file with no path in it, so verify from beside it.
	# macOS has no sha256sum and most Linuxes no shasum; check for both.
	( cd "$tmp" && if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -c "$name.sha256" >/dev/null
	else
		shasum -a 256 -c "$name.sha256" >/dev/null
	fi ) || { echo "borhan: checksum mismatch on $name — refusing to install" >&2; exit 1; }

	tar xzf "$tmp/$name" -C "$tmp"
fi

src="$(find "$tmp" -maxdepth 1 -type d -name 'borhan-*' | head -1)"
[ -n "$src" ] && [ -f "$src/borhan" ] || { echo "borhan: tarball does not look like a borhan release" >&2; exit 1; }

# Copied aside and renamed into place rather than written over: a `borhan serve`
# may be running off this exact path, and writing to a live executable is
# ETXTBSY. A rename only swaps the directory entry, so the running server
# finishes on the old inode and the next start picks up the new one.
mkdir -p "$BIN_DIR"
cp "$src/borhan" "$BIN_DIR/borhan.new"
chmod +x "$BIN_DIR/borhan.new"
mv -f "$BIN_DIR/borhan.new" "$BIN_DIR/borhan"

echo "borhan: installed $("$BIN_DIR/borhan" --version) at $BIN_DIR/borhan"

case ":$PATH:" in
	*":$BIN_DIR:"*) ;;
	*)
		echo
		echo "$BIN_DIR is not on your PATH. Add it to your shell's rc file:"
		echo
		echo "  export PATH=\"$BIN_DIR:\$PATH\""
		;;
esac

cat <<EOF

Next, start here:

  borhan --help

Or read Getting started:

  https://github.com/pouriya/borhan#getting-started
EOF
