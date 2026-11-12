#!/usr/bin/env bash
# Publish main to Forgejo; Forgejo's push mirror carries it to GitHub. Never forces.
#   mise run publish            push main
#   mise run publish -- v1.0.0  set the Cargo.toml version to 1.0.0 (and Cargo.lock) and commit it
#                               if it differs, create the annotated tag and push it together with
#                               main; on GitHub the tag starts .github/workflows/release.yml (build,
#                               attest, release)
# The repository URLs are not stored in .git: they are built from FORGEJO_URL and GITHUB_URL
# (the account base, read from .env at the repository root, which git ignores) plus the name of
# the repository folder. GitHub is only read, to refuse a tag that already exists there.
# The script doesn't wait for the mirror or the build: it prints the links to follow them.
set -euo pipefail

die() { echo "publish: $*" >&2; exit 1; }

root=$(git rev-parse --show-toplevel)
[[ -f $root/.env ]] || die "$root/.env not found (needs FORGEJO_URL and GITHUB_URL)"
set -a
# shellcheck source=/dev/null
. "$root/.env"
set +a
[[ -n ${FORGEJO_URL:-} ]] || die "FORGEJO_URL is not set in .env"
[[ -n ${GITHUB_URL:-} ]] || die "GITHUB_URL is not set in .env"
project=$(basename "$root")
forgejo="${FORGEJO_URL%/}/$project.git"
github="${GITHUB_URL%/}/$project.git"
# git@github.com:owner -> https://github.com/owner/<project>
web="https://github.com/${GITHUB_URL#*github.com[:/]}"
web="${web%/}/$project"
tag=${1:-}

[[ $(git rev-parse --abbrev-ref HEAD) == main ]] || die "not on main"
# With a tag, Cargo.toml and Cargo.lock may have changes: the version bump commits them.
dirty=$(git status --porcelain)
[[ -n $tag ]] && dirty=$(grep -vE '^ ?M  ?Cargo\.(toml|lock)$' <<<"$dirty" || true)
[[ -z $dirty ]] || die "working tree is not clean"

if [[ -n $tag ]]; then
  [[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]] || die "tag must look like v1.2.3 (release.yml listens to v*)"
  ! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists locally"
  [[ -z $(git ls-remote --tags "$github" "refs/tags/$tag") ]] || die "tag $tag already exists on $github"
  [[ -z $(git ls-remote --tags "$forgejo" "refs/tags/$tag") ]] || die "tag $tag already exists on $forgejo"
fi

if [[ -n $tag ]]; then
  version=${tag#v}
  # Only the first version line: the one in [package].
  sed -i '0,/^version = ".*"$/s//version = "'"$version"'"/' Cargo.toml
  cargo update -w --offline -q
  if [[ -n $(git status --porcelain Cargo.toml Cargo.lock) ]]; then
    git commit -q -m "🔖 bump version to $version" -- Cargo.toml Cargo.lock
    echo "publish: committed version $version"
  fi
fi
sha=$(git rev-parse HEAD)
refs=(main)
if [[ -n $tag ]]; then
  git tag -a "$tag" -m "$tag"
  refs+=("refs/tags/$tag")
fi
# One push, so the mirror sync it triggers carries main and the tag together.
echo "publish: pushing ${sha:0:7}${tag:+ and $tag} to $forgejo"
git push "$forgejo" "${refs[@]}"

echo "publish: done; Forgejo's mirror carries it to GitHub"
if [[ -n $tag ]]; then
  echo "build:   $web/actions/workflows/release.yml?query=branch%3A$tag"
  echo "release: $web/releases/tag/$tag"
else
  echo "commits: $web/commits/main"
fi
