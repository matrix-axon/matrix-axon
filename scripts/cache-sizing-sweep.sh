#!/usr/bin/env bash
#
# Read-only sizing sweep for the ADR 0085 offline-first cache.
#
# Measures what a client-side cache would have to hold and how long the
# unpaginated room list takes to arrive, so the cache ceilings in ADR 0085 come
# from a real account instead of a test one. Issues only GETs; writes nothing.
#
#   AXON_TOKEN=<bearer> AXON_BASE=http://host:8080 scripts/cache-sizing-sweep.sh
#
# Optional:
#   SAMPLE=40   how many rooms to sample for timeline pages (default 40)
#               rooms are sampled across the recency order, not just the head,
#               so the busy/quiet mix is represented
#   LIMIT=50    events per timeline page (default 50 = the client's PAGE_LIMIT)
#
# Mint a token with `axon token issue --label cache-sweep` and revoke it after
# with `axon token revoke --label cache-sweep`. The script never prints it.

set -uo pipefail

BASE=${AXON_BASE:-http://localhost:8080}
SAMPLE=${SAMPLE:-40}
LIMIT=${LIMIT:-50}

if [ -z "${AXON_TOKEN:-}" ]; then
  echo "AXON_TOKEN is not set. Mint one with: axon token issue --label cache-sweep" >&2
  exit 1
fi
for tool in curl jq awk; do
  command -v "$tool" >/dev/null || { echo "missing required tool: $tool" >&2; exit 1; }
done

auth=(-H "Authorization: Bearer $AXON_TOKEN")

code=$(curl -s -m 10 -o /dev/null -w '%{http_code}' "${auth[@]}" "$BASE/v1/status")
if [ "$code" != "200" ]; then
  echo "GET /v1/status returned $code — check AXON_BASE and AXON_TOKEN" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "=== /v1/rooms — the unpaginated room list (one record, on the boot path) ==="
# Time it: at thousands of rooms this response gates every load and refresh.
read -r rooms_time rooms_bytes < <(
  curl -s -m 300 "${auth[@]}" -o "$tmp/rooms.json" \
    -w '%{time_total} %{size_download}' "$BASE/v1/rooms"
)
room_count=$(jq -r '(.data | if type=="array" then . else .rooms end) | length' "$tmp/rooms.json")
awk -v b="$rooms_bytes" -v n="$room_count" -v t="$rooms_time" 'BEGIN {
  printf "  rooms:          %d\n", n
  printf "  payload:        %d bytes (%.2f MB)\n", b, b/1048576
  printf "  per room:       %.0f bytes\n", (n > 0 ? b/n : 0)
  printf "  wall time:      %.2f s\n", t
  printf "\n  A cached slice of the first 200 rooms would be ~%.0f KB.\n", (n > 0 ? (b/n)*200/1024 : 0)
}'

echo
echo "=== timeline pages (limit=$LIMIT) — the per-room cache record ==="
jq -r '(.data | if type=="array" then . else .rooms end)[] | "\(.account_id)\t\(.room_id)"' \
  "$tmp/rooms.json" > "$tmp/all-rooms.tsv"
total_rooms=$(wc -l < "$tmp/all-rooms.tsv")
# Stride-sample across the recency order rather than taking the head, so the
# sample spans busy and quiet rooms alike.
awk -v n="$total_rooms" -v k="$SAMPLE" 'BEGIN { step = (k > 0 && n > k) ? int(n/k) : 1 }
  (NR - 1) % step == 0 { print; c++ } c >= k { exit }' "$tmp/all-rooms.tsv" > "$tmp/sample.tsv"
echo "  sampling $(wc -l < "$tmp/sample.tsv") of $total_rooms rooms"
echo

printf "  %-40s %7s %10s %8s\n" room events bytes seconds
while IFS=$'\t' read -r acct room; do
  enc=$(printf '%s' "$room" | jq -sRr @uri)
  read -r t b < <(
    curl -s -m 60 "${auth[@]}" -o "$tmp/page.json" \
      -w '%{time_total} %{size_download}' \
      "$BASE/v1/accounts/$acct/rooms/$enc/timeline?limit=$LIMIT"
  )
  n=$(jq -r '.data.events | length' "$tmp/page.json" 2>/dev/null)
  [ -z "$n" ] && n=0
  printf "  %-40s %7s %10s %8s\n" "${room:0:38}" "$n" "$b" "$t"
  printf '%s\t%s\n' "$b" "$n" >> "$tmp/sizes.tsv"
done < "$tmp/sample.tsv"

echo
echo "=== distribution ==="
sort -n "$tmp/sizes.tsv" | awk -F'\t' '
  { bytes[NR] = $1; events[NR] = $2; sum += $1; ev += $2; if ($2 >= '"$LIMIT"') full++ }
  END {
    if (NR == 0) { print "  no rooms sampled"; exit }
    med = bytes[int((NR + 1) / 2)]
    p95i = int(NR * 0.95); if (p95i < 1) p95i = 1
    printf "  n=%d sampled\n", NR
    printf "  bytes/page   min=%.1f KB  median=%.1f KB  p95=%.1f KB  max=%.1f KB  mean=%.1f KB\n",
      bytes[1]/1024, med/1024, bytes[p95i]/1024, bytes[NR]/1024, (sum/NR)/1024
    printf "  bytes/event  %.0f B mean\n", (ev > 0 ? sum/ev : 0)
    printf "  full pages   %d of %d (%.0f%%) hit the %d-event limit\n",
      full, NR, (full * 100.0) / NR, '"$LIMIT"'
    printf "\n  A 30-room LRU of timeline pages would hold ~%.1f MB at the mean.\n",
      (sum/NR) * 30 / 1048576
  }'
