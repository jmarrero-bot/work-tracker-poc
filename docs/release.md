# Releasing the hub's image

The hub runs on jmarrero's bot machine from its container image, which the quadlet in [jmarrero/atomic-setup](https://github.com/jmarrero/atomic-setup) (`bot/workboard-hub/`) pins by digest. A release is a tag here; deploying it is a pull request there that jmarrero merges, so nothing reaches his machine without him.

## Cut a tag

Tag a commit of `main` whose `ci` run is green, with an annotated tag `vMAJOR.MINOR.PATCH`:

```sh
git switch main && git pull
git tag -a v0.1.0 -m 'workboard v0.1.0'
git push origin v0.1.0
```

The [release workflow](../.github/workflows/release.yml) then builds the image from that tag, pushes it to `ghcr.io/jmarrero-bot/work-tracker-poc` with the tag's name, runs the image smoke test (`tests/image_smoke.py`) against the pushed digest, and attests the image's build provenance. Its `packages: write` permission is that job's alone; pull requests build and test the image in `ci` without pushing.

Don't move or reuse a tag: the image tag would move with it, and the digest is what's deployed anyway.

## Get the digest

The run prints the line for the quadlet, in its log and its summary:

```
Image=ghcr.io/jmarrero-bot/work-tracker-poc@sha256:...
```

Take the digest from there, not by resolving the tag later. Check its provenance with the GitHub CLI:

```sh
gh attestation verify oci://ghcr.io/jmarrero-bot/work-tracker-poc@sha256:... --repo jmarrero-bot/work-tracker-poc
```

## Make the package public, once

A package new to ghcr.io can start out private even when its repository is public, and only the web UI changes that. After the first release, the package's owner, signed in as jmarrero-bot, opens <https://github.com/users/jmarrero-bot/packages/container/work-tracker-poc/settings> and, under "Danger Zone", changes its visibility to public. The bot machine then pulls it without credentials. The release run warns while the package isn't public.

## Open the atomic-setup bump

The bot proposes the bump like any atomic-setup change: a branch on its fork, jmarrero-forge/atomic-setup, and a draft pull request there (`bot-pr fork-pr`), which jmarrero reviews and promotes to jmarrero/atomic-setup, and merges. The change is the `Image=` line of `bot/workboard-hub/workboard-hub.container`, with a commit such as `workboard-hub: Update to v0.1.0`. Its description links the release run, the attestation check, and the changes since the last pinned tag (`https://github.com/jmarrero-bot/work-tracker-poc/compare/vOLD...vNEW`), and says if the hub's flags or setup changed with it.

Once it is merged, in the atomic-setup checkout on the bot machine:

```sh
git pull
systemctl --user daemon-reload && systemctl --user restart workboard-hub
```

Upgrade the companions first when the release changes what the hub exports: a companion refuses all of a hub's export that has a source or a reason it doesn't know (see [hub.md](hub.md)).
