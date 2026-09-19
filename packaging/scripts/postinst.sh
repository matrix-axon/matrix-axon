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

# Rewrite the URL we shipped in 0.1.0-1 before sqlx-empty-host was known.
fix_empty_host_url() {
	[ -f "$CONFIG" ] || return 0
	if grep -q 'url = "postgres://axon@/axon' "$CONFIG"; then
		url=$(sqlx_socket_url)
		tmp=$(mktemp)
		sed "s|^url = \"postgres://axon@/axon[^\"]*\"|url = \"$url\"|" "$CONFIG" >"$tmp"
		chown axon:axon "$tmp"
		chmod 0600 "$tmp"
		mv "$tmp" "$CONFIG"
	fi
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
	# systemd-sysusers may still set HOME to /home/axon. Keep the passwd
	# home off /home so ProtectHome=yes is not fighting sqlx's .pgpass open.
	if command -v usermod >/dev/null 2>&1; then
		usermod --home /nonexistent axon >/dev/null 2>&1 || true
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
		su -s /bin/sh postgres -c "psql -v ON_ERROR_STOP=1 -c 'CREATE ROLE axon LOGIN'"
	fi
	if [ "$(pg_scalar "SELECT 1 FROM pg_database WHERE datname = 'axon'")" != "1" ]; then
		su -s /bin/sh postgres -c "createdb -O axon axon"
	fi
	su -s /bin/sh postgres -c "psql -v ON_ERROR_STOP=1 -d axon -c 'CREATE EXTENSION IF NOT EXISTS pgcrypto'"
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
	[ -f "$CONFIG" ] || return 1
	awk -F ' = ' '/^url = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$CONFIG"
}

config_store_key() {
	[ -f "$CONFIG" ] || return 1
	awk -F ' = ' '/^store_key = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$CONFIG"
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

# True when there is nothing to decrypt, or the config store_key decrypts a row.
store_key_unlocks_db() {
	has_encrypted_account_secrets || return 0
	key=$(config_store_key) || return 1
	[ -n "$key" ] || return 1
	su -s /bin/sh postgres -c "psql -d axon -v ON_ERROR_STOP=1 -Atqc \"SELECT pgp_sym_decrypt(COALESCE(access_token_encrypted, oauth_refresh_token_encrypted), '$key') FROM accounts WHERE access_token_encrypted IS NOT NULL OR oauth_refresh_token_encrypted IS NOT NULL LIMIT 1\"" >/dev/null 2>&1
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

# Replace init's generic URL comment with packaged peer-auth / remote-DB notes.
# Values are copied through; the socket URL has no characters that need quoting.
rewrite_packaged_comments() {
	cfg=$1
	url=$(awk -F ' = ' '/^url = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$cfg")
	key=$(awk -F ' = ' '/^store_key = / { gsub(/^"|"$/, "", $2); print $2; exit }' "$cfg")
	if [ -z "$url" ] || [ -z "$key" ]; then
		echo "axon-server: could not parse $cfg after init; leaving comments unchanged" >&2
		return 0
	fi
	tmp=$(mktemp)
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
[sync]
store_key = "$key"
EOF
	chown axon:axon "$tmp"
	chmod 0600 "$tmp"
	mv "$tmp" "$cfg"
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

print_first_run_help() {
	cursor=$1
	echo
	echo "axon-server is installed and the service is enabled."
	echo "It listens on http://127.0.0.1:8080 (loopback only)."
	echo
	url=
	if is_systemd; then
		url=$(wait_for_bootstrap_url "$cursor" || true)
	fi
	if [ -n "$url" ]; then
		echo "Create the first client credential by opening this one-time URL"
		echo "in a browser on this machine:"
		echo
		echo "  $url"
		echo
	else
		echo "Create the first client credential from the one-time bootstrap URL"
		echo "in the journal (loopback only):"
		echo
		echo "  journalctl -u axon-server -e --no-pager | grep -i bootstrap"
		echo
	fi
	echo "Or mint a token from the CLI:"
	echo
	echo "  sudo -u axon axon-server --config /etc/axon-server/config.toml \\"
	echo "    token issue --label first"
	echo
	echo "Point axon-tui or another client at http://127.0.0.1:8080 with that token."
	echo "See /usr/share/doc/axon-server/README.Debian."
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
	fi
}

# Shared by first install, upgrade, and dpkg-reconfigure.
configure() {
	ensure_user
	ensure_dirs
	fix_empty_host_url

	if [ -f "$CONFIG" ]; then
		if config_uses_local_socket && local_postgres_ready && ! store_key_unlocks_db; then
			stop_unit
			print_leftover_database
			return 0
		fi
		start_unit 0
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

	provision_local_postgres
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
		print_peer_failed
		return 0
	fi
	chmod 0600 "$CONFIG"
	chown axon:axon "$CONFIG"
	rewrite_packaged_comments "$CONFIG"
	cursor=$(journal_cursor || true)
	start_unit 1
	print_first_run_help "$cursor"
}

# Debian first install, upgrade, and dpkg-reconfigure; RPM install and upgrade.
if [ "$1" = "configure" ] || [ "$1" = "1" ] || [ "$1" -ge 2 ] 2>/dev/null; then
	configure
	exit 0
fi
