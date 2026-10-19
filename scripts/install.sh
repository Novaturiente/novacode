#!/bin/sh
# Install the latest novacode release (L11.7):
#   curl -fsSL https://raw.githubusercontent.com/OWNER/novacode/master/scripts/install.sh | sh
# Puts `novacode` in $NOVACODE_BIN_DIR (default ~/.local/bin). Verifies the checksum.
# Never touches ~/.novacode. Update later with `novacode update`.
set -eu
repo="${NOVACODE_REPO:-OWNER/novacode}" # shortcut: placeholder until the repo exists
bin_dir="${NOVACODE_BIN_DIR:-$HOME/.local/bin}"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  *) echo "no release build for $(uname -s) $(uname -m); build from source: cargo install --locked --git https://github.com/$repo novacode" >&2; exit 1 ;;
esac
tag=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
[ -n "$tag" ] || { echo "could not find the latest release of $repo" >&2; exit 1; }
asset="novacode-${tag#v}-$target.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "downloading $asset"
curl -fsSL -o "$tmp/$asset" "https://github.com/$repo/releases/download/$tag/$asset"
curl -fsSL -o "$tmp/$asset.sha256" "https://github.com/$repo/releases/download/$tag/$asset.sha256"
want=$(cut -d' ' -f1 "$tmp/$asset.sha256")
if command -v sha256sum >/dev/null; then got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1); else got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ "$want" = "$got" ] || { echo "checksum mismatch; nothing installed" >&2; exit 1; }
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$bin_dir"
mv "$tmp/novacode" "$bin_dir/novacode"
echo "installed novacode $tag to $bin_dir/novacode"
case ":$PATH:" in *":$bin_dir:"*) ;; *) echo "add $bin_dir to your PATH" ;; esac
