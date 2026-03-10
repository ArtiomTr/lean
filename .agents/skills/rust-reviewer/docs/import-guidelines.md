# Import guidelines

## [I001]: Import grouping

`use` statements must be separated into four groups:

1. `core`/`std` imports (first, right after file docs if present).
2. External crate imports (separated from group 1 by one blank line).
3. `crate::`/`super::` imports (separated from group 2 by one blank line).
4. `pub use` statements, also separated by one blank line.

Example:

```rust
use std::{fmt, vec::Vec};
use core::ptr;

use anyhow::Error;
use tracing::debug;

use crate::config::Config;
use super::node::Node;

pub use crate::api::Client;
pub use crate::types::Result;
```

## [I002]: Module declarations

`mod` declarations are grouped/sorted by the following rules:

1. Module declarations are put on top of the file, above all of the `use` statements, and right below file-level documentation `//!`. They are grouped by following rules:
   1.1. First group private modules `mod`.
   1.2. Second group is public modules `pub mod`. This includes `pub(crate)` and `pub(super)` modules too. The groups are separated with an empty line.
2. Inline module declarations `mod {` are always put at the bottom of file, after all other code. `#[cfg(test)]` inline modules (unit tests) are always put below all other inline modules, at the very bottom of the file.

Module declarations are sorted alphabetically.

Example:

```rust
//! Some file documentation

mod internal;
mod parser;

pub(crate) mod api;
pub mod prelude;

use std::{collections::HashMap, sync::Arc};
use core::ptr;

use anyhow::Error;
use tracing::debug;

use crate::{error::Error, types::Config};

pub use crate::{api::Client. types::Result};

// other file contents - traits, structs, functions, constants, etc.

mod helpers {
    // ...
}

#[cfg(test)]
mod tests {
    // ...
}
```

## [I003]: Import sorting

Within each import group, sort imports using standard `rustfmt` ordering, aplhabetically.

## [I004]: Import deduplication

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

## [I005]: Trait importing

If a trait is imported only for method resolution and not referenced explicitly, import it as `_`.

Example:

```rust
// ❌ DON'T DO THIS
use std_ext::ArcExt;

// ✅ DO THIS INSTEAD
use std_ext::ArcExt as _;
```

## [I006]: Over(under)-qualified imports

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

## [I007]: Renaming imports

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

## [I008]: Inline imports

Inline imports are forbidden for most of the cases. The only cases, where they are allowed, are:

1. When imported item is gated behind feature flag, and is used several times, only in single context across whole file.

   ```rust
    #[cfg(feature = "parallel")]
   fn some_parallel_method() {
        // parallel iterator is used only inside this method, all other file doesn't use rayon.
        use rayon::ParallelIterator as _;
   }
   ```

## [I009]: Feature-gated imports

Imports, that are gated by feature, are not recommended to use. They should be replaced with qualification instead, following the [I006] and [I007] guidelines, with minor addition - the max qualification path allowed is extended by one. However, if qualified item is repeated 2 or more times, then the feature-gated import is preferred instead.

Example:

```rust
// ❌ DON'T DO THIS

#[cfg(feature = "serde")]
use serde::Serialize;

#[cfg(feature = "serde")]
impl Serialize for MyStruct {}

// ✅ DO THIS INSTEAD
#[cfg(feature = "serde")]
impl serde::Serialize for MyStruct {}

// ❌ DON'T DO THIS
#[cfg(feature = "serde")]
impl serde::Serialize for MyStruct {}

#[cfg(feature = "serde")]
impl serde::Serialize for MyStruct2 {}

// ✅ SERIALIZE USED IN TWO PLACES - THEREFORE, FEATURE-GATED IMPORT IS OKAY HERE
#[cfg(feature = "serde")]
use serde::Serialize;

#[cfg(feature = "serde")]
impl Serialize for MyStruct {}

#[cfg(feature = "serde")]
impl Serialize for MyStruct2 {}
```

## [I010]: Avoid wildcard imports

Wildcard imports must be replaced with specific imported items.

```rust
// ❌ DON'T DO THIS
use rayon::prelude::*;

// ✅ USE SPECIFIC ITEMS INSTEAD
use rayon::ParallelIterator as _;
```
