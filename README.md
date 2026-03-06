# typst-gather

Gather Typst packages locally for offline/hermetic builds.

## Install

```bash
cargo install --path .
```

## Usage

```bash
typst-gather packages.toml
```

Then point both `TYPST_PACKAGE_CACHE_PATH` and `TYPST_PACKAGE_PATH` at the
destination directory so Typst can resolve both `@preview` and `@local` packages:

```bash
export TYPST_PACKAGE_CACHE_PATH=/path/to/packages
export TYPST_PACKAGE_PATH=/path/to/packages
typst compile document.typ
```

Or equivalently, using CLI flags:

```bash
typst compile document.typ \
  --package-cache-path /path/to/packages \
  --package-path /path/to/packages
```

## TOML format

```toml
destination = "/path/to/packages"

# Single path
discover = "/path/to/templates"

# Or array of paths (files or directories)
discover = ["template.typ", "typst-show.typ", "/path/to/dir"]

[preview]
cetz = "0.4.1"
fontawesome = "0.5.0"

[local]
my-template = "/path/to/src"
```

- `destination` — Required. Directory where packages will be gathered.
- `discover` — Optional. Paths to scan for `#import` statements. Can be:
  - A single string path
  - An array of paths
  - Each path can be a `.typ` file or a directory (scans `.typ` files non-recursively)
- `[preview]` — Packages downloaded from Typst Universe. Skipped if already cached.
- `[local]` — Packages copied from a local directory. The source must contain a
  `typst.toml` manifest with `name` and `version` fields. Always copies fresh
  (overwrites any existing version).

## How it works

Both `@preview` and `@local` packages are written to the `destination` directory
using Typst's standard cache layout:

```
destination/
├── preview/
│   ├── cetz/0.4.1/
│   └── fontawesome/0.5.0/
└── local/
    └── my-template/1.0.0/
```

Recursively resolves `@preview` dependencies from `#import` statements using
Typst's own parser for reliable import detection.

## Quarto Integration

When used with Quarto extensions, you can run:

```bash
quarto call typst-gather
```

This will auto-detect `.typ` files from `_extension.yml` (template and template-partials) and gather their dependencies.
