#!/bin/sh
# nFPM postremove. Debian `$1=purge` removes generated config (not a conffile).
# RPM has no purge; `$1=0` is a full uninstall and we leave /etc so a reinstall
# can keep store_key.
set -e

if [ "$1" = "purge" ]; then
	rm -rf /etc/axon-server
	cat >&2 <<'EOF'
Removed /etc/axon-server (including store_key).

The following were left in place. Delete them only if you want a
clean slate; a later install cannot read the leftover Postgres data
without that store_key.

  Postgres database and role:  axon
    sudo -u postgres dropdb axon
    sudo -u postgres dropuser axon

  Durable state:               /var/lib/axon-server
    sudo rm -rf /var/lib/axon-server

  Media cache:                 /var/cache/axon-server
    sudo rm -rf /var/cache/axon-server

  System user and group:       axon
    sudo userdel axon
    sudo groupdel axon 2>/dev/null || true
EOF
fi
