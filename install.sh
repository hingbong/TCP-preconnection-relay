#!/bin/bash
set -e

echo "正在安装 TCP-preconnection-relay v1.6..."
echo "如果报错有个括号啥的，请重新到github上复制脚本链接，有变动"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APT_UPDATED=0

ensure_packages() {
    local missing=()
    local pkg

    for pkg in "$@"; do
        if ! dpkg -s "$pkg" >/dev/null 2>&1; then
            missing+=("$pkg")
        fi
    done

    if [ "${#missing[@]}" -eq 0 ]; then
        return 0
    fi

    if [ "$APT_UPDATED" -eq 0 ]; then
        apt update
        APT_UPDATED=1
    fi
    apt install -y "${missing[@]}"
}

detect_prebuilt_arch() {
    case "$(uname -m)" in
        x86_64|amd64)
            echo "linux-amd64"
            ;;
        aarch64|arm64)
            echo "linux-arm64"
            ;;
        armv7l|armv7*)
            echo "linux-armv7"
            ;;
        i386|i686)
            echo "linux-386"
            ;;
        *)
            return 1
            ;;
    esac
}

repo_clone_url() {
    local repo="${TCP_POOL_REPO:-Xeloan/TCP-preconnection-relay}"
    printf 'https://github.com/%s.git' "$repo"
}

binary_works() {
    local bin="$1"
    local out

    out="$("$bin" 2>&1 || true)"
    grep -q "ERROR: LOCAL_IP not set" <<< "$out"
}

install_binary_file() {
    local src="$1"
    local tmp

    tmp="$(mktemp /root/tcp_pool.new.XXXXXX)"
    cp "$src" "$tmp"
    chmod +x "$tmp"

    if ! binary_works "$tmp"; then
        rm -f "$tmp"
        return 1
    fi

    mv -f "$tmp" /root/tcp_pool
    return 0
}

is_repo_source_dir() {
    local dir="$1"

    [ -f "$dir/tcp_pool.c" ] || return 1
    [ -f "$dir/install.sh" ] || return 1

    if [ -d "$dir/.git" ]; then
        return 0
    fi

    [ -f "$dir/build-release.sh" ] && [ -d "$dir/dist" ] && return 0
    [ -f "$dir/README.md" ] && grep -q "TCP-preconnection-relay" "$dir/README.md" 2>/dev/null
}

clone_repo() {
    ensure_packages git ca-certificates

    local dst
    dst="$(mktemp -d /tmp/tcp-pool-src.XXXXXX)"

    local version="${TCP_POOL_VERSION:-}"
    if [ -n "$version" ] && [ "$version" != "latest" ]; then
        git clone --depth 1 --branch "$version" "$(repo_clone_url)" "$dst"
    else
        git clone --depth 1 "$(repo_clone_url)" "$dst"
    fi

    printf '%s' "$dst"
}

install_prebuilt_from_dir() {
    local src_dir="$1"

    [ "${TCP_POOL_PREBUILT:-1}" = "0" ] && return 1

    local arch
    if ! arch="$(detect_prebuilt_arch)"; then
        echo "当前架构暂时没有预编译包，回退到本地编译。"
        return 1
    fi

    local asset="tcp_pool-$arch"
    local src="$src_dir/dist/$asset"

    if [ ! -f "$src" ]; then
        echo "仓库 dist 中没有当前架构预编译二进制：$asset，回退到本地编译。"
        return 1
    fi

    echo "使用仓库 dist 里的预编译二进制：$asset"
    if install_binary_file "$src"; then
        echo "Prebuilt binary installed: $asset"
        return 0
    fi

    echo "预编译二进制无法在本机运行，回退到本地编译。"
    return 1
}

install_from_source_dir() {
    local src_dir="$1"

    ensure_packages build-essential

    if [ ! -f "$src_dir/tcp_pool.c" ]; then
        echo "缺少源码文件：$src_dir/tcp_pool.c" >&2
        exit 1
    fi

    local src_file dst_file
    src_file="$(readlink -f "$src_dir/tcp_pool.c")"
    dst_file="$(readlink -m /root/tcp_pool.c)"
    if [ "$src_file" != "$dst_file" ]; then
        cp "$src_file" "$dst_file"
    fi

    local tmp_bin
    tmp_bin="$(mktemp /root/tcp_pool.build.XXXXXX)"

    if gcc -O2 -pthread -march=native -o "$tmp_bin" /root/tcp_pool.c; then
        echo "Compile Succeeded"
    else
        echo "你的服务商太抠了，给你用这么一个古董CPU......"
        if ! gcc -O2 -pthread -march=x86-64 -mtune=generic -o "$tmp_bin" /root/tcp_pool.c; then
            rm -f "$tmp_bin"
            return 1
        fi
    fi

    chmod +x "$tmp_bin"
    mv -f "$tmp_bin" /root/tcp_pool
}

