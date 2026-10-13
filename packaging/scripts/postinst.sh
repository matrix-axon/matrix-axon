#!/bin/sh
# nFPM postinstall for both Debian (`configure` / empty $2 on first install)
# and RPM (`1` install, `2+` upgrade).
set -e

CONFIG=/etc/axon-server/config.toml

# sqlx rejects `postgres://user@/db` ("empty host"). A host that starts with `/`
# is a Unix-socket directory and must be percent-encoded in the URL
# (`postgres://axon@%2Fvar%2Frun%2Fpostgresql/axon`). Peer auth still applies.
sqlx_socket_url() {
	dir=/var/run/postgresql
	if [ ! -S "$dir/.s.PGSQL.5432" ] && [ -S /run/postgresql/.s.PGSQL.5432 ]; then
		dir=/run/postgresql
	fi
	encoded=$(printf '%s' "$dir" | sed 's|/|%2F|g')
	printf 'postgres://axon@%s/axon' "$encoded"
}

is_systemd() {
	[ -d /run/systemd/system ]
}

ensure_user() {
	if command -v systemd-sysusers >/dev/null 2>&1; then
		systemd-sysusers /usr/lib/sysusers.d/axon-server.conf >/dev/null 2>&1 || true
	fi
	if ! getent passwd axon >/dev/null 2>&1; then
		if command -v adduser >/dev/null 2>&1; then
			adduser --system --group --quiet --no-create-home \
				--home /nonexistent --shell /usr/sbin/nologin \
				--gecos "Axon Matrix state layer" axon
		else
			useradd --system --no-create-home --home-dir /nonexistent \
				--shell /usr/sbin/nologin --user-group \
				--comment "Axon Matrix state layer" axon
		fi
	fi
}

ensure_dirs() {
	if command -v systemd-tmpfiles >/dev/null 2>&1; then
		systemd-tmpfiles --create /usr/lib/tmpfiles.d/axon-server.conf >/dev/null 2>&1 || true
	fi
	mkdir -p /etc/axon-server
	chown axon:axon /etc/axon-server
	chmod 0750 /etc/axon-server
}

# True when the postgres OS user can run a query on the local Unix socket.
local_postgres_ready() {
	getent passwd postgres >/dev/null 2>&1 || return 1
	if [ -S /var/run/postgresql/.s.PGSQL.5432 ] || [ -S /run/postgresql/.s.PGSQL.5432 ]; then
		:
	else
		return 1
	fi
	su -s /bin/sh postgres -c "psql -Atqc 'SELECT 1'" >/dev/null 2>&1
}

# True when OS user `axon` can connect to database `axon` via peer.
peer_connects() {
	su -s /bin/sh axon -c "psql -d axon -Atqc 'SELECT 1'" >/dev/null 2>&1
}

pg_scalar() {
	su -s /bin/sh postgres -c "psql -Atqc \"$1\""
}

provision_local_postgres() {
	# Role + database named `axon` so peer auth matches User=axon.
	# pgcrypto must be created as a superuser (baseline migration).
	if [ "$(pg_scalar "SELECT 1 FROM pg_roles WHERE rolname = 'axon'")" != "1" ]; then
		su -s /bin/sh postgres -c "psql -v ON_ERROR_STOP=1 -c 'CREATE ROLE axon LOGIN'" || return 1
	fi
	if [ "$(pg_scalar "SELECT 1 FROM pg_database WHERE datname = 'axon'")" != "1" ]; then
		su -s /bin/sh postgres -c "createdb -O axon axon" || return 1
	fi
	if ! su -s /bin/sh postgres -c "psql -v ON_ERROR_STOP=1 -d axon -c 'CREATE EXTENSION IF NOT EXISTS pgcrypto'"; then
		cat >&2 <<'EOF'
Could not CREATE EXTENSION pgcrypto in database axon.
On RHEL/Fedora, install postgresql-contrib and reconfigure:

  sudo dnf install postgresql-contrib
  then reconfigure this package (dpkg-reconfigure axon-server, or reinstall).
EOF
		return 1
	fi
}

