#!/usr/bin/env bash
# Open N TCP connections at the same time and verify that each connection
# performs multiple echo round trips. Each client waits a little between round
# trips, so all connections stay open simultaneously, forcing the server to
# handle many connections at once.
#
# Usage: ./concurrent_echo_test.sh [PORT] [CLIENTS] [ROUNDS]
#   PORT    port to connect to (default 3456)
#   CLIENTS number of concurrent connections (default 20)
#   ROUNDS  round trips per connection (default 5)

set -u

PORT="${1:-3456}"
CLIENTS="${2:-20}"
ROUNDS="${3:-5}"

RESULT_DIR="$(mktemp -d)"
trap 'rm -rf "$RESULT_DIR"' EXIT

client() {
    local id=$1
    local fail=0

    if ! exec 3<>"/dev/tcp/127.0.0.1/$PORT"; then
        echo "connect_failed" > "$RESULT_DIR/$id"
        return
    fi

    for r in $(seq 1 "$ROUNDS"); do
        local msg="client-$id round-$r"
        echo "$msg" >&3

        local reply=""
        IFS= read -r -t 5 reply <&3

        if [[ "$reply" != "$msg" ]]; then
            echo "mismatch round=$r sent='$msg' got='$reply'" >> "$RESULT_DIR/$id.err"
            fail=1
        fi

        sleep "0.$((RANDOM % 3 + 1))"
    done

    exec 3<&-
    if [[ $fail -eq 0 ]]; then
        echo "ok" > "$RESULT_DIR/$id"
    else
        echo "ng" > "$RESULT_DIR/$id"
    fi
}

echo "port=$PORT clients=$CLIENTS rounds=$ROUNDS"
echo "---"

for i in $(seq 1 "$CLIENTS"); do
    client "$i" &
done

sleep 1
if command -v ss > /dev/null; then
    established=$(ss -Htan "sport = :$PORT" 2>/dev/null | grep -c ESTAB)
    echo "concurrent ESTABLISHED connections during the test: $established"
else
    echo "(ss not available; skipping the concurrent connection snapshot)"
fi

wait

ok=0
ng=0
for i in $(seq 1 "$CLIENTS"); do
    status=$(cat "$RESULT_DIR/$i" 2>/dev/null || echo "no_result")
    if [[ "$status" == "ok" ]]; then
        ok=$((ok + 1))
    else
        ng=$((ng + 1))
        echo "client-$i: $status"
        cat "$RESULT_DIR/$i.err" 2>/dev/null
    fi
done

echo "---"
echo "result: OK=$ok NG=$ng / $CLIENTS connections x $ROUNDS round trips"
[[ $ng -eq 0 ]]
