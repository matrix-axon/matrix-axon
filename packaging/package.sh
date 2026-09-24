#!/bin/bash
# Build axon-server .deb and .rpm via nFPM.
# Usage: ARCH=amd64|arm64 packaging/package.sh [deb|rpm|all]
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)
cd "$root"

format=${1:-all}
export ARCH="${ARCH:-amd64}"
export AXON_SERVER_BIN="${AXON_SERVER_BIN:-"$root/target/release/axon-server"}"
export VERSION="${VERSION:-$("$root/scripts/release-version.sh" print)}"

if [ ! -x "$AXON_SERVER_BIN" ]; then
	echo "missing binary: $AXON_SERVER_BIN (cargo build --release -p axon-server)" >&2
	exit 1
fi

if ! command -v nfpm >/dev/null 2>&1; then
	echo "nfpm is not on PATH. Install https://github.com/goreleaser/nfpm/releases" >&2
	exit 1
fi

out=${PACKAGING_OUT:-"$root/target/nfpm"}
mkdir -p "$out"

pack() {
	nfpm package --config "$root/packaging/nfpm.yaml" --packager "$1" --target "$out"
}

case $format in
	deb) pack deb ;;
	rpm) pack rpm ;;
	all)
		pack deb
		pack rpm
		;;
	*)
		echo "usage: $0 [deb|rpm|all]" >&2
		exit 1
		;;
esac

echo "wrote packages under $out"