# Run a query against the axon database. Prints stdout. Returns psql's exit
# status (not masked) so callers can tell failure from "no rows".
pg_axon_query() {
	su -s /bin/sh postgres -c "psql -d axon -v ON_ERROR_STOP=1 -Atqc \"$1\""
}

# True when the local `axon` database already holds pgcrypto'd account secrets
# from a previous install. Query failure is treated as "yes" so a transient
# error cannot mint a new store_key over existing ciphertext.
has_encrypted_account_secrets() {
	[ "$(pg_scalar "SELECT 1 FROM pg_database WHERE datname = 'axon'")" = "1" ] || return 1
	if ! tables=$(pg_axon_query "SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = 'accounts'"); then
		return 0
	fi
	[ "$tables" = "1" ] || return 1
	if ! rows=$(pg_axon_query "SELECT 1 FROM accounts WHERE access_token_encrypted IS NOT NULL OR oauth_refresh_token_encrypted IS NOT NULL LIMIT 1"); then
		return 0
	fi
	[ "$rows" = "1" ]
}

config_url() {
	f=${1:-$CONFIG}
	[ -f "$f" ] || return 1
	awk -F ' = ' '/^url = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$f"
}

config_store_key() {
	f=${1:-$CONFIG}
	[ -f "$f" ] || return 1
	awk -F ' = ' '/^store_key = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$f"
}

# True when database.url is the packaged Unix-socket URL (percent-encoded host).
config_uses_local_socket() {
	url=$(config_url) || return 1
	case $url in
	postgres://axon@%2Fvar%2Frun%2Fpostgresql/axon | postgres://axon@%2Frun%2Fpostgresql/axon | postgres://axon@%2Fvar%2Frun%2Fpostgresql/axon?* | postgres://axon@%2Frun%2Fpostgresql/axon?*)
		return 0
		;;
	esac
	return 1
}

# 0 = nothing to decrypt or the key works; 1 = pgcrypto wrong-key; 2 = probe error.
store_key_check() {
	if ! has_encrypted_account_secrets; then
		return 0
	fi
	key=$(config_store_key) || return 2
	[ -n "$key" ] || return 2
	# Key on stdin, not in su/psql argv. Capture stderr to classify the error.
	err=$(
		su -s /bin/sh postgres -c 'psql -d axon -v ON_ERROR_STOP=1 -Atq' 2>&1 <<SQL
SELECT pgp_sym_decrypt(COALESCE(access_token_encrypted, oauth_refresh_token_encrypted), '$key')
FROM accounts
WHERE access_token_encrypted IS NOT NULL OR oauth_refresh_token_encrypted IS NOT NULL
LIMIT 1;
SQL
	) || {
		if printf '%s\n' "$err" | grep -qi 'Wrong key or corrupt data'; then
			return 1
		fi
		return 2
	}
	return 0
}

print_leftover_database() {
	cat >&2 <<'EOF'

axon-server found an existing 'axon' Postgres database with encrypted
account data, but this install does not have a store_key that can read it.

That database is from a previous Axon install. A newly generated encryption
key cannot decrypt those tokens; starting the service would fail.

To keep the data, restore the old /etc/axon-server/config.toml (the
[sync] store_key must match) and run:

  sudo dpkg-reconfigure axon-server
  sudo systemctl enable --now axon-server

To start over, drop the leftover database (and optional SDK state) and
configure again:

  sudo -u postgres dropdb axon
  sudo rm -f /etc/axon-server/config.toml
  sudo rm -rf /var/lib/axon-server
  sudo dpkg-reconfigure axon-server

The package is installed; the service has not been started.
EOF
}

print_peer_failed() {
	cat >&2 <<'EOF'

The postgres OS user can reach the cluster, but role `axon` cannot
connect over the Unix socket (peer auth). Typical cause: pg_hba.conf
`local` lines use scram-sha-256 or md5 instead of `peer`.

Fix pg_hba.conf, reload Postgres, then:

  sudo dpkg-reconfigure axon-server

The package is installed; the service has not been started.
EOF
}

print_init_unreachable() {
	printf '%s\n' "$1" >&2
	cat >&2 <<'EOF'

Could not reach or migrate the database (init output above).
This is not necessarily a pg_hba/peer-auth problem; migrations and
schema readiness fail the same way.

The generated config was removed. After fixing the database:

  sudo dpkg-reconfigure axon-server

The package is installed; the service has not been started.
EOF
}

