#!/usr/bin/env bash
set -euo pipefail

# ── Defaults ───────────────────────────────────────────────────
REPO="hingbong/TCP-preconnection-relay"
TAG="${RELAY_VERSION:-builds}"
BIN_NAME="relay"
BIN_PATH="/usr/local/bin/${BIN_NAME}"
CONF_DIR="/etc/relay"
CONF_FILE="${CONF_DIR}/relay.toml"
SERVICE_FILE="/etc/systemd/system/${BIN_NAME}.service"

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

    # Prefer glibc dynamic linker hwcaps output when available.
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

    # Fallback for musl/minimal systems where glibc ld-linux is unavailable.
    # v4 = v3 + AVX-512 baseline features:
    # avx512f avx512bw avx512cd avx512dq avx512vl
    if [[ -r /proc/cpuinfo ]]; then
        local flags
        flags="$(grep -m1 '^flags' /proc/cpuinfo || true)"

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
           [[ "$flags" == *" abm "* || "$flags" == *" lzcnt "* ]] &&
           [[ "$flags" == *" movbe "* ]] &&
           [[ "$flags" == *" xsave "* ]]; then
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

    # Conservative fallback.
    echo "amd64-v2"
}

# ── Check architecture ─────────────────────────────────────────
ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
    err "unsupported architecture: $ARCH (only x86_64 is supported)"
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

# ── Stop existing service ─────────────────────────────────────
if systemctl is-active --quiet "${BIN_NAME}" 2>/dev/null; then
    info "stopping existing ${BIN_NAME} service"
    systemctl stop "${BIN_NAME}"
fi

# ── Create directories ────────────────────────────────────────
mkdir -p "$CONF_DIR"

# ── Download binary ───────────────────────────────────────────
info "downloading ${ASSET} (${CPU_LEVEL}) from ${TAG}..."
if command -v curl >/dev/null; then
    curl -fsSL "$DOWNLOAD_URL" -o "${BIN_PATH}.tmp"
elif command -v wget >/dev/null; then
    wget -q "$DOWNLOAD_URL" -O "${BIN_PATH}.tmp"
else
    err "curl or wget is required"
fi

chmod +x "${BIN_PATH}.tmp"
mv "${BIN_PATH}.tmp" "${BIN_PATH}"
info "installed ${BIN_PATH}"

# ── Write default config (preserve existing) ──────────────────
if [[ ! -f "$CONF_FILE" ]]; then
    cat > "$CONF_FILE" <<'TOML'
# relay.toml — all fields optional except these five
local_ip        = "0.0.0.0"
local_port      = 1234
remote_ip       = "CHANGE_ME"
remote_tcp_port = 443
remote_udp_port = 443

# Runtime tuning
pool_size           = 24
refill_batch        = 2
preconnect_ttl_ms   = 300000
splice_chunk        = 65536
log_enable          = true
TOML
    info "created ${CONF_FILE} — edit remote_ip before starting"
else
    info "config already exists at ${CONF_FILE}, skipping"
fi

# ── Install systemd service ───────────────────────────────────
cat > "$SERVICE_FILE" <<SVC
[Unit]
Description=TCP/UDP preconnection relay service
After=network.target nss-lookup.target network-online.target
Wants=network-online.target

[Service]
Type=simple
Restart=always
RestartSec=2s
CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW CAP_NET_BIND_SERVICE
ExecStartPre=/usr/bin/sleep 1s
ExecStart=${BIN_PATH} -c ${CONF_FILE}
ExecReload=/bin/kill -HUP \$MAINPID

[Install]
WantedBy=multi-user.target
SVC
info "installed ${SERVICE_FILE}"

# ── Reload & enable ───────────────────────────────────────────
systemctl daemon-reload
if [[ -f "$CONF_FILE" ]] && grep -qv 'CHANGE_ME' "$CONF_FILE" 2>/dev/null; then
    systemctl enable --now "${BIN_NAME}"
    info "service ${BIN_NAME} started and enabled on boot"
else
    systemctl enable "${BIN_NAME}"
    warn "service ${BIN_NAME} enabled but NOT started"
    warn "edit ${CONF_FILE} and set remote_ip, then:"
    warn "  systemctl start ${BIN_NAME}"
fi

echo ""
info "done."
info "  cpu:     ${CPU_LEVEL}"
info "  asset:   ${ASSET}"
info "  config:  ${CONF_FILE}"
info "  binary:  ${BIN_PATH}"
info "  service: ${BIN_NAME}"
info "  logs:    journalctl -u ${BIN_NAME} -f"