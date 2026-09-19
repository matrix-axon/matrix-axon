#!/bin/sh
# nFPM preremove: Debian `$1=remove` / RPM `$1=0` (uninstall).
# Leave config and /var/lib/axon-server in place (store_key + crypto store).
set -e

stop_unit() {
	if [ -d /run/systemd/system ]; then
		systemctl stop axon-server.service >/dev/null 2>&1 || true
		systemctl disable axon-server.service >/dev/null 2>&1 || true
	fi
}

if [ "$1" = "remove" ] || [ "$1" = "0" ]; then
	stop_unit
fi