# Rewrite the packaged config: keep init's url + store_key, pin data paths so
# the systemd unit and `sudo -u axon axon-server --config …` share them, and
# document the socket URL. Paths are written into the file so they are not
# only Environment= overrides that the CLI would miss.
write_packaged_config() {
	cfg=$1
	url=$(config_url "$cfg") || return 1
	key=$(config_store_key "$cfg") || return 1
	[ -n "$url" ] && [ -n "$key" ] || return 1
	tmp=$(mktemp "$cfg.XXXXXX") || return 1
	trap 'rm -f "$tmp"' EXIT
	cat >"$tmp" <<EOF
# Generated by axon-server init on first package configure.
# Everything not set here uses built-in defaults; see axon.toml.example.

# Local packaged default: peer auth over the Unix socket. OS user \`axon\`
# is database role \`axon\`; there is no password. sqlx needs the socket
# directory percent-encoded as the URL host (a blank host is rejected).
# For a remote or passworded server, replace this url with
#   postgres://USER:PASSWORD@HOST:5432/DBNAME
# Create pgcrypto in that database once as a superuser, then:
#   systemctl restart axon-server
[database]
url = "$url"

# Symmetric key: encrypts access tokens at rest and passphrases the SDK store.
# Generated once — changing it orphans existing encrypted data. Keep it secret.
# Data dirs are pinned here (not only in the systemd unit) so CLI subcommands
# such as \`search reindex\` use the same trees.
[sync]
store_key = "$key"
data_dir = "/var/lib/axon-server/sync"

[search]
index_path = "/var/lib/axon-server/search"

[media]
cache_dir = "/var/cache/axon-server/media"
uploads_dir = "/var/lib/axon-server/uploads"
EOF
	chown axon:axon "$tmp"
	chmod 0600 "$tmp"
	mv "$tmp" "$cfg"
	trap - EXIT
}

journal_cursor() {
	journalctl -u axon-server -n 0 --show-cursor --no-pager 2>/dev/null |
		sed -n 's/^-- cursor: //p'
}

# Wait for the unit to print the one-time bootstrap URL *after* $1 (a journal
# cursor captured before start). Sleep first so Type=simple has time to log.
wait_for_bootstrap_url() {
	cursor=$1
	n=0
	while [ "$n" -lt 10 ]; do
		sleep 1
		if [ -n "$cursor" ]; then
			log=$(journalctl -u axon-server --after-cursor "$cursor" --no-pager -o cat 2>/dev/null || true)
		else
			log=$(journalctl -u axon-server --since "20 seconds ago" --no-pager -o cat 2>/dev/null || true)
		fi
		url=$(printf '%s\n' "$log" | sed -n 's/.*open \(http:\/\/[^ ]*\).*/\1/p' | tail -n 1)
		if [ -n "$url" ]; then
			printf '%s\n' "$url"
			return 0
		fi
		n=$((n + 1))
	done
	return 1
}

# Written once the operator has been told about the web client, so an upgrade
# from a release without one says so exactly once (ADR 0116).
WEB_NOTICE_STAMP=/var/lib/axon-server/.web-client-notice
DOC=/usr/share/doc/axon-server

mark_web_notice_shown() {
	mkdir -p /var/lib/axon-server 2>/dev/null || return 0
	: >"$WEB_NOTICE_STAMP" 2>/dev/null || true
}

