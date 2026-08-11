#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
weekday=$(date +%u)
hour_minute=$(date +%H%M)
minute=$(date +%M)

# launchd does not load the interactive shell or NVM environment. Locate a
# sufficiently new Node.js runtime before codex-auth's /usr/bin/env node
# shebang is evaluated.
node_bin=""
for node_candidate in \
    /opt/homebrew/bin/node \
    /usr/local/bin/node \
    "$HOME"/.nvm/versions/node/*/bin/node
do
    if [ -x "$node_candidate" ] &&
        "$node_candidate" -e 'const [a,b]=process.versions.node.split(".").map(Number); process.exit(a>22||(a===22&&b>=21)?0:1)' \
            >/dev/null 2>&1
    then
        node_bin=$node_candidate
        break
    fi
done
if [ -z "$node_bin" ]; then
    echo "找不到 Node.js 22.21+；codex-auth 无法在 launchd 中运行" >&2
    exit 127
fi
PATH=$(dirname -- "$node_bin"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
export PATH

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
