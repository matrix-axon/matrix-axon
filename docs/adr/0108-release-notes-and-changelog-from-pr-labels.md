# ADR 0108 — Release notes and CHANGELOG.md come from PR labels

**Status:** Accepted.
Implemented in the release tooling alongside this record.

## Context

ADR 0106 has release-plz open the release PR and tag its merge, and leaves the GitHub Release to the build workflows, which attach files to it.
Nothing wrote release notes: the Releases page held a tag and files, and there was no `CHANGELOG.md`.

Notes should start out generated from the PRs merged since the last tag, grouped by what kind of change each was, and a person should be able to edit them before the release.

release-plz can write a changelog (`changelog_update`), through git-cliff.
It does not fit here:

- Commit subjects are only partly conventional (ADR 0106), so grouping by commit type would be uneven.
- release-plz fills in only a commit's PR number and author username from GitHub, not its PR's labels (checked in release_plz_core 0.38.6, `changelog_filler.rs`), so a template cannot group by label.
- The release PR is the natural place to edit a changelog, but release-plz closes it and opens a new one when anyone has pushed to its branch, which is what happens on the next push to `main`. Edits made there are lost.

## Decision

GitHub's generate-notes API writes the notes, grouped by the categories in `.github/release.yml`, which map PR labels to headings.
`scripts/release-notes.sh` wraps the API and the file handling; `.github/workflows/release-notes.yml` runs it.
release-plz keeps `changelog_update = false`.

### A draft release is the place to edit

After every Release-plz run on `main`, the `draft` job finds the open release PR, reads the version from its `Cargo.toml`, and creates or updates a draft GitHub Release for that tag.
Only a branch in this repository counts as the release PR, since a fork can name its branch `release-plz-anything`, and the version must be `X.Y.Z`.
Its notes cover the PRs merged since the previous `vX.Y.Z` tag.
A person edits the draft on GitHub before merging the release PR.

Every step that pipes sets `shell: bash`, because the default shell has no `pipefail` and a failed `generate` would otherwise leave notes that are only a marker; `stamp` also refuses empty input.

The job must not overwrite those edits.
It ends the notes with a hidden marker holding a hash of the generated text.
If the draft's text no longer matches its marker, or the marker is gone, the job leaves the notes alone, renames the draft if the version changed, and logs a warning.
PRs merged after that point are not added.
Running the workflow by hand with `force` regenerates the notes and discards the edits.
Line endings and trailing blank lines are ignored when comparing, since editing on github.com changes them.

A draft whose tag already exists in git has been tagged but not published, so the job does not reuse it for a newer release PR; `publish` owns it.

A draft is used rather than a file in the release PR for the reason above: a draft is not touched by release-plz.

### The tag publishes the draft

On the `vX.Y.Z` tag push, the `publish` job publishes the draft in place, without the marker.
With no draft (a hand-made tag, or a release PR that never got one), it generates the notes then.
If a Release for the tag already exists, it only fills in an empty body.
The build workflows then attach their files to that Release as before.

The job has its own concurrency group, not the `github-release-<ref>` group the build workflows' release jobs share.
GitHub keeps one running and one pending run per group and cancels the rest, so a cancelled `publish` would skip the `changelog` job and leave the Release without notes.
`publish` has no `needs`, so it normally runs before the build workflows create the Release.
If one gets in first, `publish` fills in the empty body instead of creating a second Release.
It never publishes empty notes: it generates them when the draft's are blank, and fails if they are still blank.

### CHANGELOG.md is a follow-up PR

After publishing, the `changelog` job adds the published notes to `CHANGELOG.md` as `## vX.Y.Z - date` and opens a PR with `RELEASE_PLZ_TOKEN`.
A PR from `GITHUB_TOKEN` would get none of the required checks, and `main` takes changes through PRs.
The job can be re-run: it skips opening the PR when one is already open for the branch.
`CHANGELOG.md` therefore trails each release by one merge, and it carries the notes as finally edited.

### Labels

`.github/release.yml` has these categories, first match wins: Breaking Changes (`breaking-change`), Security (`security`), Features (`enhancement`, `feature`), Bug Fixes (`bug`, `fix`), Performance (`performance`), Documentation (`documentation`), Dependencies (`dependencies`), Build and CI (`build`, `ci`), and Other Changes for everything else.
A PR with no label is listed under Other Changes, so an unlabelled PR is visible rather than missing.
PRs labelled `skip-changelog` are left out.
release-plz puts that label on the release PR (`pr_labels`), and the CHANGELOG.md PR gets it too.

## Consequences

- Label a PR before it merges. The notes are generated from the labels at that point, though a draft that has not been edited picks up later label changes on the next run.
- A draft stays untouched once edited. Check it before merging the release PR if PRs have merged since the edit.
- The workflow assumes release-plz's default branch prefix, `release-plz-`.
- Unverified until a release has gone through it: that the build workflows' `softprops/action-gh-release` steps keep the notes body when they find the Release already published with one.
- The `skip-changelog` label is created by the workflow if it is missing. The other labels must exist in the repository for people to apply them.
