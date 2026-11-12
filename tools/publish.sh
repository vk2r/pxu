#!/usr/bin/env bash
# Publish main to Forgejo; Forgejo's push mirror carries it to GitHub. Never forces.
#   mise run publish            push main
#   mise run publish -- v1.0.0  also create the annotated tag and push it; on GitHub the tag
#                               starts .github/workflows/release.yml (build, attest, release)
# The github remote is only read: to refuse a tag that already exists there and to wait
# until the mirror has delivered the push.
set -euo pipefail

github=${PUBLISH_GITHUB_REMOTE:-github}
forgejo=${PUBLISH_FORGEJO_REMOTE:-forgejo}
timeout=${PUBLISH_MIRROR_TIMEOUT:-120}
tag=${1:-}

die() { echo "publish: $*" >&2; exit 1; }

[[ $(git rev-parse --abbrev-ref HEAD) == main ]] || die "not on main"
[[ -z $(git status --porcelain) ]] || die "working tree is not clean"
for remote in "$github" "$forgejo"; do
  git remote get-url "$remote" >/dev/null 2>&1 || die "remote '$remote' does not exist"
done

if [[ -n $tag ]]; then
  [[ $tag == v* ]] || die "tag must start with v (release.yml listens to v*)"
  version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)
  [[ $tag == "v$version" ]] || die "tag $tag does not match Cargo.toml version $version"
  ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists locally"
  [[ -z $(git ls-remote --tags "$github" "refs/tags/$tag") ]] || die "tag $tag already exists on $github"
  [[ -z $(git ls-remote --tags "$forgejo" "refs/tags/$tag") ]] || die "tag $tag already exists on $forgejo"
fi

# Wait until $github shows <ref> at <sha>, the sign that the mirror ran.
wait_mirror() {
  local ref=$1 sha=$2 start=$SECONDS
  until [[ $(git ls-remote "$github" "$ref" | cut -f1) == "$sha" ]]; do
    ((SECONDS - start < timeout)) || die "$github still lacks $ref after ${timeout}s; check the push mirror in Forgejo"
    sleep 5
  done
  echo "publish: $ref is on $github"
}

sha=$(git rev-parse HEAD)
echo "publish: pushing ${sha:0:7} to $forgejo"
git push "$forgejo" main
wait_mirror refs/heads/main "$sha"

if [[ -n $tag ]]; then
  git tag -a "$tag" -m "$tag"
  git push "$forgejo" "refs/tags/$tag"
  wait_mirror "refs/tags/$tag" "$(git rev-parse "refs/tags/$tag")"
fi
echo "publish: done"
