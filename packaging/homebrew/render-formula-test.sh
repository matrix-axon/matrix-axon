#!/usr/bin/env bash
# Render the Homebrew formula against fixture checksums and check the locks
# the caveats have to keep. No network and no brew.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
render=$root/packaging/homebrew/render-formula.sh
publish=$root/packaging/homebrew/publish-tap.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sha_silicon=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
sha_intel=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
sha_linux=cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
out=$work/axon-server.rb

"$render" \
	--tag v1.2.3 \
	--sha-macos-silicon "$sha_silicon" \
	--sha-macos-intel "$sha_intel" \
	--sha-linux-x86_64 "$sha_linux" \
	--out "$out"

if grep -q '@@' "$out"; then
	echo "placeholder left in rendered formula" >&2
	exit 1
fi

grep -q 'version "1.2.3"' "$out"
grep -Fq "releases/download/v1.2.3/axon-server-macos-silicon.zip" "$out"
grep -Fq "releases/download/v1.2.3/axon-server-macos-intel.zip" "$out"
grep -Fq "releases/download/v1.2.3/axon-server-linux.zip" "$out"
grep -q "sha256 \"$sha_silicon\"" "$out"
grep -q "sha256 \"$sha_intel\"" "$out"
grep -q "sha256 \"$sha_linux\"" "$out"

grep -Fq 'assert_match "axon-server #{version} "' "$out"
grep -Fq 'url "https://github.com/matrix-axon/matrix-axon.git"' "$out"
if grep -Eq 'url "[^"]*releases/latest' "$out"; then
	echo "livecheck follows releases/latest, which can be a beta" >&2
	exit 1
fi

if grep -Eq 'depends_on[[:space:]]+"tailscale"' "$out"; then
	echo "formula depends on tailscale" >&2
	exit 1
fi
if grep -Eq 'depends_on[[:space:]]+"postgresql@16"' "$out"; then
	echo "formula depends on postgresql@16" >&2
	exit 1
fi

# The tildes are literal path text in the formula, not this script's $HOME.
# shellcheck disable=SC2088
for needle in \
	'~/Library/Application Support/axon-server/config.toml' \
	'~/.config/axon-server/config.toml' \
	'postgresql@16' \
	'CREATE EXTENSION IF NOT EXISTS pgcrypto' \
	"CREATE ROLE axon LOGIN PASSWORD 'axon'" \
	'postgres://axon:axon@127.0.0.1:5432/axon' \
	'brew services start axon-server' \
	'tailscale serve --bg http://127.0.0.1:8080' \
	'tailscale-app' \
	'pg_isready' \
	'--print-token' \
	'ubuntu-latest'; do
	if ! grep -F -q -e "$needle" "$out"; then
		echo "rendered formula is missing: $needle" >&2
		exit 1
	fi
done

if ! command -v ruby >/dev/null 2>&1; then
	echo "ruby is required to syntax-check the formula and print its caveats" >&2
	exit 1
fi

ruby -c "$out"

# Load the formula with the Homebrew DSL stubbed out and print caveats.
# The SQL heredoc terminator has to land in column 0 or a pasted install
# block never closes.
ruby - "$out" "$work/caveats-mac.txt" "$work/caveats-linux.txt" <<'RUBY'
module OS
  def self.mac?
    ENV.fetch("AXON_FORMULA_OS") == "mac"
  end

  def self.linux?
    !mac?
  end
end

class Formula
  class << self
    def desc(*) end
    def homepage(*) end
    def license(*) end
    def version(*) end
    def livecheck(*) end
    def on_macos(&block) = block.call
    def on_linux(&block) = block.call
    def on_arm(&block) = block.call
    def on_intel(&block) = block.call
    def url(*) end
    def sha256(*) end
    def depends_on(*) end
    def service(*) end
    def test(*) end
  end
end

load ARGV[0]
formula = ObjectSpace.each_object(Class).find { |klass| klass < Formula && klass.name == "AxonServer" }
raise "AxonServer formula did not load" unless formula

ENV["AXON_FORMULA_OS"] = "mac"
File.write(ARGV[1], formula.new.caveats)
ENV["AXON_FORMULA_OS"] = "linux"
File.write(ARGV[2], formula.new.caveats)
RUBY

grep -Fq 'macOS: ~/Library/Application Support/axon-server/config.toml' "$work/caveats-mac.txt"
grep -Fq 'Linux: ~/.config/axon-server/config.toml' "$work/caveats-mac.txt"
grep -Fq 'GLIBC' "$work/caveats-linux.txt"
if grep -Fq 'GLIBC' "$work/caveats-mac.txt"; then
	echo "macOS caveats mention the Linux glibc floor" >&2
	exit 1
fi

# Starting the service before init is a keep_alive restart loop, so the
# first start the caveats show has to come after the first init.
first_init=$(grep -n 'axon-server init' "$work/caveats-mac.txt" | head -n 1 | cut -d: -f1)
first_start=$(grep -n 'brew services start axon-server' "$work/caveats-mac.txt" | head -n 1 | cut -d: -f1)
if [ -z "$first_init" ] || [ -z "$first_start" ] || [ "$first_start" -le "$first_init" ]; then
	echo "caveats show brew services start before axon-server init" >&2
	exit 1
fi

# Rebuild the copy-paste block and syntax-check it. The heredoc terminator
# is the line whose entire contents are SQL.
awk '
  /^PG="\$\(brew --prefix postgresql@16\)\/bin"$/ { capture = 1 }
  capture { print }
  capture && /^brew services start axon-server$/ { exit }
