#!/bin/sh
set -eu

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"

case "$PUID" in
    ''|*[!0-9]*)
        echo "PUID must be a numeric user id" >&2
        exit 64
        ;;
esac

case "$PGID" in
    ''|*[!0-9]*)
        echo "PGID must be a numeric group id" >&2
        exit 64
        ;;
esac

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

exec su-exec "$PUID:$PGID" /usr/local/bin/app "$@"