# server_setting <key>: the value of <key> in the [server] table of the
# config, without quotes or a trailing comment. Empty when it is not set.
server_setting() {
	[ -f "$CONFIG" ] || return 0
	awk -v key="$1" '
		/^[[:space:]]*\[/ {
			in_server = ($0 ~ /^[[:space:]]*\[server\][[:space:]]*(#.*)?$/)
			next
		}
		in_server && $0 ~ ("^[[:space:]]*" key "[[:space:]]*=") {
			sub(/^[^=]*=[[:space:]]*/, "")
			sub(/[[:space:]]*(#.*)?$/, "")
			gsub(/^["\047]|["\047]$/, "")
			print
			exit
		}' "$CONFIG"
}

# unit_env <NAME>: an Environment= value from the unit or a drop-in, which
# outranks the config file. Empty when unset or when there is no systemd.
unit_env() {
	is_systemd || return 0
	systemctl show -p Environment --value axon-server 2>/dev/null |
		tr ' ' '\n' | sed -n "s/^$1=//p" | tail -n 1
}

# effective <key> <ENV NAME> <default>: what the service will use.
effective() {
	value=$(unit_env "$2")
	[ -n "$value" ] || value=$(server_setting "$1")
	[ -n "$value" ] || value=$3
	echo "$value"
}

# True when the operator has already switched the web client off.
web_client_disabled() {
	[ "$(effective web_client AXON_SERVER__WEB_CLIENT true)" = false ]
}

# Sets WEB_PORT, WEB_URL (an address to open on this machine) and WEB_HOST
# (empty when the server listens on loopback only, else the address it listens
# on). An operator may have widened `host` long before the server had a web
# client; telling them it is local-only would be false.
web_client_address() {
	WEB_PORT=$(effective port AXON_SERVER__PORT 8080)
	host=$(effective host AXON_SERVER__HOST 127.0.0.1)
	WEB_HOST=
	WEB_URL="http://127.0.0.1:$WEB_PORT/"
	case $host in
	127.* | localhost) ;;
	::1) WEB_URL="http://[::1]:$WEB_PORT/" ;;
	0.0.0.0) WEB_HOST=$host ;;
	::) WEB_HOST=$host WEB_URL="http://[::1]:$WEB_PORT/" ;;
	*:*) WEB_HOST=$host WEB_URL="http://[$host]:$WEB_PORT/" ;;
	*) WEB_HOST=$host WEB_URL="http://$host:$WEB_PORT/" ;;
	esac
}

# Who can reach the web client, in the operator's terms.
print_web_client_reach() {
	if [ -z "$WEB_HOST" ]; then
		echo "It is reachable from this machine only. Always open it as 127.0.0.1,"
		echo "not localhost: a browser treats those as two sites and keeps a"
		echo "separate sign-in for each."
	else
		echo "This server is configured to listen on $WEB_HOST, not only on this"
		echo "machine, and the web client shares the API's address and port: it is"
		echo "reachable from other machines wherever the API is. Always open it by"
		echo "the same address: a browser keeps a separate sign-in for each."
	fi
}

print_web_client_off() {
	echo "To turn the web client off, add to $CONFIG:"
	echo
	echo "  [server]"
	echo "  web_client = false"
	echo
	echo "then: sudo systemctl restart axon-server"
}

