#!/bin/sh
# nFPM preremove: Debian `$1=remove` / RPM `$1=0` (uninstall).
# Config stays until Debian purge (see postrm.sh). /var/lib/axon-server is
# never removed automatically.
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
