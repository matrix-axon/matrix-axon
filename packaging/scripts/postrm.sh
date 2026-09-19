#!/bin/sh
# nFPM postremove. Debian `$1=purge` removes generated config (not a conffile).
# RPM has no purge; `$1=0` is a full uninstall and we leave /etc so a reinstall
# can keep store_key.
set -e

if [ "$1" = "purge" ]; then
	rm -rf /etc/axon-server
fi
