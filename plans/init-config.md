# Plan: Add `typst-gather analyze` subcommand

## Prerequisites

None. This is the foundation that stage-parsing.md builds on.

## What this repo is

typst-gather is a Rust CLI tool that gathers Typst packages locally for
offline/hermetic builds. It reads a TOML config specifying @preview packages
(from Typst Universe) and @local packages (from local directories), downloads
or copies them to a destination directory.

Key source files:
- `src/main.rs` — CLI entry point. Parses args (single positional: config
  file path), reads TOML, calls `gather_packages()`.
- `src/lib.rs` — All library code. Contains:
  - `Config` / `RawConfig` — TOML config parsing (serde)
  - `PackageEntry` — enum of Preview or Local package entries
  - `gather_packages()` — main entry point: scans discover paths, downloads
    @preview, copies @local, resolves transitive @preview deps
  - `collect_imports()` / `try_extract_spec()` — Typst parser-based import
    extraction from syntax trees
  - `find_imports()` — scans all .typ files in a directory for imports
  - `scan_file_for_imports()` — scans a single file, tracks @local imports
  - `GatherContext` — holds state during gathering (PackageStorage, stats,
    processed set, discovered_local map)
  - `GatherResult` / `Stats` — return types with download/copy/skip/fail counts
- `tests/integration.rs` — integration tests using tempdir fixtures

The tool is consumed by quarto-cli, which currently has its own fragile
TypeScript regex-based import scanner. This plan adds an `analyze` subcommand
so quarto can delegate scanning to typst-gather and get structured JSON back.

## Goal

Add a `typst-gather analyze` subcommand that:
1. Scans .typ files for @preview and @local package imports
2. Follows transitive deps of @local packages (scans their source dirs for
   @preview imports they pull in)
3. Outputs structured JSON to stdout
4. Does NOT download or copy anything

## CLI changes

### Subcommands

Restructure from single positional arg to subcommands using clap:

```
typst-gather gather <config>   # current behavior (download + copy)
typst-gather analyze <config>  # new: scan only, output JSON
```

Where `<config>` is a file path or `-` for stdin.

For backwards compatibility, keep `typst-gather <file>` working as an alias
for `typst-gather gather <file>` (clap default subcommand or fallback
detection based on whether the arg looks like a subcommand).

### Stdin support

When `<config>` is `-`, read TOML from stdin instead of a file. This applies
to both `gather` and `analyze`. This eliminates the need for callers to
create temporary files.

### Analyze-specific behavior

- Does NOT download or copy anything
- Does NOT require `destination` field in config (gather still requires it)
- Reads `discover` paths, scans for imports using Typst's parser
- For @local packages listed in `[local]` config section: reads their source
  dirs and scans for @preview imports (one level of transitive resolution)
