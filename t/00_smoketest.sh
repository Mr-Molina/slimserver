#!/bin/bash
set -euo pipefail

LOG_DIR=$(mktemp -d)
SERVER_LOG="$LOG_DIR/server.log"
TIMEOUT=30
NODE_PID=""

wait_port() {
	local wait_seconds="${1:-10}"
	local pid="${2:-}"

	while [ "$wait_seconds" -gt 0 ]; do
		if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
			echo "LMS process (PID $pid) exited prematurely" >&2
			return 1
		fi
		if nc -z localhost 9000; then
			return 0
		fi
		sleep 1
		wait_seconds=$((wait_seconds - 1))
	done

	return 1
}

finish() {
	if [ -n "${NODE_PID:-}" ]; then
		kill -TERM -"$NODE_PID" 2>/dev/null || kill -TERM "$NODE_PID" 2>/dev/null || true
		local i=0
		while [ "$i" -lt 10 ] && kill -0 "$NODE_PID" 2>/dev/null; do
			sleep 0.5
			i=$((i + 1))
		done
		kill -KILL -"$NODE_PID" 2>/dev/null || kill -KILL "$NODE_PID" 2>/dev/null || true
		wait "$NODE_PID" 2>/dev/null || true
	fi
	if [ -f "${SERVER_LOG:-}" ]; then
		cat "$SERVER_LOG"
	fi
	if [ -n "${LOG_DIR:-}" ]; then
		rm -rf "$LOG_DIR"
	fi
}
trap finish EXIT

set -m
./slimserver.pl --logfile="$SERVER_LOG" &
NODE_PID=$!
set +m

wait_port "$TIMEOUT" "$NODE_PID" || {
	echo "Timing out trying to connect to LMS"
	exit 1
}

STATUS=$(curl -m1 -fsSX POST -H 'Content-Type: application/json' -d '{"id":0,"params":["",["serverstatus"]],"method":"slim.request"}' http://localhost:9000/jsonrpc.js)
[ -n "$STATUS" ]

printf '%s\n' "$STATUS" | jq .

printf '%s\n' "$STATUS" | jq -e '
	.error == null and
	(.result.version | type == "string" and length > 0)
' > /dev/null

DATE=$(curl -m1 -fsSX POST -H 'Content-Type: application/json' -d '{"id":1,"params":["",["date"]],"method":"slim.request"}' http://localhost:9000/jsonrpc.js)
[ -n "$DATE" ]

printf '%s\n' "$DATE" | jq .

printf '%s\n' "$DATE" | jq -e '
	.error == null and
	(.result.date_epoch as $epoch |
	.result.utc_offset_minutes as $offset |
	.result.is_dst as $dst |
	.result.timezone as $timezone |
	($epoch | type == "number" and floor == . and . > 0) and
	($offset | type == "number" and floor == . and . >= -720 and . <= 840) and
	($dst == 0 or $dst == 1) and
	($timezone | type == "string" and length > 0))
' > /dev/null
