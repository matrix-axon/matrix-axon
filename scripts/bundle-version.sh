#!/usr/bin/env bash
# Keeps the web client's version, which every shell bundle carries, equal to the
# release version. The release version is axon-server's Cargo version
# (scripts/release-version.sh), the one release-plz bumps and tags.
#
# `clients/web/package.json` is what the web client reports (vite.config.ts) and
# what `tauri.conf.json` reads by path, so it names the .dmg, the .msi, the
# AppImage, the iOS and Mac App Store builds (CFBundleShortVersionString) and
# the Android versionName. Left alone it is whatever was last committed.
#
#   scripts/bundle-version.sh sync
#       Write the release version into package.json.
#
#   scripts/bundle-version.sh check
#       Fail unless package.json already holds the release version.
#
# BUNDLE_PACKAGE_JSON points both at another package.json (the test uses it).
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)
package_json=${BUNDLE_PACKAGE_JSON:-$root/clients/web/package.json}

usage() {
	sed -n '2,17p' "$0" >&2
}

current() {
	node -e '
		const fs = require("node:fs")
		process.stdout.write(String(JSON.parse(fs.readFileSync(process.argv[1], "utf8")).version))
	' "$package_json"
}

want=$("$root/scripts/release-version.sh" print)

case "${1:-}" in
sync)
	node -e '
		const fs = require("node:fs")
		const pkg = JSON.parse(fs.readFileSync(process.argv[1], "utf8"))
		pkg.version = process.argv[2]
		fs.writeFileSync(process.argv[1], JSON.stringify(pkg, null, 2) + "\n")
	' "$package_json" "$want"
	echo "Bundle version set to $want."
	;;
check)
	have=$(current)
	if [ "$have" != "$want" ]; then
		echo "::error::clients/web/package.json says $have but the release version is $want (Cargo.toml)." >&2
		echo "Run scripts/bundle-version.sh sync and commit the result." >&2
		exit 1
	fi
	echo "package.json matches the release version $want"
	;;
-h | --help)
	usage
	;;
*)
	usage
	exit 2
	;;
esac