install_program() {
    local src_dir=""
    local cloned_dir=""

    if is_repo_source_dir "$SCRIPT_DIR" && [ "${TCP_POOL_USE_GIT:-0}" != "1" ]; then
        src_dir="$SCRIPT_DIR"
    else
        cloned_dir="$(clone_repo)"
        src_dir="$cloned_dir"
    fi

    install_prebuilt_from_dir "$src_dir" || install_from_source_dir "$src_dir"

    if [ -n "$cloned_dir" ]; then
        rm -rf "$cloned_dir"
    fi
}

if is_repo_source_dir "$SCRIPT_DIR" && [ "${TCP_POOL_PREBUILT:-0}" != "1" ] && [ "${TCP_POOL_USE_GIT:-0}" != "1" ]; then
    install_from_source_dir "$SCRIPT_DIR"
else
    install_program
fi

ensure_packages nano

mkdir -p /etc/tcp_pool

apply_tcp_tuning_generic() {
    echo "正在写入 TCP 调优（通用版）配置..."

    local cc="cubic"
    if grep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control 2>/dev/null; then
        cc="bbr"
    fi

    cat > /etc/sysctl.d/99-custom-network-tuning.conf <<EOF
# TCP-preconnection-relay 通用转发调优
net.ipv4.tcp_congestion_control = $cc
net.core.default_qdisc = fq
net.ipv4.tcp_fastopen = 3

net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.core.netdev_max_backlog = 250000

net.core.rmem_max = 134217728
net.core.wmem_max = 134217728
net.core.rmem_default = 1048576
net.core.wmem_default = 1048576
net.ipv4.tcp_rmem = 4096 1048576 134217728
net.ipv4.tcp_wmem = 4096 1048576 134217728
net.ipv4.udp_rmem_min = 8192
net.ipv4.udp_wmem_min = 8192

net.ipv4.tcp_fin_timeout = 10
net.ipv4.tcp_mtu_probing = 1
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_no_metrics_save = 1

net.ipv4.tcp_retries2 = 8
net.ipv4.tcp_timestamps = 1
net.ipv4.tcp_sack = 1
net.ipv4.tcp_syncookies = 1
net.ipv4.ip_local_port_range = 1024 65535

net.ipv4.tcp_keepalive_time = 300
net.ipv4.tcp_keepalive_intvl = 15
net.ipv4.tcp_keepalive_probes = 2
EOF
    
    if sysctl --system; then
        echo "TCP 调优（通用版）已应用。"
    else
        echo "TCP 调优配置文件已写入，但应用时出现报错。"
        echo "你可以手动检查 sysctl 输出，或稍后执行：sysctl --system"
    fi
}

cat > /usr/local/bin/tcp-pool-parse <<'EOF'
#!/bin/bash
set -euo pipefail

SRC="/etc/tcp_pool/relays.conf"
DST="/etc/tcp_pool"

[ -f "$SRC" ] || { echo "缺少 $SRC"; exit 1; }

mkdir -p "$DST"

find "$DST" -maxdepth 1 -type f -name '*.conf' ! -name 'relays.conf' -delete

current=""
declare -A section_seen
declare -A kv

trim() {
    local s="$1"
    s="${s#"${s%%[![:space:]]*}"}"
    s="${s%"${s##*[![:space:]]}"}"
    printf '%s' "$s"
}

reset_kv() {
    kv=(
        [LOCAL_IP]=""
        [LOCAL_PORT]=""
        [REMOTE_IP]=""
        [REMOTE_TCP_PORT]=""
        [REMOTE_UDP_PORT]=""
        [POOL_SIZE]=""
        [REFILL_BATCH]=""
        [CONNECT_TIMEOUT]=""
        [IDLE_TIMEOUT]=""
        [HALF_CLOSE_TIMEOUT]=""
        [PRECONNECT_TTL_MS]=""
        [SPLICE_CHUNK]=""
        [UDP_IDLE_TIMEOUT]=""
        [UDP_SOCKET_BUFFER]=""
        [LISTEN_BACKLOG]=""
        [LOG_ENABLE]=""
        [LOG_RATE_PER_SEC]=""
        [TCP_KEEPIDLE]=""
        [TCP_KEEPINTVL]=""
        [TCP_KEEPCNT]=""
        [TCP_USER_TIMEOUT_MS]=""
    )
}

is_valid_port() {
    local p="$1"
    [[ "$p" =~ ^[0-9]+$ ]] || return 1
    (( p >= 1 && p <= 65535 )) || return 1
    return 0
}

is_valid_int_range() {
    local n="$1"
    local min="$2"
    local max="$3"
    [[ "$n" =~ ^[0-9]+$ ]] || return 1
    (( n >= min && n <= max )) || return 1
    return 0
}

is_valid_bool() {
    case "$1" in
        0|1|true|false|TRUE|FALSE|yes|no|YES|NO|on|off|ON|OFF) return 0 ;;
        *) return 1 ;;
    esac
}

