# Plan: Add package-cache support to `typst-gather analyze`

## Prerequisites

Depends on init-config.md being fully implemented (done in v0.2.0,
commit c674ce9). After that plan, the codebase has:

- `src/main.rs` — CLI with two subcommands (`gather` and `analyze`) using
  clap. Both accept `<config>` (file path or `-` for stdin). Bare
  `typst-gather <file>` is backwards-compatible (treated as `gather`).
  `run_gather()` contains the original download/copy logic. `run_analyze()`
  calls `analyze(&config)` and prints JSON to stdout.
- `src/lib.rs` — contains:
  - `Config` / `RawConfig` — TOML config parsing (destination, rootdir,
    discover, preview, local)
  - `analyze(config: &Config) -> AnalyzeResult` — takes the full Config
    struct (not individual fields). Handles rootdir resolution internally
    for discover and local paths. Scans discover paths for imports,
    follows @local transitive deps (reads source dirs for @preview
    imports they pull in), deduplicates using a
    `HashMap<(String, String, String), ImportInfo>` keyed by
    (namespace, name, version) where direct=true wins over transitive.
  - `analyze_file()` — helper that scans a single .typ file and inserts
    into the import_map HashMap
  - `AnalyzeResult` / `ImportInfo` — serializable types with Serialize
  - `gather_packages()` — original gather logic (download @preview, copy
    @local, resolve transitive deps via network)
  - `find_imports(dir)` — scans all .typ files in a directory (recursive
    WalkDir) for package imports, returns `Vec<PackageSpec>`
  - `collect_imports(node, imports)` — extracts imports from a Typst
    syntax tree
  - All `println!` has been moved to `eprintln!` — stdout is reserved
    for JSON in analyze mode
- `Cargo.toml` — has `serde_json` in both dependencies and dev-dependencies
- `tests/integration.rs` — existing tests plus analyze unit tests (29 in
  lib.rs) and CLI integration tests (9 in integration.rs)

## Goal

Extend `typst-gather analyze` with a `package-cache` config field that
enables full transitive @preview dependency resolution by reading from
local cache directories (no network access). This lets quarto determine
the exact set of packages to stage for compilation.

## What changes from init-config

The init-config plan's `analyze()` does:
1. Scan discover paths → direct imports
2. Scan @local source dirs → transitive @preview imports from local packages
3. Does NOT follow @preview → @preview transitive deps

This plan adds step 3: when `package-cache` is provided, follow @preview
packages into the cache to find their transitive @preview dependencies,
recursively.

## Config format

New field `package-cache`, not used by `gather` (only by `analyze`):

```toml
discover = ["/path/to/document.typ"]
package-cache = "/path/to/extension/typst/packages"

[local]
my-theme = "/path/to/my-theme"
```

Accepts a single path or array of paths (same `StringOrVec` pattern as
`discover`):

```toml
package-cache = [
  "/path/to/quarto/resources/typst/packages",
  "/path/to/extension/typst/packages"
]
```

These point at directories with Typst's standard cache layout:
```
package-cache-dir/
├── preview/
│   ├── cetz/0.4.1/
│   │   ├── typst.toml
│   │   └── lib.typ
│   └── oxifmt/0.2.1/
│       └── ...
└── local/
    └── my-theme/1.0.0/
        └── ...
```

The `rootdir` config field applies to `package-cache` paths the same way
it applies to `discover` and `[local]` paths.

## Output format

Same JSON as init-config, just with more entries when transitive deps are
resolved:

```json
{
  "imports": [
    {
      "namespace": "preview",
      "name": "cetz",
      "version": "0.4.1",
      "source": "document.typ",
      "direct": true
    },
    {
      "namespace": "preview",
      "name": "oxifmt",
      "version": "0.2.1",
      "source": "@preview/cetz:0.4.1",
      "direct": false
    }
  ],
  "files": ["document.typ"]
}
```

Quarto derives staging directories from each import:
`{namespace}/{name}/{version}/` → `preview/cetz/0.4.1/`,
`preview/oxifmt/0.2.1/`.

## Implementation steps

### 1. Add `package-cache` to config (src/lib.rs)

Extend `RawConfig`:

```rust
#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    rootdir: Option<PathBuf>,
    destination: Option<PathBuf>,
    #[serde(default)]
    discover: Option<StringOrVec>,
    #[serde(default, rename = "package-cache")]
    package_cache: Option<StringOrVec>,
    #[serde(default)]
    preview: HashMap<String, String>,
    #[serde(default)]
    local: HashMap<String, String>,
}
```

Extend `Config`:

