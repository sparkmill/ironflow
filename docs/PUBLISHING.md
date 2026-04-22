# Publishing guidelines

Ensure all changes are merged, CI is green on `main`, crate versions are
bumped in `Cargo.toml`, and `CHANGELOG.md` has been updated (entries moved
from `[Unreleased]` into a versioned heading with a comparison link).

## Publish

```sh
cargo login  # if not already logged in

# Only publish macros if it has changes — skip otherwise.
# Macros must go first because ironflow depends on it.
cargo package -p ironflow-macros
cargo publish -p ironflow-macros

cargo package -p ironflow
cargo publish -p ironflow
```

## Create a GitHub release

```sh
VERSION=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name=="ironflow") | .version')

NOTES=$(awk -v ver="$VERSION" '
  $0 ~ "^## \\[" ver "\\]" { found=1; next }
  found && /^## \[/ { exit }
  found
' CHANGELOG.md)

git tag -a "v${VERSION}" -m "v${VERSION}"
git push origin "v${VERSION}"
gh release create "v${VERSION}" --title "v${VERSION}" --notes "${NOTES}"
```

Alternatively, create the release from the GitHub web UI: **Releases** →
**Draft a new release**, tag `v<VERSION>`, and paste the release notes.
