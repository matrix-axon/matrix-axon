# winget manifests (ADR 0120)

Templates for the three Windows packages, and `render.sh`, which fills them from a GitHub Release.

| Package                 | Release asset                  | Kind                          |
| ----------------------- | ------------------------------ | ----------------------------- |
| `MatrixAxon.AxonServer` | `axon-server-windows.zip`      | portable, alias `axon-server` |
| `MatrixAxon.AxonTui`    | `axon-tui-windows.zip`         | portable, alias `axon-tui`    |
| `MatrixAxon.Axon`       | `Axon_<version>_x64-setup.exe` | NSIS installer, per user      |

CI does not use these yet. Steady-state releases will go through `wingetcreate update` (ADR 0120); this directory exists for the first submission of each package, which an update tool cannot do.

## First submission

Each package is its own pull request to `microsoft/winget-pkgs` (the pull request template requires one manifest per PR).
Submit from the `matrix-axon/winget-pkgs` fork, so the PR comes from the organization and not a person.

You need a Windows machine with a current `winget`, Git Bash (or any bash with `sed` and `awk`), `git`, and `gh` logged in with `public_repo`.
The manifests use schema 1.12.0, so an older `winget` may reject them; update App Installer if `winget validate` says the version is unsupported.
Local manifests must be enabled once, from an elevated shell: `winget settings --enable LocalManifestFiles`.

### 1. Render

```sh
TAG=v0.1.10 packaging/winget/render.sh "$HOME/winget-out" axon-server axon-tui
```

This downloads each release asset, hashes it, and writes `manifests/m/MatrixAxon/<Package>/<version>/` under the output directory.
The SHA-256 is of the bytes the script downloaded, and it should equal the digest GitHub shows for the asset (`gh release view TAG --json assets`).
Name `axon` as well to render the desktop installer.

Each rendered file is also parsed as YAML, with `ruby` or `python3` with PyYAML if either is installed, so a template that is not valid YAML fails here rather than in `winget validate`.
With neither installed (typical for Git Bash on Windows) the script warns and skips the check.

### 2. Validate and test each package

```powershell
$dir = "$HOME\winget-out\manifests\m\MatrixAxon\AxonServer\0.1.10"
winget validate --manifest $dir
winget install --manifest $dir
axon-server --version     # resolved through the portable alias; open a new shell first
winget list MatrixAxon     # a local-manifest install is listed under an ID that is not MatrixAxon.AxonServer
winget uninstall <the Id column from winget list>
```

Repeat for `AxonTui` (`axon-tui --version`).

A package installed from a local manifest is not known to winget by its package identifier, so `winget uninstall MatrixAxon.Axon` finds nothing.
For the desktop installer, uninstall it by its ARP identifier:

```powershell
winget uninstall ARP\User\X64\MatrixAxon.Axon__DefaultSource
```
Fix anything `validate` reports in the **templates**, not in the rendered copy, and commit it, so the automated job inherits the fix.

For the desktop installer (`MatrixAxon.Axon`), the install is per user and silent (`/S`).
After it installs, read the uninstall entry once, because the manifest does not yet carry a `ProductCode` or `AppsAndFeaturesEntries`:

```powershell
Get-ChildItem HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall |
  Get-ItemProperty | Where-Object DisplayName -like 'Axon*' |
  Select-Object PSChildName, DisplayName, Publisher, DisplayVersion
```

Compare `Publisher` with the manifest's `matrix-axon`. An installer built before `bundle.publisher` was set shows Tauri's default (`matrixaxon`).
Add `AppsAndFeaturesEntries` (with `DisplayName`, `Publisher`, `DisplayVersion` and `ProductCode` as shown) to the installer template if they differ, or wait for a release built with the setting.
Also install over an older build, if one is around, and confirm it lands in the same folder (ADR 0120).

### 3. Open the pull request from the fork

One per package. Fork once, then for each package:

```sh
git clone --depth 1 --single-branch https://github.com/matrix-axon/winget-pkgs
cd winget-pkgs
git remote add upstream https://github.com/microsoft/winget-pkgs
git fetch --depth 1 upstream master

PKG=AxonServer; VER=0.1.10
git switch -c "MatrixAxon.$PKG-$VER" upstream/master
mkdir -p manifests/m/MatrixAxon/$PKG
cp -r "$HOME/winget-out/manifests/m/MatrixAxon/$PKG/$VER" manifests/m/MatrixAxon/$PKG/
git add manifests/m/MatrixAxon/$PKG/$VER
git commit -m "New package: MatrixAxon.$PKG version $VER"
git push origin HEAD
```

Then open the PR against `master`, with the repository's template as the body (tick the boxes you actually did):

```sh
gh api repos/microsoft/winget-pkgs/contents/.github/PULL_REQUEST_TEMPLATE.md -q .content | base64 -d > pr-body.md
# edit pr-body.md: describe the package, tick the checklist
gh pr create --repo microsoft/winget-pkgs --base master \
  --head "matrix-axon:MatrixAxon.$PKG-$VER" \
  --title "New package: MatrixAxon.$PKG version $VER" --body-file pr-body.md
```

The title format is `New package: Publisher.Name version X.Y.Z`. The first contribution needs the Microsoft CLA, which a bot comment on the PR walks through.

### 4. After it is open

- An automated pipeline downloads the installer, runs Defender and SmartScreen, and installs it in a sandbox. The PR is labeled with the outcome; `ValidationFailureGuide.md` in `winget-pkgs` explains the labels.
- A moderator reviews new packages by hand, which can take days. Answer comments on the PR; push fixes as new commits.
- Do not replace or re-upload a release asset once its manifest is submitted: the hash would no longer match.
- `wingetcreate submit` is an alternative, but as far as I know it pushes to a fork owned by the token's user rather than the organization's, which is why this document does it with `git` and `gh`.