```rust
#[derive(Debug, Default)]
pub struct Config {
    pub rootdir: Option<PathBuf>,
    pub destination: Option<PathBuf>,
    pub discover: Vec<PathBuf>,
    pub package_cache: Vec<PathBuf>,
    pub preview: HashMap<String, String>,
    pub local: HashMap<String, String>,
}
```

Update `From<RawConfig> for Config` to convert `package_cache` the same way
as `discover`.

`analyze()` already takes `&Config`, so adding `package_cache` to
`Config` makes it automatically available. The return type changes from
`AnalyzeResult` to `Result<AnalyzeResult, String>` because @preview
packages importing @local is now a hard error. The rootdir resolution
for package_cache paths should be added inside `analyze()` alongside
the existing rootdir resolution for discover and local paths.

`run_analyze()` in `main.rs` needs a small update to handle the `Result`
return — map `Err` to an error message on stderr and `ExitCode::FAILURE`.

When `package_cache` is empty (the default), behavior is identical to
the current init-config implementation (no @preview transitive resolution).
All existing tests pass unchanged.

### 2. Implement @preview transitive resolution from cache (src/lib.rs)

Add the resolution loop inside `analyze()`, after the existing direct +
@local-transitive collection but BEFORE converting `import_map` to the
final `Vec`. The code works against `import_map` (the
`HashMap<(String, String, String), ImportInfo>`) for deduplication.

Resolve rootdir for package_cache paths at the top of the new block,
alongside the existing rootdir resolution for discover and local.

Pseudocode:

```rust
let package_cache: Vec<PathBuf> = config
    .package_cache
    .iter()
    .map(|p| match &rootdir {
        Some(root) => root.join(p),
        None => p.clone(),
    })
    .collect();

if !package_cache.is_empty() {
    // Seed queue from all @preview imports found so far.
    // Sort for deterministic output — HashMap iteration order is
    // arbitrary, so without sorting the `source` field for transitive
    // deps would vary between runs.
    let mut seed: Vec<(String, String)> = import_map
        .keys()
        .filter(|(ns, _, _)| ns == "preview")
        .map(|(_, name, ver)| (name.clone(), ver.clone()))
        .collect();
    seed.sort();

    let mut queue: VecDeque<(String, String)> = seed.into_iter().collect();
    let mut processed: HashSet<(String, String)> = queue
        .iter()
        .cloned()
        .collect();

    while let Some((name, version)) = queue.pop_front() {
        // Find package in cache directories (first match)
        for cache_dir in &package_cache {
            let pkg_dir = cache_dir
                .join("preview")
                .join(&name)
                .join(&version);
            if pkg_dir.is_dir() {
                let source_label = format!("@preview/{name}:{version}");
                for dep in find_imports(&pkg_dir) {
                    // @preview packages must not import @local packages.
                    // This indicates a corrupted or hand-edited cache.
                    if dep.namespace == "local" {
                        eprintln!(
                            "Error: @preview/{name}:{version} imports \
                             @local/{dep_name}:{dep_ver} — a published \
                             package cannot depend on local packages",
                            dep_name = dep.name,
                            dep_ver = dep.version,
                        );
                        return Err("@preview package imports @local");
                    }

                    let key = (
                        dep.namespace.to_string(),
                        dep.name.to_string(),
                        dep.version.to_string(),
                    );
                    if !import_map.contains_key(&key) {
                        import_map.insert(key, ImportInfo {
                            namespace: dep.namespace.to_string(),
                            name: dep.name.to_string(),
                            version: dep.version.to_string(),
                            source: source_label.clone(),
                            direct: false,
                        });
                    }
                    let queue_key = (dep.name.to_string(), dep.version.to_string());
                    if processed.insert(queue_key.clone()) {
                        queue.push_back(queue_key);
                    }
                }
                break; // found in this cache, don't check others
            }
        }
        // If not found in any cache: no warning needed, the package is
        // already in the results (just can't resolve its transitive deps)
    }
}

// Convert import_map to final Vec (existing line)
let imports = import_map.into_values().collect();
```

Key design decisions:
- **First cache match wins for scanning**: when a package exists in multiple
  cache paths, we scan the first one found. This is about which .typ files
  we read to discover transitive deps — it doesn't affect override semantics
  at staging time (quarto handles that separately with last-write-wins).
- **@preview → @local is a hard error**: a published @preview package cannot
  depend on a user-local package. If scanning a @preview package in the
  cache yields any @local import, print an error to stderr identifying the
  offending package and import, and return a non-zero exit code. This
  indicates a corrupted or hand-edited cache.
