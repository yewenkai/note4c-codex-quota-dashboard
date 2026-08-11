#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
weekday=$(date +%u)
hour_minute=$(date +%H%M)
minute=$(date +%M)

# 工作日 08:30–17:15 每刻钟执行。其他时段和周末只在整点执行。
if [ "$weekday" -le 5 ] && [ "$hour_minute" -ge 0830 ] && [ "$hour_minute" -le 1715 ]; then
    exec "$script_dir/codex-note4c-relay" sync \
        --config "$script_dir/note4c-sync.json" --refresh
fi

if [ "$minute" = "00" ]; then
    exec "$script_dir/codex-note4c-relay" sync \
        --config "$script_dir/note4c-sync.json" --refresh
fi

exit 0