# The commands for whichever reverse proxy is already installed. Nothing here
# touches another service: its configuration is the operator's to change, and
# only the operator knows the hostname and the certificate (ADR 0116).
print_remote_access() {
	echo "To reach it from other devices, put TLS in front. Do this after the"
	echo "first sign-in: behind a proxy on this machine every request looks"
	echo "local, and the first-sign-in page is limited to local requests."
	echo
	found=0
	if command -v tailscale >/dev/null 2>&1; then
		found=1
		echo "  Tailscale (private to your tailnet, no certificate to manage):"
		echo "    sudo tailscale serve --bg http://127.0.0.1:$WEB_PORT"
		echo
	fi
	if command -v caddy >/dev/null 2>&1; then
		found=1
		echo "  Caddy (obtains the certificate itself):"
		echo "    sudo cp $DOC/examples/axon-server.caddy /etc/caddy/"
		echo "    sudoedit /etc/caddy/axon-server.caddy    # set your hostname"
		echo "    grep -qxF 'import axon-server.caddy' /etc/caddy/Caddyfile ||"
		# printf, not echo: dash's echo would expand the \n meant for the operator.
		printf '%s\n' "      printf '\\nimport axon-server.caddy\\n' | sudo tee -a /etc/caddy/Caddyfile >/dev/null"
		echo "    sudo caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile"
		echo "    sudo systemctl reload caddy"
		echo
	fi
	if command -v nginx >/dev/null 2>&1; then
		found=1
		echo "  nginx:"
		if [ -d /etc/nginx/sites-available ]; then
			echo "    sudo cp $DOC/examples/axon-server.nginx.conf /etc/nginx/sites-available/axon-server"
			echo "    sudoedit /etc/nginx/sites-available/axon-server    # hostname and certificate paths"
			echo "    sudo ln -s /etc/nginx/sites-available/axon-server /etc/nginx/sites-enabled/"
		else
			echo "    sudo cp $DOC/examples/axon-server.nginx.conf /etc/nginx/conf.d/axon-server.conf"
			echo "    sudoedit /etc/nginx/conf.d/axon-server.conf    # hostname and certificate paths"
		fi
		echo "    sudo nginx -t && sudo systemctl reload nginx"
		echo
	fi
	if command -v apache2 >/dev/null 2>&1; then
		found=1
		echo "  Apache:"
		echo "    sudo cp $DOC/examples/axon-server.apache.conf /etc/apache2/sites-available/axon-server.conf"
		echo "    sudoedit /etc/apache2/sites-available/axon-server.conf    # hostname and certificate paths"
		echo "    sudo a2enmod ssl proxy proxy_http proxy_wstunnel && sudo a2ensite axon-server"
		echo "    sudo apachectl configtest && sudo systemctl reload apache2"
		echo
	elif command -v httpd >/dev/null 2>&1; then
		found=1
		echo "  Apache:"
		echo "    sudo cp $DOC/examples/axon-server.apache.conf /etc/httpd/conf.d/axon-server.conf"
		echo "    sudoedit /etc/httpd/conf.d/axon-server.conf    # hostname and certificate paths"
		echo "    sudo apachectl configtest && sudo systemctl reload httpd"
		echo
	fi
	if [ "$found" = 0 ]; then
		echo "  No Tailscale, Caddy, nginx or Apache was found on this machine."
		echo "  Any of them will do; ready-made site files are in $DOC/examples/."
		echo
	fi
	if [ "$WEB_PORT" != 8080 ]; then
		echo "The site files assume port 8080; this server uses $WEB_PORT, so change"
		echo "that in the file you copy."
		echo
	fi
	echo "Each step is explained in $DOC/README.Debian."
}

print_first_run_help() {
	cursor=$1
	web_client_address
	echo
	echo "axon-server is installed and the service is enabled."
	echo
	echo "The web client is on, at:"
	echo
	echo "  $WEB_URL"
	echo
	print_web_client_reach
	echo
	url=
	if is_systemd; then
		url=$(wait_for_bootstrap_url "$cursor" || true)
	fi
	if [ -n "$url" ]; then
		echo "To sign in for the first time, open this one-time URL in a browser"
		echo "on this machine and choose \"Create bearer token\":"
		echo
		echo "  $url"
		echo
		echo "The next page shows the token, once. If it offers \"Open web client\","
		echo "that button signs this browser in with nothing to paste."
		echo
	else
		echo "To sign in for the first time, open the one-time bootstrap URL from"
		echo "the journal in a browser on this machine:"
		echo
		echo "  journalctl -u axon-server -e --no-pager | grep -i bootstrap"
		echo
	fi
	echo "With no browser on this machine, forward the port from one that has"
	echo "one, then open the same URL there:"
	echo
	echo "  ssh -L $WEB_PORT:127.0.0.1:$WEB_PORT <this machine>"
	echo
	echo "For every later sign-in (another browser, a phone, axon-tui), mint a"
	echo "token and paste it into the client:"
	echo
	echo "  sudo -u axon axon-server token issue --label laptop"
	echo
	print_web_client_off
	echo
	print_remote_access
	mark_web_notice_shown
}

# An upgrade from a release whose server had no web client. Said once.
print_web_client_upgrade_notice() {
	[ -e "$WEB_NOTICE_STAMP" ] && return 0
	mark_web_notice_shown
	web_client_disabled && return 0
	web_client_address
	echo
	echo "New in this release: axon-server now serves the web client itself, at"
	echo
	echo "  $WEB_URL"
	echo
	print_web_client_reach
	echo
	echo "It needs a token to sign in. Mint one with:"
	echo
	echo "  sudo -u axon axon-server token issue --label browser"
	echo
	print_web_client_off
	echo
	print_remote_access
}