validate_and_write_section() {
    [[ -n "$current" ]] || return 0

    local key
    for key in LOCAL_IP LOCAL_PORT REMOTE_IP REMOTE_TCP_PORT REMOTE_UDP_PORT; do
        if [[ -z "${kv[$key]}" ]]; then
            echo "[$current] 缺少: $key" >&2
            exit 1
        fi
    done

    is_valid_port "${kv[LOCAL_PORT]}" || {
        echo "[$current] 不合法 LOCAL_PORT: ${kv[LOCAL_PORT]}" >&2
        exit 1
    }
    is_valid_port "${kv[REMOTE_TCP_PORT]}" || {
        echo "[$current] 不合法 REMOTE_TCP_PORT: ${kv[REMOTE_TCP_PORT]}" >&2
        exit 1
    }
    is_valid_port "${kv[REMOTE_UDP_PORT]}" || {
        echo "[$current] 不合法 REMOTE_UDP_PORT: ${kv[REMOTE_UDP_PORT]}" >&2
        exit 1
    }

    declare -A ranges=(
        [POOL_SIZE]="0 256"
        [REFILL_BATCH]="1 256"
        [CONNECT_TIMEOUT]="1 120"
        [IDLE_TIMEOUT]="30 86400"
        [HALF_CLOSE_TIMEOUT]="1 300"
        [PRECONNECT_TTL_MS]="10000 3600000"
        [SPLICE_CHUNK]="16384 1048576"
        [UDP_IDLE_TIMEOUT]="5 3600"
        [UDP_SOCKET_BUFFER]="65536 67108864"
        [LISTEN_BACKLOG]="128 65535"
        [LOG_RATE_PER_SEC]="0 10000"
        [TCP_KEEPIDLE]="30 86400"
        [TCP_KEEPINTVL]="1 3600"
        [TCP_KEEPCNT]="1 30"
        [TCP_USER_TIMEOUT_MS]="0 3600000"
    )

    local opt
    for opt in "${!ranges[@]}"; do
        [[ -z "${kv[$opt]}" ]] && continue
        read -r min max <<< "${ranges[$opt]}"
        is_valid_int_range "${kv[$opt]}" "$min" "$max" || {
            echo "[$current] 不合法 $opt: ${kv[$opt]}，范围 $min-$max" >&2
            exit 1
        }
    done

    if [[ -n "${kv[LOG_ENABLE]}" ]] && ! is_valid_bool "${kv[LOG_ENABLE]}"; then
        echo "[$current] 不合法 LOG_ENABLE: ${kv[LOG_ENABLE]}" >&2
        exit 1
    fi

    local outfile="$DST/$current.conf"
    : > "$outfile"
    chmod 600 "$outfile"

    {
        printf 'LOCAL_IP=%s\n' "${kv[LOCAL_IP]}"
        printf 'LOCAL_PORT=%s\n' "${kv[LOCAL_PORT]}"
        printf 'REMOTE_IP=%s\n' "${kv[REMOTE_IP]}"
        printf 'REMOTE_TCP_PORT=%s\n' "${kv[REMOTE_TCP_PORT]}"
        printf 'REMOTE_UDP_PORT=%s\n' "${kv[REMOTE_UDP_PORT]}"
        for opt in POOL_SIZE REFILL_BATCH CONNECT_TIMEOUT IDLE_TIMEOUT HALF_CLOSE_TIMEOUT PRECONNECT_TTL_MS SPLICE_CHUNK UDP_IDLE_TIMEOUT UDP_SOCKET_BUFFER LISTEN_BACKLOG LOG_ENABLE LOG_RATE_PER_SEC TCP_KEEPIDLE TCP_KEEPINTVL TCP_KEEPCNT TCP_USER_TIMEOUT_MS; do
            [[ -z "${kv[$opt]}" ]] && continue
            printf '%s=%s\n' "$opt" "${kv[$opt]}"
        done
    } > "$outfile"
}

reset_kv

