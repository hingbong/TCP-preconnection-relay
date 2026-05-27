#!/bin/bash
set -euo pipefail

out_dir="${1:-dist}"
mkdir -p "$out_dir"

case "$(uname -m)" in
    x86_64|amd64)
        asset="tcp_pool-linux-amd64"
        cflags=(-O2 -pthread -march=x86-64 -mtune=generic)
        ;;
    aarch64|arm64)
        asset="tcp_pool-linux-arm64"
        cflags=(-O2 -pthread)
        ;;
    armv7l|armv7*)
        asset="tcp_pool-linux-armv7"
        cflags=(-O2 -pthread -march=armv7-a -mfpu=vfpv3-d16 -mfloat-abi=hard)
        ;;
    i386|i686)
        asset="tcp_pool-linux-386"
        cflags=(-O2 -pthread -march=i686 -mtune=generic)
        ;;
    *)
        echo "unsupported local architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

gcc "${cflags[@]}" -Wall -Wextra -Wshadow -Wformat=2 -o "$out_dir/$asset" tcp_pool.c
chmod +x "$out_dir/$asset"
file "$out_dir/$asset"
