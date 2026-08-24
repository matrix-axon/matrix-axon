# shellcheck shell=bash
#
# Shared release-tag/repo resolution for the ADR 0086 demo scripts
# (scripts/demo-web.sh, scripts/demo-all.sh) — kept in one place so the two
# scripts' notion of "which release" can't drift apart from each other, or
# from what .github/workflows/api-docs.yml resolves by default.
#
# Not a standalone script — sourced. Callers must already define `die() {
# printf '...\n' >&2; exit 1; }` (or equivalent) before sourcing this.

# demo_release_tag <repo>  ->  the tag to operate on: $DEMO_RELEASE_TAG if
# set, else the most recently *published* release on <repo>.
#
# Deliberately not GitHub's own "latest release" (`gh release view`/
# `gh release download` with no tag, the `/releases/latest` API) — that
# excludes pre-releases, and every release in this repo is one on purpose
# (ADR 0086, so a video-only or demo release never wears the "Latest" badge
# on a version that isn't). `gh release list`'s default sort does not
# exclude them, so it is what resolves "latest" here — matching
# api-docs.yml's own default.
demo_release_tag() {
  local repo="$1"
  if [ -n "${DEMO_RELEASE_TAG:-}" ]; then
    printf '%s\n' "$DEMO_RELEASE_TAG"
    return
  fi
  local tag
  tag="$(gh release list --repo "$repo" --exclude-drafts --limit 1 --json tagName -q '.[0].tagName' 2>/dev/null)" ||
    die "could not list releases on $repo — set DEMO_RELEASE_TAG yourself"
  [ -n "$tag" ] || die "$repo has no releases — create one first, or set DEMO_RELEASE_TAG yourself"
  printf '%s\n' "$tag"
}

# demo_release_repo  ->  owner/repo to operate on: $DEMO_RELEASE_REPO if set,
# else the 'upstream' git remote, falling back to 'origin'.
demo_release_repo() {
  if [ -n "${DEMO_RELEASE_REPO:-}" ]; then
    printf '%s\n' "$DEMO_RELEASE_REPO"
    return
  fi
  local url
  url="$(git remote get-url upstream 2>/dev/null || git remote get-url origin 2>/dev/null || true)"
  [ -n "$url" ] || die "no 'upstream' or 'origin' git remote — set DEMO_RELEASE_REPO=owner/repo yourself"
  printf '%s\n' "$url" | sed -E 's#^(git@github\.com:|https://github\.com/)##; s#\.git$##'
}
