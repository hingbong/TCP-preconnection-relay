#!/usr/bin/env bash
set -euo pipefail

# ── Defaults ───────────────────────────────────────────────────
REPO="hingbong/TCP-preconnection-relay"
TAG="${RELAY_VERSION:-builds}"
BIN_NAME="relay"
BIN_DIR="/usr/local/bin"
BIN_PATH="${BIN_DIR}/${BIN_NAME}"
SERVICE_NAME="${BIN_NAME}"

# ── Colors ─────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

info() { echo -e "${GREEN}[+]${NC} $*"; }
warn() { echo -e "${CYAN}[~]${NC} $*"; }
err()  { echo -e "${RED}[!]${NC} $*"; exit 1; }

# ── Detect CPU level ───────────────────────────────────────────
detect_cpu_level() {
    local ld_path=""
    local candidate

    for candidate in \
        /lib64/ld-linux-x86-64.so.2 \
        /lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 \
        /lib/ld-linux-x86-64.so.2
    do
        if [[ -x "$candidate" ]]; then
            ld_path="$candidate"
            break
        fi
    done

    if [[ -n "$ld_path" ]]; then
        if "$ld_path" --help 2>/dev/null | grep -q "x86-64-v4 (supported"; then
            echo "amd64-v4"
            return
        fi

        if "$ld_path" --help 2>/dev/null | grep -q "x86-64-v3 (supported"; then
            echo "amd64-v3"
            return
        fi

        if "$ld_path" --help 2>/dev/null | grep -q "x86-64-v2 (supported"; then
            echo "amd64-v2"
            return
        fi
    fi

    if [[ -r /proc/cpuinfo ]]; then
        local flags
        flags=" $(grep -m1 '^flags' /proc/cpuinfo | cut -d: -f2- || true) "

        if [[ "$flags" == *" avx512f "* ]] &&
           [[ "$flags" == *" avx512bw "* ]] &&
           [[ "$flags" == *" avx512cd "* ]] &&
           [[ "$flags" == *" avx512dq "* ]] &&
           [[ "$flags" == *" avx512vl "* ]]; then
            echo "amd64-v4"
            return
        fi

        if [[ "$flags" == *" avx "* ]] &&
           [[ "$flags" == *" avx2 "* ]] &&
           [[ "$flags" == *" bmi1 "* ]] &&
           [[ "$flags" == *" bmi2 "* ]] &&
           [[ "$flags" == *" f16c "* ]] &&
           [[ "$flags" == *" fma "* ]] &&
           [[ "$flags" == *" movbe "* ]] &&
           [[ "$flags" == *" xsave "* ]] &&
           { [[ "$flags" == *" abm "* ]] || [[ "$flags" == *" lzcnt "* ]]; }; then
            echo "amd64-v3"
            return
        fi

        if [[ "$flags" == *" cx16 "* ]] &&
           [[ "$flags" == *" lahf_lm "* ]] &&
           [[ "$flags" == *" popcnt "* ]] &&
           [[ "$flags" == *" sse4_1 "* ]] &&
           [[ "$flags" == *" sse4_2 "* ]] &&
           [[ "$flags" == *" ssse3 "* ]]; then
            echo "amd64-v2"
            return
        fi
    fi

    echo "amd64-v2"
}

# ── Basic checks ───────────────────────────────────────────────
if [[ "$(id -u)" -ne 0 ]]; then
    err "please run as root"
fi

ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
    err "unsupported architecture: $ARCH (only x86_64 is supported)"
fi

if ! command -v systemctl >/dev/null; then
    err "systemctl is required"
fi

if ! systemctl list-unit-files "${SERVICE_NAME}.service" >/dev/null 2>&1; then
    err "systemd service not found: ${SERVICE_NAME}.service"
fi

CPU_LEVEL="${RELAY_CPU:-$(detect_cpu_level)}"

case "$CPU_LEVEL" in
    amd64|amd64-v2|amd64-v3|amd64-v4)
        ;;
    *)
        err "invalid RELAY_CPU=${CPU_LEVEL}; expected amd64, amd64-v2, amd64-v3, or amd64-v4"
        ;;
esac

ASSET="relay-${CPU_LEVEL}"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${TAG}/${ASSET}"

mkdir -p "$BIN_DIR"

TMP_PATH="$(mktemp "${BIN_DIR}/.${BIN_NAME}.update.XXXXXX")"
cleanup() {
    rm -f "$TMP_PATH"
}
trap cleanup EXIT

# ── Download ───────────────────────────────────────────────────
info "downloading ${ASSET} from ${TAG}"
info "url: ${DOWNLOAD_URL}"

if command -v curl >/dev/null; then
    curl -fL --retry 3 --retry-delay 1 "$DOWNLOAD_URL" -o "$TMP_PATH"
elif command -v wget >/dev/null; then
    wget -q "$DOWNLOAD_URL" -O "$TMP_PATH"
else
    err "curl or wget is required"
fi

chmod 0755 "$TMP_PATH"

# ── Sanity check ───────────────────────────────────────────────
if ! file "$TMP_PATH" | grep -q "ELF 64-bit"; then
    err "downloaded file is not a 64-bit ELF binary"
fi

# ── Replace binary atomically ──────────────────────────────────
mv -f "$TMP_PATH" "$BIN_PATH"
trap - EXIT

info "installed ${BIN_PATH}"
info "restarting ${SERVICE_NAME} service"

# Do not pre-stop. Let systemd perform restart transaction.
systemctl restart "$SERVICE_NAME"

info "service restarted"
info "status:"
systemctl --no-pager --full status "$SERVICE_NAME" || true

echo ""
info "done."
info "  cpu:     ${CPU_LEVEL}"
info "  asset:   ${ASSET}"
info "  binary:  ${BIN_PATH}"
info "  service: ${SERVICE_NAME}"
info "  logs:    journalctl -u ${SERVICE_NAME} -f"