print_byo_postgres() {
	cat >&2 <<'EOF'
axon-server is installed but no local Postgres cluster was reachable.
After the database exists (pgcrypto created once as a superuser):

  axon-server init --non-interactive \
    --config /etc/axon-server/config.toml \
    --database-url 'postgres://USER:PASSWORD@HOST:5432/DB' \
    --no-token
  chown axon:axon /etc/axon-server/config.toml
  chmod 0600 /etc/axon-server/config.toml

init writes only url and store_key. Add these so the unit and CLI
share the same trees (put data_dir under the existing [sync] section;
do not add a second [sync] table):

  data_dir = "/var/lib/axon-server/sync"

  [search]
  index_path = "/var/lib/axon-server/search"

  [media]
  cache_dir = "/var/cache/axon-server/media"
  uploads_dir = "/var/lib/axon-server/uploads"

  systemctl enable --now axon-server

See /usr/share/doc/axon-server/README.Debian.
EOF
}

stop_unit() {
	if is_systemd; then
		systemctl stop axon-server.service >/dev/null 2>&1 || true
	fi
}

# $1=1 enable+start (fresh config written by this script). $1=0 restart only
# if the admin already enabled the unit — do not re-enable on upgrade.
start_unit() {
	enable=$1
	is_systemd || return 0
	systemctl daemon-reload
	if [ "$enable" = 1 ]; then
		systemctl enable axon-server.service
		systemctl start axon-server.service
	elif systemctl is-enabled --quiet axon-server.service 2>/dev/null; then
		systemctl restart axon-server.service
	else
		echo "axon-server is installed but the unit is not enabled (apt remove disables it; upgrades preserve that)." >&2
		echo "Start it with: sudo systemctl enable --now axon-server" >&2
	fi
}

# Shared by first install, upgrade, and dpkg-reconfigure.
configure() {
	ensure_user
	ensure_dirs

	if [ -f "$CONFIG" ]; then
		if config_uses_local_socket && local_postgres_ready; then
			store_key_check && sk=0 || sk=$?
			if [ "$sk" = 1 ]; then
				stop_unit
				print_leftover_database
				return 0
			fi
			if [ "$sk" = 2 ]; then
				echo "axon-server: could not verify store_key against the database (not treating this as a leftover install); leaving the unit as-is." >&2
			fi
		fi
		start_unit 0
		print_web_client_upgrade_notice
		return 0
	fi

	if ! local_postgres_ready; then
		print_byo_postgres
		return 0
	fi

	if has_encrypted_account_secrets; then
		stop_unit
		print_leftover_database
		return 0
	fi

	if ! provision_local_postgres; then
		return 0
	fi
	if ! peer_connects; then
		print_peer_failed
		return 0
	fi
	# Peer auth requires the connecting OS user to match the role, so init
	# runs as `axon`, not root. --no-token: first credential is the unit's
	# web bootstrap (AXON_SERVER__BOOTSTRAP_WEB_AUTO).
	socket_url=$(sqlx_socket_url)
	# init prints Docker-oriented next-steps; keep write/connect lines only.
	init_out=$(su -s /bin/sh axon -c "axon-server init --non-interactive --config '$CONFIG' --database-url '$socket_url' --no-token" 2>&1) || {
		echo "$init_out" >&2
		return 1
	}
	echo "$init_out" | grep -E 'Wrote configuration|connected|not reachable' || true
	if echo "$init_out" | grep -q 'not reachable'; then
		# init writes the file before probing; remove it so the next
		# configure retries first-install (enable + start), not start_unit 0.
		rm -f "$CONFIG"
		print_init_unreachable "$init_out"
		return 0
	fi
	chmod 0600 "$CONFIG"
	chown axon:axon "$CONFIG"
	write_packaged_config "$CONFIG"
	cursor=$(journal_cursor || true)
	start_unit 1
	print_first_run_help "$cursor"
}

# Debian first install, upgrade, and dpkg-reconfigure; RPM install and upgrade.
if [ "$1" = "configure" ] || [ "$1" = "1" ] || [ "$1" -ge 2 ] 2>/dev/null; then
	configure
	exit 0
fi