while IFS= read -r raw || [ -n "$raw" ]; do
    line="${raw%$'\r'}"
    line="$(trim "$line")"

    [[ -z "$line" ]] && continue
    [[ "$line" == \#* ]] && continue
    [[ "$line" == \;* ]] && continue

    if [[ "$line" =~ ^\[(.+)\]$ ]]; then
        tag="${BASH_REMATCH[1]}"
    
        if [[ ! "$tag" =~ ^[A-Za-z0-9_-]+$ ]]; then
            echo "标签不合法（只能包含字母数字下划线横杠，参考python变量格式）: [$tag]" >&2
            exit 1
        fi
    
        next_section="$tag"
    
        validate_and_write_section
    
        current="$next_section"
        if [[ -n "${section_seen[$current]:-}" ]]; then
            echo "你标签写重复了: [$current]" >&2
            exit 1
        fi
        section_seen["$current"]=1
        reset_kv
        continue
    fi

    if [[ -z "$current" ]]; then
        echo "你漏写标签了: $line" >&2
        exit 1
    fi

    if [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
        key="${BASH_REMATCH[1]}"
        val="$(trim "${BASH_REMATCH[2]}")"
    else
        echo "配置项不合法 [$current]: $line" >&2
        exit 1
    fi

    case "$key" in
        LOCAL_IP|LOCAL_PORT|REMOTE_IP|REMOTE_TCP_PORT|REMOTE_UDP_PORT|POOL_SIZE|REFILL_BATCH|CONNECT_TIMEOUT|IDLE_TIMEOUT|HALF_CLOSE_TIMEOUT|PRECONNECT_TTL_MS|SPLICE_CHUNK|UDP_IDLE_TIMEOUT|UDP_SOCKET_BUFFER|LISTEN_BACKLOG|LOG_ENABLE|LOG_RATE_PER_SEC|TCP_KEEPIDLE|TCP_KEEPINTVL|TCP_KEEPCNT|TCP_USER_TIMEOUT_MS)
            kv["$key"]="$val"
            ;;
        *)
            echo "[$current] 你写了个莫名奇妙的配置进来: $key" >&2
            exit 1
            ;;
    esac
done < "$SRC"

validate_and_write_section

echo "配置解析完成"
EOF

chmod +x /usr/local/bin/tcp-pool-parse

cat > /etc/systemd/system/tcp-pool@.service <<'EOF'
[Unit]
Description=High Performance TCP Connection Pool (C Version)
Wants=network-online.target
After=network-online.target xray.service

[Service]
ExecStart=/root/tcp_pool
EnvironmentFile=/etc/tcp_pool/%i.conf

Nice=-10
LimitNOFILE=65535

Restart=always
RestartSec=3
User=root
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload

cat > /usr/local/bin/tcp-pool-start <<'EOF'
#!/bin/bash
set -euo pipefail
tcp-pool-parse

mapfile -t old_units < <(
    {
        systemctl list-units --full --all --no-legend 'tcp-pool@*.service' 2>/dev/null | awk '{print $1}'
        systemctl list-unit-files --full --no-legend 'tcp-pool@*.service' 2>/dev/null | awk '{print $1}'
    } | sort -u
)

for unit in "${old_units[@]}"; do
    [ -n "$unit" ] || continue
    systemctl stop "$unit" 2>/dev/null || true
    systemctl disable "$unit" 2>/dev/null || true
done

shopt -s nullglob
confs=(/etc/tcp_pool/*.conf)

instances=()
for conf in "${confs[@]}"; do
    name="$(basename "$conf")"
    [[ "$name" == "relays.conf" ]] && continue
    [[ "$name" != *.conf ]] && continue
    instances+=("${name%.conf}")
done

if [ "${#instances[@]}" -eq 0 ]; then
    echo "没有可启动的转发实例，请检查 /etc/tcp_pool/relays.conf"
    exit 1
fi

for inst in "${instances[@]}"; do
    echo "正在启动并设置开机自启 tcp-pool@$inst ..."
    systemctl enable "tcp-pool@$inst"
    systemctl restart "tcp-pool@$inst"
done

echo ""
echo "全部实例已启用完成。"
echo "查看日志命令："
for inst in "${instances[@]}"; do
    echo "journalctl -u tcp-pool@$inst -f"
done
EOF

chmod +x /usr/local/bin/tcp-pool-start

cat > /usr/local/bin/relay <<'EOF'
#!/bin/bash
set -euo pipefail

CONF="/etc/tcp_pool/relays.conf"
CONF_DIR="/etc/tcp_pool"
SERVICE_TEMPLATE="/etc/systemd/system/tcp-pool@.service"
BIN="/root/tcp_pool"
INSTALL_URL="${TCP_POOL_INSTALL_URL:-https://raw.githubusercontent.com/Xeloan/TCP-preconnection-relay/main/install.sh}"
EDITOR_CMD="${EDITOR:-nano}"

need_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "请使用 root 运行：sudo relay"
        exit 1
    fi
}

pause() {
    read -r -p "按回车继续..." _
}

valid_tag() {
    [[ "$1" =~ ^[A-Za-z0-9_-]+$ ]]
}

valid_port() {
    [[ "$1" =~ ^[0-9]+$ ]] && (( "$1" >= 1 && "$1" <= 65535 ))
}

valid_int_range() {
    local val="$1"
    local min="$2"
    local max="$3"
    [[ "$val" =~ ^[0-9]+$ ]] && (( val >= min && val <= max ))
}

ADVANCED_KEYS=(
    POOL_SIZE
    REFILL_BATCH
    CONNECT_TIMEOUT
    IDLE_TIMEOUT
    HALF_CLOSE_TIMEOUT
    PRECONNECT_TTL_MS
    SPLICE_CHUNK
    UDP_IDLE_TIMEOUT
    UDP_SOCKET_BUFFER
    LISTEN_BACKLOG
    LOG_ENABLE
    LOG_RATE_PER_SEC
    TCP_USER_TIMEOUT_MS
)

ADVANCED_DEFAULTS=(
    24
    8
    5
    240
    10
    50000
    262144
    60
    4194304
    16384
    1
    24
    0
)

ADVANCED_RANGES=(
    "0 256"
    "1 256"
    "1 120"
    "30 86400"
    "1 300"
    "10000 3600000"
    "16384 1048576"
    "5 3600"
    "65536 67108864"
    "128 65535"
    "0 1"
    "0 10000"
    "0 3600000"
)

ADVANCED_LABELS=(
    "预连接池大小，0表示关闭预连接"
    "每轮补连接最大并发数"
    "出站 TCP 连接超时秒数"
    "已使用 TCP 连接空闲回收秒数"
    "半关闭状态保留秒数"
    "池内未使用预连接轮换毫秒数"
    "单方向 splice 缓冲目标字节数"
    "UDP 会话空闲回收秒数"
    "UDP socket 缓冲目标字节数"
    "TCP listen backlog"
    "日志开关，0关闭，1开启"
    "每秒最多刷出的日志条数"
    "TCP 未确认数据最长等待毫秒数，0表示不用"
)

ensure_conf() {
    mkdir -p "$CONF_DIR"
    [ -f "$CONF" ] || touch "$CONF"
}

list_tags() {
    [ -f "$CONF" ] || return 0
    awk '
        /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
            line=$0
            sub(/^[[:space:]]*\[/, "", line)
            sub(/\][[:space:]]*$/, "", line)
            print line
        }
    ' "$CONF"
}

tag_exists() {
    local tag="$1"
    list_tags | grep -Fxq "$tag"
}

remove_section_file() {
    local tag="$1"
    local tmp
    tmp="$(mktemp)"
    awk -v target="$tag" '
        /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
            line=$0
            sub(/^[[:space:]]*\[/, "", line)
            sub(/\][[:space:]]*$/, "", line)
            skip=(line == target)
        }
        !skip { print }
    ' "$CONF" > "$tmp"
    cat "$tmp" > "$CONF"
    rm -f "$tmp"
}

get_section_value() {
    local tag="$1"
    local key="$2"
    awk -v target="$tag" -v wanted="$key" '
        /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
            line=$0
            sub(/^[[:space:]]*\[/, "", line)
            sub(/\][[:space:]]*$/, "", line)
            in_sec=(line == target)
        }
        in_sec && $0 ~ ("^" wanted "=") {
            sub("^[^=]*=", "")
            print
            exit
        }
    ' "$CONF"
}

append_section() {
    local tag="$1"
    shift

    {
        echo ""
        echo "[$tag]"
        local kv
        for kv in "$@"; do
            echo "$kv"
        done
    } >> "$CONF"
}

replace_section() {
    local tag="$1"
    shift

    remove_section_file "$tag"
    append_section "$tag" "$@"
}

show_instances() {
    echo ""
    echo "当前转发："

    local found=0
    while IFS= read -r tag; do
        [ -n "$tag" ] || continue
        found=1
        local state enabled local_ip local_port remote_ip remote_tcp remote_udp
        state="$(systemctl is-active "tcp-pool@$tag" 2>/dev/null || true)"
        enabled="$(systemctl is-enabled "tcp-pool@$tag" 2>/dev/null || true)"
        local_ip="$(awk -F= '/^LOCAL_IP=/{print $2; exit}' "$CONF_DIR/$tag.conf" 2>/dev/null || true)"
        local_port="$(awk -F= '/^LOCAL_PORT=/{print $2; exit}' "$CONF_DIR/$tag.conf" 2>/dev/null || true)"
        remote_ip="$(awk -F= '/^REMOTE_IP=/{print $2; exit}' "$CONF_DIR/$tag.conf" 2>/dev/null || true)"
        remote_tcp="$(awk -F= '/^REMOTE_TCP_PORT=/{print $2; exit}' "$CONF_DIR/$tag.conf" 2>/dev/null || true)"
        remote_udp="$(awk -F= '/^REMOTE_UDP_PORT=/{print $2; exit}' "$CONF_DIR/$tag.conf" 2>/dev/null || true)"

        if [ -z "$local_port" ]; then
            local_ip="$(awk -v target="$tag" '
                /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
                    line=$0; sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*$/, "", line); in_sec=(line == target)
                }
                in_sec && /^LOCAL_IP=/ { sub(/^LOCAL_IP=/, ""); print; exit }
            ' "$CONF")"
            local_port="$(awk -v target="$tag" '
                /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
                    line=$0; sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*$/, "", line); in_sec=(line == target)
                }
                in_sec && /^LOCAL_PORT=/ { sub(/^LOCAL_PORT=/, ""); print; exit }
            ' "$CONF")"
            remote_ip="$(awk -v target="$tag" '
                /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
                    line=$0; sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*$/, "", line); in_sec=(line == target)
                }
                in_sec && /^REMOTE_IP=/ { sub(/^REMOTE_IP=/, ""); print; exit }
            ' "$CONF")"
            remote_tcp="$(awk -v target="$tag" '
                /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
                    line=$0; sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*$/, "", line); in_sec=(line == target)
                }
                in_sec && /^REMOTE_TCP_PORT=/ { sub(/^REMOTE_TCP_PORT=/, ""); print; exit }
            ' "$CONF")"
            remote_udp="$(awk -v target="$tag" '
                /^[[:space:]]*\[[A-Za-z0-9_-]+\][[:space:]]*$/ {
                    line=$0; sub(/^[[:space:]]*\[/, "", line); sub(/\][[:space:]]*$/, "", line); in_sec=(line == target)
                }
                in_sec && /^REMOTE_UDP_PORT=/ { sub(/^REMOTE_UDP_PORT=/, ""); print; exit }
            ' "$CONF")"
        fi

        printf '  %-16s %-8s %-8s %s:%s -> %s tcp/%s udp/%s\n' \
            "$tag" "${state:-unknown}" "${enabled:-unknown}" \
            "${local_ip:-?}" "${local_port:-?}" "${remote_ip:-?}" "${remote_tcp:-?}" "${remote_udp:-?}"
    done < <(list_tags)

    if [ "$found" -eq 0 ]; then
        echo "  暂无转发配置"
    fi
    echo ""
}

prompt_default() {
    local prompt="$1"
    local def="$2"
    local val
    read -r -p "$prompt [$def]: " val >&2
    printf '%s' "${val:-$def}"
}

prompt_int_range() {
    local prompt="$1"
    local def="$2"
    local min="$3"
    local max="$4"
    local val

    while true; do
        val="$(prompt_default "$prompt" "$def")"
        if valid_int_range "$val" "$min" "$max"; then
            printf '%s' "$val"
            return 0
        fi
        echo "请输入 $min-$max 范围内的整数。" >&2
    done
}

collect_advanced_params() {
    local tag="${1:-}"
    local mode="${2:-create}"
    local configure="n"

    if [ "$mode" = "edit" ]; then
        read -r -p "是否修改高级参数？不修改会保留原值。[y/N]: " configure
        case "$configure" in
            y|Y) ;;
            *)
                local key val
                for key in "${ADVANCED_KEYS[@]}"; do
                    val="$(get_section_value "$tag" "$key")"
                    [ -n "$val" ] && printf '%s=%s\n' "$key" "$val"
                done
                return 0
                ;;
        esac
    else
        read -r -p "是否配置高级参数？不配置则全部使用程序默认值。[y/N]: " configure
        case "$configure" in
            y|Y) ;;
            *) return 0 ;;
        esac
    fi

    echo "高级参数配置：直接回车使用括号内默认/原值。" >&2
    local i key def cur prompt min max val
    for i in "${!ADVANCED_KEYS[@]}"; do
        key="${ADVANCED_KEYS[$i]}"
        def="${ADVANCED_DEFAULTS[$i]}"
        cur=""
        [ -n "$tag" ] && cur="$(get_section_value "$tag" "$key")"
        [ -n "$cur" ] && def="$cur"
        read -r min max <<< "${ADVANCED_RANGES[$i]}"
        prompt="${ADVANCED_LABELS[$i]} $key"
        val="$(prompt_int_range "$prompt" "$def" "$min" "$max")"
        printf '%s=%s\n' "$key" "$val"
    done
}

collect_relay_params() {
    local tag="${1:-}"
    local mode="${2:-create}"
    local local_ip local_port remote_ip remote_tcp remote_udp

    local_ip="$(prompt_default "本地监听 IP" "$(get_section_value "$tag" LOCAL_IP)")"
    [ -n "$local_ip" ] || local_ip="0.0.0.0"

    while true; do
        local_port="$(prompt_default "本地监听端口" "$(get_section_value "$tag" LOCAL_PORT)")"
        valid_port "$local_port" && break
        echo "端口必须是 1-65535。" >&2
    done

    while true; do
        remote_ip="$(prompt_default "远端 IP 或域名" "$(get_section_value "$tag" REMOTE_IP)")"
        [ -n "$remote_ip" ] && break
        echo "远端 IP 或域名不能为空。" >&2
    done

    while true; do
        remote_tcp="$(prompt_default "远端 TCP 端口" "$(get_section_value "$tag" REMOTE_TCP_PORT)")"
        valid_port "$remote_tcp" && break
        echo "端口必须是 1-65535。" >&2
    done

    remote_udp="$(prompt_default "远端 UDP 端口" "$(get_section_value "$tag" REMOTE_UDP_PORT)")"
    [ -n "$remote_udp" ] || remote_udp="$remote_tcp"
    while ! valid_port "$remote_udp"; do
        read -r -p "远端 UDP 端口: " remote_udp
    done

    printf 'LOCAL_IP=%s\n' "$local_ip"
    printf 'LOCAL_PORT=%s\n' "$local_port"
    printf 'REMOTE_IP=%s\n' "$remote_ip"
    printf 'REMOTE_TCP_PORT=%s\n' "$remote_tcp"
    printf 'REMOTE_UDP_PORT=%s\n' "$remote_udp"
    collect_advanced_params "$tag" "$mode"
}

create_relay() {
    need_root
    ensure_conf

    local tag

    while true; do
        read -r -p "转发标签（字母/数字/_/-，如 HK）: " tag
        if ! valid_tag "$tag"; then
            echo "标签不合法。"
            continue
        fi
        if tag_exists "$tag"; then
            echo "标签已存在。"
            continue
        fi
        break
    done

    mapfile -t relay_lines < <(collect_relay_params "$tag" create)
    append_section "$tag" "${relay_lines[@]}"

    tcp-pool-parse
    systemctl enable "tcp-pool@$tag"
    systemctl restart "tcp-pool@$tag"
    echo "已创建并启动：tcp-pool@$tag"
}

modify_relay() {
    need_root
    ensure_conf
    show_instances

    local tag
    read -r -p "要修改的标签: " tag
    if ! tag_exists "$tag"; then
        echo "标签不存在：$tag"
        return 1
    fi

    mapfile -t relay_lines < <(collect_relay_params "$tag" edit)
    replace_section "$tag" "${relay_lines[@]}"

    tcp-pool-parse
    read -r -p "是否立即重启该转发？[Y/n]: " yn
    case "$yn" in
        n|N) ;;
        *)
            systemctl enable "tcp-pool@$tag"
            systemctl restart "tcp-pool@$tag"
            ;;
    esac
    echo "已修改：$tag"
}

delete_relay() {
    need_root
    ensure_conf
    show_instances

    local tag
    read -r -p "要删除的标签: " tag
    if ! tag_exists "$tag"; then
        echo "标签不存在：$tag"
        return 1
    fi

    read -r -p "确认删除 [$tag]？[y/N]: " yn
    case "$yn" in
        y|Y) ;;
        *) echo "已取消。"; return 0 ;;
    esac

    systemctl stop "tcp-pool@$tag" 2>/dev/null || true
    systemctl disable "tcp-pool@$tag" 2>/dev/null || true
    remove_section_file "$tag"
    rm -f "$CONF_DIR/$tag.conf"
    systemctl daemon-reload
    echo "已删除：$tag"
}

restart_all() {
    need_root
    tcp-pool-start
}

stop_all() {
    need_root
    while IFS= read -r tag; do
        [ -n "$tag" ] || continue
        systemctl stop "tcp-pool@$tag" 2>/dev/null || true
    done < <(list_tags)
    echo "全部实例已停止。"
}

start_one() {
    need_root
    show_instances
    local tag
    read -r -p "要启动/重启的标签: " tag
    tag_exists "$tag" || { echo "标签不存在：$tag"; return 1; }
    tcp-pool-parse
    systemctl enable "tcp-pool@$tag"
    systemctl restart "tcp-pool@$tag"
    echo "已启动：$tag"
}

stop_one() {
    need_root
    show_instances
    local tag
    read -r -p "要停止的标签: " tag
    tag_exists "$tag" || { echo "标签不存在：$tag"; return 1; }
    systemctl stop "tcp-pool@$tag"
    echo "已停止：$tag"
}

logs_one() {
    need_root
    show_instances
    local tag
    read -r -p "要查看日志的标签: " tag
    tag_exists "$tag" || { echo "标签不存在：$tag"; return 1; }
    journalctl -u "tcp-pool@$tag" -f
}

edit_conf() {
    need_root
    ensure_conf
    "$EDITOR_CMD" "$CONF"
    tcp-pool-parse
    read -r -p "是否立即应用配置并重启全部实例？[Y/n]: " yn
    case "$yn" in
        n|N) ;;
        *) tcp-pool-start ;;
    esac
}

apply_tuning() {
    need_root
    local cc="cubic"
    if grep -qw bbr /proc/sys/net/ipv4/tcp_available_congestion_control 2>/dev/null; then
        cc="bbr"
    fi

    cat > /etc/sysctl.d/99-custom-network-tuning.conf <<SYSCTL
# TCP-preconnection-relay 通用转发调优
net.ipv4.tcp_congestion_control = $cc
net.core.default_qdisc = fq
net.ipv4.tcp_fastopen = 3

net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.core.netdev_max_backlog = 250000

net.core.rmem_max = 134217728
net.core.wmem_max = 134217728
net.core.rmem_default = 1048576
net.core.wmem_default = 1048576
net.ipv4.tcp_rmem = 4096 1048576 134217728
net.ipv4.tcp_wmem = 4096 1048576 134217728
net.ipv4.udp_rmem_min = 8192
net.ipv4.udp_wmem_min = 8192

net.ipv4.tcp_fin_timeout = 10
net.ipv4.tcp_mtu_probing = 1
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_no_metrics_save = 1

net.ipv4.tcp_retries2 = 8
net.ipv4.tcp_timestamps = 1
net.ipv4.tcp_sack = 1
net.ipv4.tcp_syncookies = 1
net.ipv4.ip_local_port_range = 1024 65535

net.ipv4.tcp_keepalive_time = 300
net.ipv4.tcp_keepalive_intvl = 15
net.ipv4.tcp_keepalive_probes = 2
SYSCTL

    sysctl --system
}

update_program() {
    need_root
    command -v curl >/dev/null 2>&1 || { echo "缺少 curl，请先安装。"; return 1; }

    echo "更新会同步更新主程序、systemd 模板、tcp-pool-* 辅助脚本和 relay 管理脚本。"
    echo "已有 /etc/tcp_pool/relays.conf 不会被安装脚本修改。"
    read -r -p "确认更新？[Y/n]: " yn
    case "$yn" in
        n|N) echo "已取消。"; return 0 ;;
    esac

    local tmp
    tmp="$(mktemp)"
    curl -fsSL "$INSTALL_URL" -o "$tmp"
    TCP_POOL_SKIP_PROMPTS=1 bash "$tmp"
    rm -f "$tmp"
    tcp-pool-start || true
    echo "更新完成，relay 管理脚本已同步更新。"
}

uninstall_program() {
    need_root
    read -r -p "确认卸载 TCP-preconnection-relay？这会停止服务并删除程序文件。[y/N]: " yn
    case "$yn" in
        y|Y) ;;
        *) echo "已取消。"; return 0 ;;
    esac

    mapfile -t units < <(
        {
            systemctl list-units --full --all --no-legend 'tcp-pool@*.service' 2>/dev/null | awk '{print $1}'
            systemctl list-unit-files --full --no-legend 'tcp-pool@*.service' 2>/dev/null | awk '{print $1}'
        } | sort -u
    )

    local unit
    for unit in "${units[@]}"; do
        [ -n "$unit" ] || continue
        systemctl stop "$unit" 2>/dev/null || true
        systemctl disable "$unit" 2>/dev/null || true
    done

    rm -f "$SERVICE_TEMPLATE"
    rm -f /usr/local/bin/tcp-pool-start /usr/local/bin/tcp-pool-parse /usr/local/bin/relay
    rm -f "$BIN" /root/tcp_pool.c

    read -r -p "是否同时删除所有配置文件 /etc/tcp_pool？[y/N]: " del_conf
    case "$del_conf" in
        y|Y) rm -rf "$CONF_DIR" ;;
        *) echo "已保留配置目录：$CONF_DIR" ;;
    esac

    systemctl daemon-reload || true
    echo "卸载完成。"
}

menu() {
    need_root
    while true; do
        clear 2>/dev/null || true
        echo "========================================"
        echo " TCP-preconnection-relay 管理"
        echo "========================================"
        show_instances
        echo "1) 创建转发"
        echo "2) 修改转发"
        echo "3) 删除转发"
        echo "4) 启动/重启某个转发"
        echo "5) 停止某个转发"
        echo "6) 重启并应用全部转发"
        echo "7) 停止全部转发"
        echo "8) 查看日志"
        echo "9) 编辑配置文件"
        echo "10) 应用 TCP 调优"
        echo "11) 更新程序和 relay 管理脚本"
        echo "12) 卸载程序"
        echo "0) 退出"
        echo ""
        read -r -p "请选择: " choice
        case "$choice" in
            1) create_relay; pause ;;
            2) modify_relay; pause ;;
            3) delete_relay; pause ;;
            4) start_one; pause ;;
            5) stop_one; pause ;;
            6) restart_all; pause ;;
            7) stop_all; pause ;;
            8) logs_one ;;
            9) edit_conf; pause ;;
            10) apply_tuning; pause ;;
            11) update_program; pause ;;
            12) uninstall_program; exit 0 ;;
            0) exit 0 ;;
            *) echo "无效选择"; pause ;;
        esac
    done
}

need_root

case "${1:-}" in
    add|create) create_relay ;;
    modify|mod|set) modify_relay ;;
    del|delete|remove) delete_relay ;;
    list|status) show_instances ;;
    start) start_one ;;
    stop) stop_one ;;
    stop-all) stop_all ;;
    restart|apply) restart_all ;;
    restart-all|apply-all) restart_all ;;
    logs|log) logs_one ;;
    edit) edit_conf ;;
    tune) apply_tuning ;;
    update) update_program ;;
    uninstall) uninstall_program ;;
    ""|menu) menu ;;
    *)
        echo "用法：relay [add|modify|delete|list|start|stop|stop-all|restart|logs|edit|tune|update|uninstall]"
        exit 1
        ;;
esac
EOF

chmod +x /usr/local/bin/relay

echo ""
echo "========================================"
echo " Install completed!"
echo "========================================"

if [ "${TCP_POOL_SKIP_PROMPTS:-0}" != "1" ]; then
    read -r -p "是否现在打开管理脚本 relay？[y/N]: " OPEN_RELAY_NOW
    case "$OPEN_RELAY_NOW" in
        y|Y)
            relay
            ;;
        *)
            echo "之后可以输入 relay 打开管理脚本。"
            ;;
    esac

    echo ""
    read -r -p "是否进行 TCP 调优（通用版）？ [y/N]: " TCP_TUNE_NOW
    case "$TCP_TUNE_NOW" in
        y|Y)
            apply_tcp_tuning_generic
            ;;
        *)
            echo "已跳过 TCP 调优。之后可输入 relay 并选择 TCP 调优。"
            ;;
    esac
fi
echo ""
echo "========================================"
echo " 常用命令说明"
echo "========================================"
echo "打开交互管理脚本："
echo "relay"
echo ""
echo "修改配置文件："
echo "nano /etc/tcp_pool/relays.conf"
echo ""
echo "应用配置并启动/重启全部转发："
echo "tcp-pool-start"
echo ""
echo "停止某个实例（把 HK 改成你自己的标签）："
echo "systemctl stop tcp-pool@HK"
echo ""
echo "禁用某个实例开机自启（把 HK 改成你自己的标签）："
echo "systemctl disable tcp-pool@HK"
echo ""
echo "查看某个实例日志（把 HK 改成你自己的标签），如果看到一坨Preconnect +1，说明成了："
echo "journalctl -u tcp-pool@HK -f"
echo "========================================"
