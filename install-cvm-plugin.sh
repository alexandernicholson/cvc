#!/usr/bin/env bash
# Install the cvc CVM plugin from this checkout.
set -euo pipefail

CVM_DIR="${CVM_DIR:-$HOME/.cvm}"
SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST_DIR="$CVM_DIR/plugins/cvc"

[[ -x "$CVM_DIR/bin/cvm" ]] || command -v cvm >/dev/null 2>&1 || {
  printf 'error: cvm is required\n' >&2
  exit 1
}
[[ -f "$CVM_DIR/plugins/cvp/plugin.sh" ]] || {
  printf 'error: cvp is required; run: cvm plugin install alexandernicholson/cvp\n' >&2
  exit 1
}

mkdir -p "$DEST_DIR/cvm-plugin"
install -m 0755 "$SOURCE_DIR/plugin.sh" "$DEST_DIR/plugin.sh"
install -m 0755 "$SOURCE_DIR/cvm-plugin/cvc.sh" "$DEST_DIR/cvm-plugin/cvc.sh"
printf '✓ Installed cvc CVM plugin at %s\n' "$DEST_DIR"
printf '  Configure it with: cvm cvc configure\n'
