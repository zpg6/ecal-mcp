#!/usr/bin/env bash
# ecal-mcp installer.
#
# Usage:
#   curl -fsSL https://zpg6.github.io/ecal-mcp/install.sh | bash
#
# Optional env vars:
#   ECAL_MCP_VERSION   Release tag to install (e.g. v0.1.1). Defaults to "latest".
#   ECAL_MCP_PREFIX    Install prefix. Defaults to /usr/local (binary -> $PREFIX/bin).
#   ECAL_MCP_REPO      GitHub repo "owner/name". Defaults to zpg6/ecal-mcp.

set -euo pipefail

repo="${ECAL_MCP_REPO:-zpg6/ecal-mcp}"
prefix="${ECAL_MCP_PREFIX:-/usr/local}"
version="${ECAL_MCP_VERSION:-latest}"

err() { printf 'ecal-mcp install: %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) ;;
  MINGW*|MSYS*|CYGWIN*) err "use install.ps1 on Windows (run from PowerShell)." ;;
  *) err "only Linux prebuilt binaries are published; build from source on $(uname -s)." ;;
esac

case "$(uname -m)" in
  x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
  *) err "unsupported architecture: $(uname -m)" ;;
esac

for cmd in curl tar sha256sum; do
  command -v "$cmd" >/dev/null 2>&1 || err "missing required command: $cmd"
done

if [[ "$version" == "latest" ]]; then
  api="https://api.github.com/repos/${repo}/releases/latest"
else
  api="https://api.github.com/repos/${repo}/releases/tags/${version}"
fi

tag=$(curl -fsSL "$api" \
  | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
  | head -n1)
[[ -n "$tag" ]] || err "could not resolve release tag from $api"

ver="${tag#v}"
stem="ecal-mcp-${ver}-${target}"
base="https://github.com/${repo}/releases/download/${tag}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading ${stem}.tar.gz from ${tag}..."
curl -fsSL -o "$tmp/${stem}.tar.gz"        "${base}/${stem}.tar.gz"
curl -fsSL -o "$tmp/${stem}.tar.gz.sha256" "${base}/${stem}.tar.gz.sha256"

(cd "$tmp" && sha256sum -c "${stem}.tar.gz.sha256") \
  || err "sha256 mismatch for ${stem}.tar.gz"

tar -C "$tmp" -xzf "$tmp/${stem}.tar.gz"

bin="$tmp/${stem}/ecal-mcp"
[[ -x "$bin" ]] || err "extracted archive missing executable at $bin"

dest_dir="${prefix}/bin"
# Use sudo only if we can't write into dest_dir ourselves. `install -m -d`
# tries to chmod the target dir, which fails on existing system dirs we
# don't own (e.g. runner-writable /usr/local/bin). So only create the dir
# when missing, and skip the chmod-during-create when it already exists.
if [[ -d "$dest_dir" && -w "$dest_dir" ]]; then
  install -m 0755 "$bin" "$dest_dir/ecal-mcp"
elif [[ ! -e "$dest_dir" && -w "$prefix" ]]; then
  install -m 0755 -d "$dest_dir"
  install -m 0755 "$bin" "$dest_dir/ecal-mcp"
else
  echo "Installing to $dest_dir (requires sudo)..."
  sudo install -m 0755 -d "$dest_dir"
  sudo install -m 0755 "$bin" "$dest_dir/ecal-mcp"
fi

echo "Installed ecal-mcp ${tag} -> ${dest_dir}/ecal-mcp"
echo "Note: ecal-mcp requires the eCAL v6 runtime on the host."
echo "      See https://github.com/eclipse-ecal/ecal/releases"
