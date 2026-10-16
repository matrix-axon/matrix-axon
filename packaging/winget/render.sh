#!/usr/bin/env bash
# Fill the manifest templates in this directory from a GitHub Release and
# write them in the winget-pkgs layout:
#
#   TAG=v0.1.10 packaging/winget/render.sh OUT_DIR [axon-server axon-tui axon]
#
# OUT_DIR gets manifests/m/MatrixAxon/<Package>/<version>/ for each app named
# (all three by default). Assets are downloaded with `gh` and hashed here, so
# the hash is of the bytes users will download. Dry run, no submission: see
# packaging/winget/README.md for what to do with the result.
set -euo pipefail

repo=${REPO:-matrix-axon/matrix-axon}
here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)

if [ -z "${TAG:-}" ]; then
	echo "TAG is required (the release tag, for example v0.1.10)" >&2
	exit 2
fi
if ! printf '%s' "$TAG" | grep -Eq '^v[0-9]+(\.[0-9]+)+$'; then
	echo "refusing $TAG: only stable vX.Y.Z tags are published to winget" >&2
	exit 1
fi
out=${1:?usage: TAG=vX.Y.Z render.sh OUT_DIR [app...]}
shift
apps=("$@")
[ ${#apps[@]} -gt 0 ] || apps=(axon-server axon-tui axon)

version=${TAG#v}
release_date=$(gh release view "$TAG" --repo "$repo" --json publishedAt -q '.publishedAt[:10]')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sha256_file() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -- "$1" | awk '{print toupper($1)}'
	else
		shasum -a 256 -- "$1" | awk '{print toupper($1)}'
	fi
}

# A manifest that is not valid YAML only fails later, inside `winget validate`
# or the submission pipeline. Parse it here with whatever is installed: ruby
# or python3 with PyYAML. With neither, say so and carry on.
yaml_parser=
if command -v ruby >/dev/null 2>&1; then
	yaml_parser=ruby
elif python3 -c 'import yaml' >/dev/null 2>&1; then
	yaml_parser=python3
else
	echo "warning: neither ruby nor python3 with PyYAML found; not parsing the manifests as YAML" >&2
fi

check_yaml() {
	case "$yaml_parser" in
	ruby) ruby -rdate -ryaml -e 'YAML.safe_load(File.read(ARGV[0]), permitted_classes: [Date])' "$1" ;;
	python3) python3 -I -c 'import sys, yaml; yaml.safe_load(open(sys.argv[1], encoding="utf-8"))' "$1" ;;
	*) return 0 ;;
	esac
}

for app in "${apps[@]}"; do
	case "$app" in
	axon-server) id=MatrixAxon.AxonServer pkg=AxonServer asset=axon-server-windows.zip ;;
	axon-tui) id=MatrixAxon.AxonTui pkg=AxonTui asset=axon-tui-windows.zip ;;
	axon) id=MatrixAxon.Axon pkg=Axon asset="Axon_${version}_x64-setup.exe" ;;
	*)
		echo "unknown app: $app (axon-server, axon-tui or axon)" >&2
		exit 2
		;;
	esac

	gh release download "$TAG" --repo "$repo" --pattern "$asset" --dir "$work" --clobber
	[ -s "$work/$asset" ] || {
		echo "release $TAG has no $asset" >&2
		exit 1
	}
	sha=$(sha256_file "$work/$asset")

	dest="$out/manifests/m/MatrixAxon/$pkg/$version"
	mkdir -p "$dest"
	for tmpl in "$here/$app/$id".*yaml.tmpl; do
		name=$(basename "$tmpl" .tmpl)
		sed -e "s|@@VERSION@@|$version|g" \
			-e "s|@@TAG@@|$TAG|g" \
			-e "s|@@SHA256@@|$sha|g" \
			-e "s|@@RELEASE_DATE@@|$release_date|g" \
			"$tmpl" >"$dest/$name"
	done
	if grep -rq '@@' "$dest"; then
		echo "unfilled placeholder in $dest" >&2
		exit 1
	fi
	for manifest in "$dest"/*.yaml; do
		if ! check_yaml "$manifest" 2>"$work/yaml.err"; then
			echo "$manifest is not valid YAML:" >&2
			head -n 1 "$work/yaml.err" >&2
			exit 1
		fi
	done
	echo "$id $version  $asset  sha256=$sha  -> $dest"
done