- Does NOT recurse into @preview packages (no network, no package cache
  reading — that's added in stage-parsing.md)
- Outputs JSON to stdout
- All diagnostic/progress messages go to stderr

## Config format

Same TOML format as gather. `destination` is optional for analyze.
`rootdir` is supported and applies to `discover` and `[local]` paths
the same way it does for gather.

```toml
# rootdir is optional, resolves relative discover/local paths
rootdir = "path/to/root"
discover = ["template.typ", "typst-show.typ"]

[local]
my-pkg = "/path/to/my-pkg"
```

## Output format

```json
{
  "imports": [
    {
      "namespace": "preview",
      "name": "cetz",
      "version": "0.4.1",
      "source": "template.typ",
      "direct": true
    },
    {
      "namespace": "preview",
      "name": "oxifmt",
      "version": "0.2.1",
      "source": "@local/my-pkg",
      "direct": false
    },
    {
      "namespace": "local",
      "name": "my-pkg",
      "version": "1.0.0",
      "source": "typst-show.typ",
      "direct": true
    }
  ],
  "files": ["template.typ", "typst-show.typ"]
}
```

Field definitions:

- `imports` — deduplicated list of discovered package imports.
- `imports[].namespace` — "preview" or "local".
- `imports[].name` — package name.
- `imports[].version` — version string. Source of truth depends on how the
  import was discovered:
  - Direct imports (from scanning .typ files): version from the `#import`
    statement (e.g. `#import "@local/my-pkg:1.0.0"` → version "1.0.0").
  - @local packages from `[local]` config: version from `typst.toml` in
    the source dir (the `[local]` config maps name → directory path, and
    the version is read from the manifest).
  - Transitive @preview deps found inside @local packages: version from
    the `#import` statement in the local package's .typ files.
- `imports[].source` — provenance. For direct imports: the .typ filename.
  For transitive imports found inside a local package: the package spec
  string (e.g. `@local/my-pkg`).
- `imports[].direct` — true if found in the user's scanned source files,
  false if found transitively inside a `@local` package.
- `files` — basenames of all .typ files that were scanned via `discover`
  paths (not files inside @local packages).

Deduplication: by (namespace, name, version) tuple. If the same package is
found both directly and transitively, `direct: true` wins and `source` is
the direct source file.

## Implementation steps

### 1. Refactor CLI to subcommands (src/main.rs)

Current main.rs has a simple `Args` struct with one positional `spec_file`.
Replace with clap subcommands:

```rust
#[derive(Parser)]
#[command(version, about = "Gather Typst packages to a local directory")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Download and copy packages (default behavior)
    Gather {
        /// TOML config file, or - for stdin
        config: String,
    },
    /// Analyze imports and output JSON (no downloads)
    Analyze {
        /// TOML config file, or - for stdin
        config: String,
    },
}
```

Add a `read_config(path: &str) -> Result<String>` helper that reads from
stdin when path is "-", otherwise reads the file. Used by both subcommands.

For backwards compat: when `command` is `None`, check if there's a bare
positional arg and treat it as `gather`.

The existing gather logic in main() moves into a `run_gather()` function.
The new analyze logic goes into a `run_analyze()` function.

### 2. Move progress output to stderr (src/lib.rs)

Currently `discover_imports()` (line 218, 223) and `cache_preview_with_deps()`
(line 433) and `gather_local()` (line 334, 369) use `println!` for progress.
Move ALL `println!` to `eprintln!` so stdout is clean for JSON output.

This is a simple find-and-replace but important — if any println! leaks
through, it corrupts the JSON output on stdout. The summary line in main.rs
(line 77-80) should also move to stderr.

### 3. Add analyze function (src/lib.rs)

Add new public types and function:

```rust
#[derive(Debug, Serialize)]
pub struct AnalyzeResult {
    pub imports: Vec<ImportInfo>,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ImportInfo {
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub direct: bool,
}

pub fn analyze(
    discover_paths: &[PathBuf],
    local: &HashMap<String, String>,
) -> AnalyzeResult
```

Implementation:
1. For each discover path, scan .typ files using existing `discover_imports`
   / `scan_file_for_imports` logic. Collect direct imports as ImportInfo
   entries with `direct: true`, `source` = filename.
2. Track scanned filenames in `files` list.
3. For each entry in `local` HashMap (name → source dir path):
   a. Read `typst.toml` from source dir to get version. If missing, warn
      on stderr and skip transitive scanning for this package. Still add
      the @local import to results if it was found in a discover scan
      (version from the #import statement).
   b. Use `find_imports()` (existing function, recursive WalkDir scan) on
      the source dir to find @preview imports.
   c. Add each as ImportInfo with `direct: false`,
      `source: format!("@local/{name}")`.
4. Deduplicate by (namespace, name, version). If duplicate, prefer
   `direct: true` over `direct: false`.
5. Return AnalyzeResult.

Note: @local imports discovered during discover scanning but NOT listed in
the `[local]` config section are still reported (direct: true, version from
#import statement) — they just don't get transitive resolution because we
don't know their source directory.

### 4. Add JSON output (src/main.rs, Cargo.toml)

Add `serde_json` to `[dependencies]` in Cargo.toml.

Add `Serialize` derive to `AnalyzeResult` and `ImportInfo`.

In the `run_analyze()` function:
```rust
let result = analyze(&discover_paths, &config.local);
let json = serde_json::to_string_pretty(&result)?;
println!("{json}");
```

This is the ONLY println! in analyze mode — everything else is stderr.

## Testing

### Unit tests for `analyze()` function

Add as a new module in `src/lib.rs` tests or in `tests/integration.rs`.
All tests create tempdir fixtures with .typ files and typst.toml manifests.

#### Direct import discovery

- **single_preview_import**: one .typ file with `#import "@preview/cetz:0.4.1"`.
  Expect single import with namespace=preview, name=cetz, version=0.4.1,
  source=filename, direct=true.

- **single_local_import**: one .typ file with `#import "@local/my-pkg:1.0.0"`.
  Local package source dir exists with valid typst.toml (name=my-pkg,
  version=1.0.0). Config has `[local] my-pkg = "/path/to/src"`.
  Expect import with namespace=local, name=my-pkg, version=1.0.0, direct=true.

- **multiple_imports_single_file**: one .typ file with three imports
  (@preview/cetz, @preview/fletcher, @local/my-pkg). Expect all three in
  output, all direct=true.

- **multiple_files**: two .typ files each with different imports. Expect all
  imports present, `files` list contains both filenames.

- **import_with_items**: `#import "@preview/cetz:0.4.1": canvas, draw` — still
  detected correctly.

- **include_statement**: `#include "@preview/template:1.0.0"` — detected as
  an import.

- **nested_import_in_function**: import inside `#let f() = { import ... }` —
  still detected.

- **relative_import_ignored**: `#import "utils.typ"` produces no package import.

- **no_imports**: file with no imports returns empty imports list, file still
  appears in `files`.

- **invalid_package_spec_ignored**: `#import "@preview/cetz"` (no version) —
  silently skipped, no entry in output.

#### Deduplication

- **duplicate_import_same_file**: same import appears twice in one file.
  Expect single entry in output.

- **duplicate_import_across_files**: same @preview/cetz:0.4.1 in two files.
  Expect single entry; source is the first file encountered.

- **different_versions_not_deduped**: @preview/cetz:0.4.1 and
  @preview/cetz:0.5.0 are different entries (different version = different
  package).

#### @local transitive resolution

- **local_with_preview_dep**: [local] config points my-pkg at a source dir.
  Source dir has typst.toml and a .typ file importing @preview/oxifmt:0.2.1.
  Expect both @local/my-pkg (direct=true) and @preview/oxifmt (direct=false,
  source="@local/my-pkg") in output.

- **local_with_multiple_preview_deps**: local package imports two @preview
  packages. Both appear as transitive with source="@local/my-pkg".

- **local_dep_already_direct**: document directly imports @preview/cetz, and
  @local/my-pkg also imports @preview/cetz. Expect single entry for cetz
  with direct=true (direct wins over transitive).

- **local_with_no_deps**: @local package source dir has .typ files with no
  package imports. Only the @local entry appears.

- **local_missing_typst_toml**: [local] config maps my-pkg to a source dir
  that has .typ files but no typst.toml. Warning on stderr. Transitive
  scanning is skipped. If my-pkg was also found via discover scanning
  (e.g. `#import "@local/my-pkg:1.0.0"`), it appears with the version
  from the #import statement.

- **local_missing_source_dir**: [local] config maps my-pkg to a path that
  doesn't exist. Warning on stderr, transitive scanning skipped. Same
  behavior as above for imports found via discover.

- **local_no_preview_recursion**: @local/my-pkg imports @preview/cetz.
  Cetz's own transitive deps are NOT resolved (no package-cache — that's
  stage-parsing.md's scope). Only my-pkg and cetz appear.

- **local_imports_another_local**: @local/my-pkg's source has
  `#import "@local/other-pkg:2.0.0"`. The @local/other-pkg import is
  reported (direct=false, source="@local/my-pkg") but NOT recursed into.
  Only @preview deps from local package scanning are followed.

- **local_version_from_toml**: [local] config maps my-pkg to source dir.
  Source dir's typst.toml says version=2.0.0. The @local/my-pkg entry in
  output has version=2.0.0 (from typst.toml, not from any #import statement).

#### Discover path handling

- **discover_directory**: discover points at a directory containing multiple
  .typ files. All .typ files in the directory are scanned.

- **discover_directory_non_recursive**: discover directory has a subdirectory
  with .typ files. Subdirectory files are NOT scanned (matches existing
  discover_imports behavior: non-recursive directory scan).

- **discover_single_file**: discover points at a single .typ file. Works.

- **discover_nonexistent_path**: path doesn't exist. Warning on stderr,
  no crash, empty results for that path.

- **discover_mixed**: discover is an array with one directory and one file.
  Both processed correctly.

- **discover_non_typ_file**: discover points at a .txt file. Ignored (only
  .typ files are scanned).

#### Rootdir handling

- **rootdir_resolves_discover**: rootdir="subdir", discover=["template.typ"].
  Scans subdir/template.typ.

- **rootdir_resolves_local**: rootdir="subdir", [local] my-pkg="../pkg-src".
  Resolves to subdir/../pkg-src.

#### Output format

- **files_list_correct**: `files` contains the basenames of all scanned .typ
  files from discover paths, in encounter order, no duplicates. Does NOT
  include files scanned inside @local package source dirs.

- **empty_discover**: no discover paths and no local config. Returns empty
  imports and empty files.

### Integration tests (CLI, in tests/integration.rs or as binary tests)

These tests invoke the compiled typst-gather binary as a subprocess to test
the actual CLI behavior.

#### Subcommand basics

- **analyze_from_file**: write TOML config to a temp file, run
  `typst-gather analyze <file>`. Parse stdout as JSON, verify structure
  matches AnalyzeResult.

- **analyze_from_stdin**: pipe TOML config to `typst-gather analyze -` via
  stdin. Parse stdout as JSON, verify same result as file mode.

- **gather_still_works**: `typst-gather gather <file>` with a minimal config
  (destination set, empty preview section, no network needed). Verify it
  runs without error and exits 0.

- **bare_file_backwards_compat**: `typst-gather <file>` (no subcommand)
  still works — equivalent to `typst-gather gather <file>`.

- **analyze_no_destination_ok**: analyze config without `destination` field.
  Should succeed (exit 0) since destination is only required for gather.

- **gather_no_destination_fails**: gather config without `destination`.
  Should fail (exit non-zero) with error message.

#### JSON output validation

- **json_output_is_valid**: analyze output parses as valid JSON.

- **json_has_required_fields**: parsed output has `imports` (array) and
  `files` (array) at top level.

- **json_import_fields**: each entry in `imports` has all five fields
  (namespace: string, name: string, version: string, source: string,
  direct: bool) with correct types.

- **json_output_no_extra_on_stdout**: stdout contains ONLY valid JSON.
  No progress messages mixed in. Verify by confirming JSON parsing succeeds
  and no extra bytes before/after the JSON object.

#### End-to-end scenarios

- **e2e_extension_like_config**: simulate quarto init-config flow.
  1. Create temp dir with two .typ files importing @preview and @local
     packages
  2. Create local package source dir with typst.toml and a .typ file that
     imports @preview/oxifmt
  3. Write TOML config with discover paths and [local] section
  4. Pipe config on stdin to `typst-gather analyze -`
  5. Verify JSON output contains:
     - Direct @preview imports from step 1
     - Direct @local import from step 1
     - Transitive @preview/oxifmt from step 2 with direct=false
     - Correct source attribution for each

- **e2e_empty_project**: discover path exists but .typ files have no imports.
  Output has empty imports array, files list is populated with the filenames.

- **e2e_local_only**: config has only [local] entries, no discover paths.
  Output contains @local entries (from typst.toml version) and their
  transitive @preview deps. `files` is empty.

- **e2e_discover_only**: config has only discover paths, no [local] section.
  Only direct imports appear. No transitive resolution.

## Quarto-side changes (out of scope for this repo, noted for context)

See `~/src/quarto-cli/plans/typst-gather-init-config.md` for the quarto
changes that consume this new subcommand:
- Replace `discoverImportsFromFiles()` regex scanning with
  `typst-gather analyze -`
- Pipe config on stdin instead of creating temp files
- Parse JSON output to generate typst-gather.toml
