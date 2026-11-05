#!/usr/bin/env bash
# Usage: release.sh validate|create
# validate checks that VERSION can be released from this workflow run.
# create also tags the checked-out commit with VERSION and creates a draft
# release with the files in dist/.
set -euo pipefail

if [[ "${GITHUB_REPOSITORY}" != DataDog/dd-test-rs || "${GITHUB_REF}" != refs/heads/main || "${GITHUB_EVENT_NAME}" != workflow_dispatch ]]; then
  echo 'Releases must be dispatched from main in DataDog/dd-test-rs.' >&2
  exit 1
fi

if [[ ! "${VERSION}" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo 'Version must have the form vMAJOR.MINOR.PATCH without leading zeroes.' >&2
  exit 1
fi

crate_version=$(cargo metadata --no-deps --format-version 1 --locked |
  jq -r '.packages[] | select(.name == "dd-test-wrapper") | .version')
if [[ "v$crate_version" != "$VERSION" ]]; then
  echo "The workspace version is $crate_version. Update it to ${VERSION#v} on main before releasing $VERSION." >&2
  exit 1
fi

# A re-run must be authorized by both the original actor and the re-running actor.
for actor in "$GITHUB_ACTOR" "$GITHUB_TRIGGERING_ACTOR"; do
  allowed=$(gh api "repos/$GITHUB_REPOSITORY/collaborators/$actor/permission" \
    --jq '.user.permissions | .maintain == true or .admin == true')
  if [[ "$allowed" != true ]]; then
    echo "Release requires Maintain or Admin access: $actor is not authorized." >&2
    exit 1
  fi
done

refs=$(gh api "repos/$GITHUB_REPOSITORY/git/matching-refs/tags/$VERSION" --jq '.[].ref')
if grep -Fxq "refs/tags/$VERSION" <<< "$refs"; then
  echo "Tag $VERSION already exists. Choose an unused version." >&2
  exit 1
fi

case "${1:-}" in
  validate) exit 0 ;;
  create) ;;
  *) echo 'Usage: release.sh validate|create' >&2; exit 1 ;;
esac

sha=$(git rev-parse HEAD)
# Creating the ref fails if another run already claimed this version. A tag is never moved.
gh api "repos/$GITHUB_REPOSITORY/git/refs" -f "ref=refs/tags/$VERSION" -f "sha=$sha" > /dev/null
gh release create "$VERSION" --verify-tag --title "$VERSION" --draft --generate-notes dist/*
release_url=$(gh release view "$VERSION" --json url --jq .url)
{
  echo "## Draft release $VERSION"
  echo "Commit: $sha"
  echo "Review the assets and generated notes, then publish: $release_url"
} >> "$GITHUB_STEP_SUMMARY"
