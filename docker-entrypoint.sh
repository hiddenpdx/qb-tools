#!/bin/sh
set -eu

need_daemon=1
need_config=1

for arg in "$@"; do
    case "$arg" in
        --daemon)
            need_daemon=0
            ;;
        --config|--conf|-c|--config=*|--conf=*)
            need_config=0
            ;;
    esac
done

if [ "$need_config" -eq 1 ]; then
    set -- --config /config/config.toml "$@"
fi

if [ "$need_daemon" -eq 1 ]; then
    set -- --daemon "$@"
fi

exec /usr/local/bin/app "$@"
