#!/usr/bin/env bash
# cvc — CVM plugin manifest

CVM_PLUGIN_NAME="cvc"
CVM_PLUGIN_COMMAND="cvc"
CVM_PLUGIN_VERSION="0.1.0"
CVM_PLUGIN_DESCRIPION="Configure and verify Claude Code profiles for a cvc Codex gateway"

_CVC_PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cvm_plugin_main() {
  # shellcheck disable=SC1091
  source "$_CVC_PLUGIN_DIR/cvm-plugin/cvc.sh"
  cvc_plugin_main "$@"
}

cvm_plugin_init() {
  if ! command -v cvm >/dev/null 2>&1; then
    return 0
  fi
  if ! cvm plugin list 2>/dev/null | grep -q 'cvp'; then
    printf '%s\n' "cvc plugin installed. Install cvp before configuring a profile:" >&2
    printf '%s\n' "  cvm plugin install alexandernicholson/cvp" >&2
  fi
}