- **Deterministic seed queue**: the BFS queue is seeded from `import_map`
  keys. Because `HashMap` iteration order is arbitrary, the `source` field
  for transitive deps would otherwise be non-deterministic (e.g. if both A
  and B depend on C, whichever is dequeued first determines C's `source`).
  Fix: collect the seed entries into a `Vec`, sort by `(name, version)`,
  then push into the `VecDeque`. This makes output reproducible.
- **No warning for cache miss**: if a @preview package isn't in the cache,
  it's still in the results (it was found by direct or @local scanning),
  we just can't resolve its transitive deps. This is not an error — the
  package might be fetched at compile time.

### 3. Factor shared logic with gather (src/lib.rs)

The existing `gather` path has `scan_deps()` which calls `find_imports()`.
The new analyze cache resolution also calls `find_imports()`. No further
factoring is strictly needed — `find_imports()` is already the shared
piece. Both code paths use it independently.

If the implementations diverge, consider extracting a trait or strategy
pattern later. For now, keep it simple.

## Testing

All tests use tempdir fixtures. Cache directories are created with the
standard `preview/{name}/{version}/` layout containing .typ files and
optionally typst.toml manifests.

### Unit tests for @preview transitive resolution

#### Basic cache resolution

- **cache_resolves_transitive_preview**: document.typ imports
  @preview/cetz:0.4.1. Create cache with:
  - `preview/cetz/0.4.1/lib.typ` containing `#import "@preview/oxifmt:0.2.1"`
  - `preview/oxifmt/0.2.1/lib.typ` (no imports)
  Call `analyze()` with config.package_cache pointing at cache dir.
  Expect cetz (direct=true, source="document.typ") and oxifmt (direct=false,
  source="@preview/cetz:0.4.1").

- **cache_resolves_deep_chain**: create cache with packages A→B→C→D (each
  imports the next). Document imports A. All four appear in output.
  A: direct=true. B: source="@preview/A:...". C: source="@preview/B:...".
  D: source="@preview/C:...".

- **cache_handles_diamond_dependency**: A imports B and C. Both B and C
  import D. Create cache accordingly. D appears once in output
  (deduplicated). Source is deterministic due to sorted seed queue —
  the package that sorts first alphabetically by (name, version)
  determines D's source.

- **cache_handles_cycle**: cache has A importing B and B importing A.
  No infinite loop. Both appear in output exactly once.

#### @preview → @local is a hard error

- **cache_preview_imports_local_is_error**: cache has @preview/foo:1.0.0
  with a .typ file importing @local/bar:2.0.0. Document imports
  @preview/foo. `analyze()` returns an error. Error message on stderr
  identifies the offending package (@preview/foo:1.0.0) and the @local
  import (@local/bar:2.0.0). Exit code is non-zero.

- **cache_preview_imports_local_deep_is_error**: A→B, B imports
  @local/bar. Error is raised when processing B, not silently ignored.
  The error identifies B as the offending package.

#### Missing packages in cache

- **cache_missing_package_no_crash**: document imports @preview/foo:1.0.0.
  Cache does NOT contain preview/foo/1.0.0/. Foo still appears in output
  (direct=true) but its transitive deps can't be resolved. No crash,
  no warning on stderr (cache miss is expected).

- **cache_partial_chain**: A→B→C. Cache has A and B but not C. Output
  contains A (direct), B (transitive, source=A), C (transitive, source=B).
  C's own transitive deps are unknown but C itself is listed because B
  imported it.

#### Multiple cache paths

- **multiple_caches_first_match_scanned**: same package exists in both
  cache paths with different .typ contents (different transitive deps).
  First cache's version is scanned for transitive deps. Document that
  this is first-match-wins for scanning (not related to quarto's
  last-write-wins staging semantics — those are separate concerns).

- **multiple_caches_different_packages**: cache A has preview/cetz/,
  cache B has preview/fletcher/. Document imports both. Both resolved
  from their respective caches.

- **multiple_caches_fallthrough**: package not in first cache, found in
  second. Resolved correctly from second cache.

#### Interaction with @local transitive deps

- **local_dep_then_cache_follows**: [local] config maps my-pkg to source
  dir. Source dir has .typ importing @preview/cetz:0.4.1. Cache has
  preview/cetz/0.4.1/ importing @preview/oxifmt:0.2.1. Output contains:
  my-pkg (direct, local), cetz (transitive from @local/my-pkg), oxifmt
  (transitive from @preview/cetz:0.4.1).

- **local_and_direct_preview_merged**: document directly imports
  @preview/cetz:0.4.1 AND @local/my-pkg also imports @preview/cetz:0.4.1.
  Cetz appears once with direct=true. Its transitive deps from cache
  are still resolved (the dedup doesn't skip cache scanning).

#### No cache (init-config behavior preserved)

- **no_cache_no_preview_resolution**: package_cache is empty (default).
  Document imports @preview/cetz. Cetz appears but its transitive deps
  do NOT. This is identical to init-config behavior — verify no regression.

- **no_cache_local_still_resolved**: package_cache is empty but [local]
  has entries. @local → @preview transitive deps still found (reading
  source dirs directly). Only @preview → @preview recursion is gated
  on cache.

#### Cache directory structure edge cases

- **cache_ignores_non_typ_files**: cache package dir has .json, .toml,
  .txt files alongside .typ files. Only .typ files scanned for imports.

- **cache_scans_subdirectories**: package dir in cache has nested `src/`
  subdirectory with .typ files. These ARE scanned (find_imports uses
  WalkDir which is recursive within the package dir).

- **empty_cache_package_dir**: preview/cetz/0.4.1/ directory exists in
  cache but contains no .typ files. No transitive deps found, no crash.

- **cache_with_invalid_typ**: a .typ file in cache that is syntactically
  broken. Typst's parser is lenient (returns partial tree). Should not
  crash. May find no imports from that file — that's fine.

### Integration tests (CLI as subprocess)

These invoke the compiled `typst-gather` binary to test actual CLI behavior
with cache support.

#### End-to-end with cache

- **e2e_staging_scenario**: full quarto staging simulation.
  1. Create temp dir:
     ```
     cache/
       preview/
         cetz/0.4.1/
           lib.typ         (#import "@preview/oxifmt:0.2.1")
         oxifmt/0.2.1/
           lib.typ         (no imports)
     doc/
       document.typ        (#import "@preview/cetz:0.4.1")
     ```
  2. Write TOML config: `discover = ["doc/document.typ"]`,
     `package-cache = ["cache/"]`
  3. Run `typst-gather analyze <config-file>` (or pipe on stdin)
  4. Parse JSON stdout
  5. Verify: cetz (direct=true), oxifmt (direct=false,
     source="@preview/cetz:0.4.1")
  6. Verify: consumer can derive staging dirs `preview/cetz/0.4.1/`
     and `preview/oxifmt/0.2.1/` from the output

- **e2e_staging_with_local**: document imports @preview/cetz and
  @local/my-theme. [local] maps my-theme to source dir. Source dir
  imports @preview/fontawesome. Cache has preview/cetz/ (with transitive
  dep on oxifmt) and preview/fontawesome/. Output has all five:
  cetz (direct), my-theme (direct, local), fontawesome (transitive from
  @local/my-theme), oxifmt (transitive from @preview/cetz).

- **e2e_no_package_cache**: config has discover but no package-cache field.
  Analyze succeeds with direct imports only. Exit code 0.

- **e2e_multiple_caches**: two cache directories. Package A in cache 1,
  package B in cache 2. Document imports both. Both resolved correctly.

- **e2e_cache_via_stdin**: pipe config with package-cache field on stdin
  (`typst-gather analyze -`). Verify same results as file mode.

#### Edge cases

- **large_dependency_tree**: create cache with 15 packages in a chain
  (A→B→C→...→O). Document imports A. All 15 appear. No timeout or
  stack overflow.

- **package_cache_path_nonexistent**: package-cache contains a path that
  doesn't exist on disk. Warning on stderr. Analysis continues — direct
  imports still reported, cache resolution skipped for that path.

- **package_cache_path_is_file**: package-cache points at a file, not
  a directory. Warning on stderr, that path skipped. Other cache paths
  (if any) still checked.

- **rootdir_applies_to_package_cache**: config has rootdir="sub" and
  package-cache="cache". Resolves to sub/cache/. Verify transitive
  resolution works with the resolved path.

- **e2e_preview_imports_local_error**: cache has @preview/foo:1.0.0
  importing @local/bar:1.0.0. Document imports @preview/foo. Run
  `typst-gather analyze <config>`. Exit code is non-zero. Stderr
  contains error message identifying @preview/foo and @local/bar.
  Stdout is empty (no partial JSON output).

### Config parsing tests

- **package_cache_single_string**: `package-cache = "/path/to/cache"`.
  Parsed as single-element Vec.

- **package_cache_array**: `package-cache = ["/a", "/b"]`. Parsed as
  two-element Vec.

- **package_cache_absent**: no package-cache field. Parsed as empty Vec.
  (Default behavior, no @preview transitive resolution.)

- **package_cache_with_rootdir**: rootdir + relative package-cache path.
  Resolved correctly.

## Quarto-side changes (out of scope, noted for context)

See `~/src/quarto-cli/plans/typst-gather-staging.md` for the quarto
changes that consume this feature:
- `stageTypstPackages()` calls `typst-gather analyze` with package-cache
  pointing at built-in + extension package dirs
- Parses JSON to get exact package list
- Stages only needed `{namespace}/{name}/{version}/` dirs (last-write-wins)
- Falls back to copy-everything if analyze fails or binary unavailable
