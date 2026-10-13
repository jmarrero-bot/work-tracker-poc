# Releasing the hub's image

A hub runs from its container image, which its deployment, such as a quadlet, pins by digest. A release is a tag here; deploying it is a change to that pin, which whoever runs the hub reviews, so nothing reaches their machine without them.

## Cut a tag

Tag a commit of `main` whose `ci` run is green, with an annotated tag `vMAJOR.MINOR.PATCH`:

```sh
git switch main && git pull
git tag -a v0.1.0 -m 'workboard v0.1.0'
git push origin v0.1.0
```

The [release workflow](../.github/workflows/release.yml) then builds the image from that tag and runs the image smoke test (`tests/image_smoke.py`) on it. Only then does it push it to `ghcr.io/OWNER/REPO` (here `ghcr.io/jmarrero-bot/work-tracker-poc`, or the repository variable `IMAGE_NAME` in place of `OWNER/REPO`) with the tag's name, run the smoke test again on the pushed digest, and attest the image's build provenance. Its `packages: write` permission is that job's alone; pull requests build and test the image in `ci` without pushing.

**If the run fails, don't pin anything from it**, even a digest it printed or pushed: an image pushed before a later step failed may be untested or unattested. Fix the cause on `main` and cut a new tag. Don't move or reuse a tag: the image tag would move with it, and the digest is what's deployed anyway.

## Move the base images

The Containerfile pins its base images by digest, so a tag builds from the same bases however late it's cut, and they move only when someone moves them: before a release, at least for security fixes in the runtime image. Look up each tag's current digest, put it after the tag in its `FROM` line, and land that as a pull request like any other change:

```sh
skopeo inspect --format '{{.Digest}}' docker://gcr.io/distroless/cc-debian13:nonroot
skopeo inspect --format '{{.Digest}}' docker://docker.io/library/rust:1-slim-trixie
skopeo inspect --format '{{.Digest}}' docker://docker.io/library/node:22-trixie-slim
```

## Get the digest

The run prints the line to pin, in its log and its summary:

```
Image=ghcr.io/jmarrero-bot/work-tracker-poc@sha256:...
```

Take the digest from there, not by resolving the tag later. Check its provenance with the GitHub CLI:

```sh
gh attestation verify oci://ghcr.io/jmarrero-bot/work-tracker-poc@sha256:... --repo jmarrero-bot/work-tracker-poc
```

## Check that the package is public

A deployment pulls the image without credentials, so its package, under the repository owner's **Packages**, has to be public. This repository's is, since v0.1.0, and the release run warns if an anonymous pull of the new digest fails. Should that happen, the package's owner changes its visibility back to public under its settings' "Danger Zone"; only the web UI can.

## Move the pin

Move the deployment's pin to the new digest, as a change its owner reviews, and restart the hub. Its description links the release run, the attestation check, and the changes since the last pinned tag (`https://github.com/OWNER/REPO/compare/vOLD...vNEW`), and says if the hub's flags or setup changed with it.

### This repository's own hub

This repository's hub runs from the quadlet in its operator's setup repository, jmarrero/atomic-setup (`bot/workboard-hub/`). The bot proposes the bump like any change there: a branch on its fork, jmarrero-forge/atomic-setup, and a pull request to jmarrero/atomic-setup, which the operator reviews and merges. The change is the `Image=` line of `bot/workboard-hub/workboard-hub.container` and the comment above it naming the tag and its release run, with a commit such as `workboard-hub: Update to v0.2.0`. Once it is merged, in the atomic-setup checkout on the hub's machine:

```sh
git pull
systemctl --user daemon-reload && systemctl --user restart workboard-hub
```

Upgrade the companions first when the release changes what the hub exports: a companion refuses all of a hub's export that has a source or a reason it doesn't know (see [hub.md](hub.md)).