' "$work/caveats-mac.txt" >"$work/local-postgres.sh"

if [ ! -s "$work/local-postgres.sh" ]; then
	echo "could not find the local Postgres block in the caveats" >&2
	exit 1
fi
# `sh -n` exits 0 on an unclosed heredoc (bash only warns), so it cannot
# catch an indented terminator. Look for the terminator line itself, then
# parse with bash and treat any warning as a failure.
if ! grep -qx 'SQL' "$work/local-postgres.sh"; then
	echo "the SQL heredoc terminator is not alone in column 0" >&2
	exit 1
fi
if ! bash -n "$work/local-postgres.sh" 2>"$work/local-postgres.err" || [ -s "$work/local-postgres.err" ]; then
	echo "the local Postgres block does not parse cleanly:" >&2
	cat "$work/local-postgres.err" >&2
	exit 1
fi

# Uppercase checksums are normalized. A tag that is not a release ref is refused.
upper=$(printf 'A%.0s' $(seq 1 64))
"$render" \
	--tag v9.9.9 \
	--sha-macos-silicon "$upper" \
	--sha-macos-intel "$sha_intel" \
	--sha-linux-x86_64 "$sha_linux" \
	--out "$work/upper.rb"
grep -q "sha256 \"$(printf 'a%.0s' $(seq 1 64))\"" "$work/upper.rb"

reject() {
	if "$@" >"$work/rejected.out" 2>"$work/rejected.err"; then
		echo "expected failure: $*" >&2
		exit 1
	fi
}

reject "$render" --tag 'v1.2.3;touch /tmp/pwned' \
	--sha-macos-silicon "$sha_silicon" \
	--sha-macos-intel "$sha_intel" \
	--sha-linux-x86_64 "$sha_linux" \
	--out "$work/bad.rb"
reject "$render" --tag v1.2.3 \
	--sha-macos-silicon abc \
	--sha-macos-intel "$sha_intel" \
	--sha-linux-x86_64 "$sha_linux" \
	--out "$work/bad.rb"
# Prerelease tags are not rendered: the tap has one formula, and Homebrew
# cannot order beta-1 against 1.2.3.
for tag in beta-1 alpha-2 beta-v0.0.4 v1.2.3-rc1 v1; do
	reject "$render" --tag "$tag" \
		--sha-macos-silicon "$sha_silicon" \
		--sha-macos-intel "$sha_intel" \
		--sha-linux-x86_64 "$sha_linux" \
		--out "$work/bad.rb"
done

# Publish dry run: commit the rendered formula into a local tap checkout and
# refuse to commit again when nothing changed.
mkdir -p "$work/zips"
printf 'silicon' >"$work/zips/axon-server-macos-silicon.zip"
printf 'intel' >"$work/zips/axon-server-macos-intel.zip"
printf 'linux' >"$work/zips/axon-server-linux.zip"
git init -q -b main "$work/tap"
TAG=v1.2.3 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
grep -q 'version "1.2.3"' "$work/tap/Formula/axon-server.rb"
test -f "$work/tap/README.md"
commits=$(git -C "$work/tap" rev-list --count HEAD)
if [ "$commits" -ne 1 ]; then
	echo "expected one tap commit, got $commits" >&2
	exit 1
fi
TAG=v1.2.3 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
commits=$(git -C "$work/tap" rev-list --count HEAD)
if [ "$commits" -ne 1 ]; then
	echo "second dry run created another commit" >&2
	exit 1
fi

# The tap checkout's own git identity is left alone.
if git -C "$work/tap" config --local user.name >/dev/null; then
	echo "publish-tap.sh wrote user.name into the tap checkout's config" >&2
	exit 1
fi

# A dry run must not require the push token.
if TAG=v1.2.3 TAP_TOKEN='' "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"; then
	:
else
	echo "dry run failed when TAP_TOKEN was empty" >&2
	exit 1
fi

# Prerelease tags exit 0 without touching the tap.
TAG=beta-1 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
TAG=alpha-2 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
grep -q 'version "1.2.3"' "$work/tap/Formula/axon-server.rb"

# An older tag (a re-run, or a backport) does not downgrade the tap.
TAG=v1.2.2 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
TAG=v1.1.10 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
grep -q 'version "1.2.3"' "$work/tap/Formula/axon-server.rb"
commits=$(git -C "$work/tap" rev-list --count HEAD)
if [ "$commits" -ne 1 ]; then
	echo "an older tag changed the tap" >&2
	exit 1
fi

# A newer tag does publish, including one that sorts before as a string.
TAG=v1.2.10 "$publish" --dry-run --zip-dir "$work/zips" --tap-dir "$work/tap"
grep -q 'version "1.2.10"' "$work/tap/Formula/axon-server.rb"

# A dry run downloads and clones nothing, so it needs both directories.
reject env TAG=v1.2.3 "$publish" --dry-run
reject env TAG=v1.2.3 "$publish" --dry-run --zip-dir "$work/zips"
reject env TAG=v1.2.3 "$publish" --dry-run --tap-dir "$work/tap"

if grep -q 'AUTHORIZATION: bearer' "$publish"; then
	echo "git HTTPS must send TAP_TOKEN as the basic password, not a bearer header" >&2
	exit 1
fi
grep -q 'username=x-access-token' "$publish"
grep -q 'GIT_CONFIG_KEY_0=credential.helper' "$publish"

echo "homebrew formula render test ok"
