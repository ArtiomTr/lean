---
name: rust-code-style-reviewer
description: Review Rust code for code style guidelines.
---

# Rust code style reviewer

Use this skill after feature work to run a focused Rust style review.

Only report violations covered by this skill's guidelines. Do not report other issues.

## Input expectations

The user should provide one or more review targets:

- Specific crates
- Specific files
- A git diff/range

## Output format

Format your findings in oxlint/biome format, including:

1. Guideline identifier (for example, `[I001]`, `[D003]`).
2. File path (line number if available).
3. Short violation description.
4. Violating code snippet.
5. Optional fix snippet for trivial corrections, otherwise a short fix direction.

## Scope

Review only these two categories:

- Import style in Rust source files
- Dependency style in `Cargo.toml` files

## Guidelines

### Imports

#### [I001]: Import grouping

`use` statements must be separated into three groups:

1. `core`/`std` imports (first, right after file docs if present).
2. External crate imports (separated from group 1 by one blank line).
3. `crate::`/`super::` imports (separated from group 2 by one blank line).

Example:

```rust
use std::{fmt, vec::Vec};
use core::ptr;

use anyhow::Error;
use tracing::debug;

use crate::config::Config;
use super::node::Node;
```

#### [I002]: Import sorting

Within each import group, sort imports using standard `rustfmt` ordering.

#### [I003]: Import deduplication

Avoid multiple `use` statements from the same crate unless imports are conditionally gated (for example, `#[cfg(...)]`).

Example:

```rust
// ❌ DON'T DO THIS
use std::vec::Vec;
use std::fmt;
use std::fmt::Debug;

// ✅ DO THIS INSTEAD
use std::{vec::Vec, fmt::{self, Debug}};

// ✅ ALSO CORRECT
#[cfg(linux)]
use std::os::unix;
use std::vec::Vec;
```

#### [I004]: Trait importing

If a trait is imported only for method resolution and not referenced explicitly, import it as `_`.

Example:

```rust
// ❌ DON'T DO THIS
use std_ext::ArcExt;

// ✅ DO THIS INSTEAD
use std_ext::ArcExt as _;
```

#### [I005]: Over(under)-qualified imports

Prefer fully (or nearly fully) qualified imports when a short imported symbol is ambiguous. Otherwise, import and use the member directly, avoiding unnecessary qualification.

Example:

```rust
// ❌ DON'T DO THIS - "var" is too generic
use std::env::var;

// ✅ DO THIS INSTEAD
use std::env;

// ❌ DON'T DO THIS - Error is often used for generic errors
use std::fmt::Error;

// ✅ DO THIS INSTEAD
use anyhow::Error;
use std::fmt;

// ❌ DON'T DO THIS - overqualification in usage
use config;

// ✅ DO THIS INSTEAD
use config::Config;
```

#### [I006]: Renaming imports

Avoid renaming imports where qualification can solve conflicts. Prefer renaming only as a last resort.

Exceptions:

- Qualification would require overly long paths (more than one additional path member) to disambiguate reasonably.
- Renaming is the clearest option for standard-library error trait usage (`StdError`).

Examples:

```rust
// ❌ DON'T DO THIS
use anyhow::Error as AnyhowError;
use thiserror::Error;

// ✅ DO THIS INSTEAD
use anyhow::Error;

#[derive(thiserror::Error)]
enum MyError {}

// ✅ ACCEPTABLE LAST-RESORT RENAME
use networking::config::Config as NetworkConfig;
use chain::config::Config as ChainConfig;

// ✅ ALSO GOOD WHEN SHORTER QUALIFIED TYPES EXIST
use networking;
use chain;

struct AppConfig {
    chain_config: chain::Config,
    network_config: networking::Config,
}

// ✅ COMMON EXCEPTION FOR ERROR TRAIT
use std::error::Error as StdError;

impl Something {
    fn trace() -> Box<dyn StdError> {
        todo!()
    }
}

// ✅ IF ONLY TRAIT METHODS ARE NEEDED
use std::error::Error as _;
```

### Dependencies

These rules apply to workspace and crate dependency declarations in `Cargo.toml`.

#### [D001]: Dependencies in workspace root

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

#### [D002]: Crate dependencies

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

#### [D003]: Dependency ordering

Sort dependencies alphabetically within each group.

#### [D004]: Specifying dependency features

Define features at workspace level unless they are crate-specific or conditionally enabled in a crate.

#### [D005]: TOML formatting

Use single quotes for strings and inline table syntax for dependency declarations.

#### [D006]: Dependency versions

- Prefer major-only semver specification (`x` when `x > 0`, `0.y` when `x = 0`) unless exact pinning is required due to known issues.
- For git dependencies, prefer tag; otherwise commit hash.
- Avoid branch-based or floating git dependency specs.
