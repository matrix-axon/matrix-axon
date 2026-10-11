#!/usr/bin/env bash
# Boot an axon-server built with `--features embed-web` and check, over real
# HTTP, that it serves the bundle it was built from under the ADR 0116 rules.
#
# Usage: DATABASE_URL=postgres://... scripts/embed-web-smoke.sh <binary> <dist>
#
# DATABASE_URL must name an empty throwaway database the role can create
# pgcrypto in. Sync state, search index and media go to a temp directory, and
# the server gets an explicit --config, so no platform or system config file
# and no real data directory is read or written.
#
# What unit tests cannot say and this can: that the binary carries *this*
# bundle, byte for byte, and that every kind of file Vite emitted gets a real
# content type. Responses carry `nosniff`, so a new extension that fell through
# to application/octet-stream would not load in a browser.
set -euo pipefail

if [ "$#" -ne 2 ] || [ -z "${DATABASE_URL:-}" ]; then
	echo "usage: DATABASE_URL=postgres://... $0 <axon-server binary> <dist directory>" >&2
	exit 2
fi
bin=$1
dist=$2
port=${PORT:-18473}
base="http://127.0.0.1:$port"

work=$(mktemp -d)
server_pid=
cleanup() {
	if [ -n "$server_pid" ]; then
		kill "$server_pid" 2>/dev/null || true
		wait "$server_pid" 2>/dev/null || true
	fi
	rm -rf "$work"
}
trap cleanup EXIT

cat >"$work/config.toml" <<TOML
[server]
host = "127.0.0.1"
port = $port

[database]
url = "$DATABASE_URL"

[sync]
store_key = "embed-web-smoke-throwaway-key"
data_dir = "$work/sync"

[search]
index_path = "$work/search"

[media]
cache_dir = "$work/media"
uploads_dir = "$work/uploads"
TOML

# Config comes from the file alone: an AXON_* or DATABASE_URL in the caller's
# environment would outrank it.
env -i PATH="$PATH" HOME="$work" "$bin" --config "$work/config.toml" >"$work/server.log" 2>&1 &
server_pid=$!

fail() {
	echo "FAIL: $*" >&2
	echo "--- server log ---" >&2
	tail -n 40 "$work/server.log" >&2 || true
	exit 1
}

ready=0
for _ in $(seq 1 60); do
	if curl -fsS "$base/healthz" >/dev/null 2>&1; then
		ready=1
		break
	fi
	kill -0 "$server_pid" 2>/dev/null || fail "the server exited before it was ready"
	sleep 1
done
[ "$ready" = 1 ] || fail "the server did not answer /healthz within 60s"

# header <name> <url> [curl args]: one response header, lowercased name.
header() {
	name=$1
	shift
	curl -sS -o /dev/null -D - "$@" | tr -d '\r' |
		awk -v n="$name" 'BEGIN { FS = ": " } tolower($1) == n { sub(/^[^:]*: /, ""); print; exit }'
}
status() {
	curl -sS -o /dev/null -w '%{http_code}' "$@"
}

grep -q "web client served at" "$work/server.log" || fail "the startup log does not say the web client is served"

# Every file in the bundle: served as itself, with a real type and the right
# cache rule for where it lives.
files=0
while IFS= read -r file; do
	path=${file#"$dist"}
	files=$((files + 1))
	[ "$(status "$base$path")" = 200 ] || fail "$path is not 200"
	curl -sS "$base$path" | cmp -s - "$file" || fail "$path does not match the file in dist"
	type=$(header content-type "$base$path")
	case $type in
	"" | application/octet-stream*) fail "$path has content type '$type'; add its extension to content_type() in crates/axon-api/src/web.rs" ;;
	esac
	cache=$(header cache-control "$base$path")
	case $path in
	/assets/*) [ "$cache" = "public, max-age=31536000, immutable" ] || fail "$path cache-control is '$cache'" ;;
	*) [ "$cache" = "no-cache" ] || fail "$path cache-control is '$cache'" ;;
	esac
done < <(find "$dist" -type f | sort)
[ "$files" -gt 0 ] || fail "no files under $dist"

# Client-side routes fall back to index.html.
for path in / /rooms/anything /oauth/callback; do
	curl -sS "$base$path" | cmp -s - "$dist/index.html" || fail "$path is not index.html"
done

# A chunk this build does not have is an uncacheable 404 (ADR 0087).
[ "$(status "$base/assets/index-00000000.js")" = 404 ] || fail "a missing asset is not 404"
[ "$(header cache-control "$base/assets/index-00000000.js")" = "no-store" ] || fail "a missing asset is cacheable"

# Revalidation.
etag=$(header etag "$base/version.json")
[ -n "$etag" ] || fail "version.json has no ETag"
[ "$(status -H "If-None-Match: $etag" "$base/version.json")" = 304 ] || fail "a matching If-None-Match is not 304"

# Compression is negotiated for the bundle as it is for JSON.
[ "$(header content-encoding -H 'Accept-Encoding: gzip' "$base/")" = gzip ] || fail "index.html is not gzip-encoded on request"

# The API keeps every path that is its own.
[ "$(status "$base/v1/accounts")" = 401 ] || fail "/v1/accounts without a token is not 401"
[ "$(status "$base/v1/not-a-route")" = 401 ] || fail "an unknown /v1 path fell through to the web client"
[ "$(status -X POST "$base/")" = 405 ] || fail "POST / is not 405"

echo "ok: $files bundle files served correctly by $bin"
