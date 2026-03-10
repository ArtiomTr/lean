# Dependencies

These rules apply to workspace and crate dependency declarations in `Cargo.toml`.

## [D001]: Dependencies in workspace root

In `[workspace.dependencies]`, list dependencies in two groups:

1. Workspace members as `path` dependencies.
2. External dependencies (git or semver), separated from group 1 by one blank line.

Example:

```toml
[workspace.dependencies]
chain = { path = './chain' }
networking = { path = './networking' }

anyhow = '1'
clap = { version = '4', features = ['derive'] }
```

## [D002]: Crate dependencies

In crate `Cargo.toml`, all dependencies must use `workspace = true` (with optional crate-specific features). Keep them in a single group.

Example:

```toml
# ❌ DON'T DO THIS
[dependencies]
anyhow = '1'
chain = { workspace = true }
clap = '4'

# ❌ DON'T DO THIS
[dependencies]
chain = { workspace = true }

anyhow = { workspace = true }
clap = { workspace = true }

# ✅ DO THIS INSTEAD
[dependencies]
anyhow = { workspace = true, features = ['backtrace'] }
chain = { workspace = true }
clap = { workspace = true }
```

## [D003]: Dependency ordering

Sort dependencies alphabetically within each group.

## [D004]: Specifying dependency features

Define features at workspace level unless they are crate-specific or conditionally enabled in a crate.

## [D005]: TOML formatting

Use single quotes for strings and inline table syntax for dependency declarations.

## [D006]: Dependency versions

- Prefer major-only semver specification (`x` when `x > 0`, `0.y` when `x = 0`) unless exact pinning is required due to known issues.
- For git dependencies, prefer tag; otherwise commit hash.
- Avoid branch-based or floating git dependency specs.
