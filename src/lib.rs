//! typst-gather: Gather Typst packages locally for offline/hermetic builds.

use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use ecow::EcoString;
use globset::{Glob, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use typst_kit::download::{Downloader, ProgressSink};
use typst_kit::package::PackageStorage;
use typst_syntax::ast;
use typst_syntax::package::{PackageManifest, PackageSpec, PackageVersion};
use typst_syntax::SyntaxNode;
use walkdir::WalkDir;

/// Statistics about gathering operations.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub downloaded: usize,
    pub copied: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// Result of a gather operation.
#[derive(Debug, Default)]
pub struct GatherResult {
    pub stats: Stats,
    /// @local imports discovered during scanning that are not configured in [local] section.
    /// Each entry is (package_name, source_file_path).
    pub unconfigured_local: Vec<(String, String)>,
}

/// TOML configuration format.
///
/// ```toml
/// destination = "/path/to/packages"
/// discover = ["/path/to/templates", "/path/to/other.typ"]
///
/// [preview]
/// cetz = "0.4.1"
/// fletcher = "0.5.3"
///
/// [local]
/// my-pkg = "/path/to/pkg"
/// ```
/// Helper enum for deserializing string or array of strings
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StringOrVec {
    Single(String),
    Multiple(Vec<String>),
}

impl Default for StringOrVec {
    fn default() -> Self {
        StringOrVec::Multiple(Vec::new())
    }
}

impl From<StringOrVec> for Vec<PathBuf> {
    fn from(value: StringOrVec) -> Self {
        match value {
            StringOrVec::Single(s) => vec![PathBuf::from(s)],
            StringOrVec::Multiple(v) => v.into_iter().map(PathBuf::from).collect(),
        }
    }
}

/// Raw config for deserialization
#[derive(Debug, Deserialize, Default)]
struct RawConfig {
    /// Root directory for resolving relative paths (discover, destination)
    rootdir: Option<PathBuf>,
    destination: Option<PathBuf>,
    #[serde(default)]
    discover: Option<StringOrVec>,
    #[serde(default)]
    preview: HashMap<String, String>,
    #[serde(default)]
    local: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Config {
    /// Root directory for resolving relative paths (discover, destination).
    /// If set, discover and destination paths are resolved relative to this.
    pub rootdir: Option<PathBuf>,
    /// Destination directory for gathered packages
    pub destination: Option<PathBuf>,
    /// Paths to scan for imports. Can be directories (scans .typ files) or individual .typ files.
    /// Accepts either a single path or an array of paths.
    pub discover: Vec<PathBuf>,
    pub preview: HashMap<String, String>,
    pub local: HashMap<String, String>,
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        Config {
            rootdir: raw.rootdir,
            destination: raw.destination,
            discover: raw.discover.map(Into::into).unwrap_or_default(),
            preview: raw.preview,
            local: raw.local,
        }
    }
}

/// A resolved package entry ready for gathering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageEntry {
    Preview { name: String, version: String },
    Local { name: String, dir: PathBuf },
}

impl Config {
    /// Parse a TOML configuration string.
    pub fn parse(content: &str) -> Result<Self, toml::de::Error> {
        let raw: RawConfig = toml::from_str(content)?;
        Ok(raw.into())
    }

    /// Convert config into a list of package entries.
    pub fn into_entries(self) -> Vec<PackageEntry> {
        let mut entries = Vec::new();

        for (name, version) in self.preview {
            entries.push(PackageEntry::Preview { name, version });
        }

        for (name, dir) in self.local {
            entries.push(PackageEntry::Local {
                name,
                dir: PathBuf::from(dir),
            });
        }

        entries
    }
}

/// Context for gathering operations, holding shared state.
struct GatherContext<'a> {
    storage: PackageStorage,
    dest: &'a Path,
    configured_local: &'a HashSet<String>,
    processed: HashSet<String>,
    stats: Stats,
    /// @local imports discovered during scanning (name -> source_file)
    discovered_local: HashMap<String, String>,
}

impl<'a> GatherContext<'a> {
    fn new(dest: &'a Path, configured_local: &'a HashSet<String>) -> Self {
        Self {
            storage: PackageStorage::new(
                Some(dest.to_path_buf()),
                None,
                Downloader::new("typst-gather/0.1.0"),
            ),
            dest,
            configured_local,
            processed: HashSet::new(),
            stats: Stats::default(),
            discovered_local: HashMap::new(),
        }
    }
}

/// Gather packages to the destination directory.
pub fn gather_packages(
    dest: &Path,
    entries: Vec<PackageEntry>,
    discover_paths: &[PathBuf],
    configured_local: &HashSet<String>,
) -> GatherResult {
    let mut ctx = GatherContext::new(dest, configured_local);

    // First, process discover paths
    for path in discover_paths {
        discover_imports(&mut ctx, path);
    }

    // Then process explicit entries
    for entry in entries {
        match entry {
            PackageEntry::Preview { name, version } => {
                cache_preview(&mut ctx, &name, &version);
            }
            PackageEntry::Local { name, dir } => {
                gather_local(&mut ctx, &name, &dir);
            }
        }
    }

    // Find @local imports that aren't configured
    let unconfigured_local: Vec<(String, String)> = ctx
        .discovered_local
        .into_iter()
        .filter(|(name, _)| !ctx.configured_local.contains(name))
        .collect();

    GatherResult {
        stats: ctx.stats,
        unconfigured_local,
    }
}

/// Scan a path for imports. If it's a directory, scans .typ files in it (non-recursive).
/// If it's a file, scans that file directly.
fn discover_imports(ctx: &mut GatherContext, path: &Path) {
    if path.is_file() {
        // Single file
        if path.extension().is_some_and(|e| e == "typ") {
            eprintln!("Discovering imports in {}...", display_path(path));
            scan_file_for_imports(ctx, path);
        }
    } else if path.is_dir() {
        // Directory - scan .typ files (non-recursive)
        eprintln!("Discovering imports in {}...", display_path(path));

        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("  Failed to read directory: {e}");
                ctx.stats.failed += 1;
                return;
            }
        };

        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.is_file() && file_path.extension().is_some_and(|e| e == "typ") {
                scan_file_for_imports(ctx, &file_path);
            }
        }
    } else {
        eprintln!(
            "Warning: discover path does not exist: {}",
            display_path(path)
        );
    }
}

/// Scan a single .typ file for @preview and @local imports.
/// @preview imports are cached, @local imports are tracked for later warning.
fn scan_file_for_imports(ctx: &mut GatherContext, path: &Path) {
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut imports = Vec::new();
        collect_imports(&typst_syntax::parse(&content), &mut imports);

        let source_file = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());

        for spec in imports {
            if spec.namespace == "preview" {
                cache_preview_with_deps(ctx, &spec);
            } else if spec.namespace == "local" {
                // Track @local imports (only first occurrence per package name)
                ctx.discovered_local
                    .entry(spec.name.to_string())
                    .or_insert(source_file.clone());
            }
        }
    }
}

fn cache_preview(ctx: &mut GatherContext, name: &str, version_str: &str) {
    let Ok(version): Result<PackageVersion, _> = version_str.parse() else {
        eprintln!("Invalid version '{version_str}' for @preview/{name}");
        ctx.stats.failed += 1;
        return;
    };

    let spec = PackageSpec {
        namespace: EcoString::from("preview"),
        name: EcoString::from(name),
        version,
    };

    cache_preview_with_deps(ctx, &spec);
}

/// Default exclude patterns for local packages (common non-package files).
const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    ".git/**",
    ".github",
    ".github/**",
    ".gitignore",
    ".gitattributes",
    ".vscode",
    ".vscode/**",
    ".idea",
    ".idea/**",
    "*.bak",
    "*.swp",
    "*~",
];

fn gather_local(ctx: &mut GatherContext, name: &str, src_dir: &Path) {
    // Read typst.toml to get version (and validate name)
    let manifest_path = src_dir.join("typst.toml");
    let manifest: PackageManifest = match std::fs::read_to_string(&manifest_path)
        .map_err(|e| e.to_string())
        .and_then(|s| toml::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error reading typst.toml for @local/{name}: {e}");
            ctx.stats.failed += 1;
            return;
        }
    };

    // Validate name matches
    if manifest.package.name.as_str() != name {
        eprintln!(
            "Name mismatch for @local/{name}: typst.toml has '{}'",
            manifest.package.name
        );
        ctx.stats.failed += 1;
        return;
    }

    let version = manifest.package.version;
    let dest_dir = ctx.dest.join(format!("local/{name}/{version}"));

    eprintln!("Copying @local/{name}:{version}...");

    // Clean slate: remove destination if exists
    if dest_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dest_dir) {
            eprintln!("  Failed to remove existing dir: {e}");
            ctx.stats.failed += 1;
            return;
        }
    }

    // Build exclude pattern matcher from defaults + manifest excludes
    let mut builder = GlobSetBuilder::new();
    for pattern in DEFAULT_EXCLUDES {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    // Add manifest excludes if present
    for pattern in &manifest.package.exclude {
        if let Ok(glob) = Glob::new(pattern.as_str()) {
            builder.add(glob);
        }
    }
    let excludes = builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap());

    // Copy files, respecting exclude patterns
    if let Err(e) = copy_filtered(src_dir, &dest_dir, &excludes) {
        eprintln!("  Failed to copy: {e}");
        ctx.stats.failed += 1;
        return;
    }

    eprintln!("  -> {}", display_path(&dest_dir));
    ctx.stats.copied += 1;

    // Mark as processed
    let spec = PackageSpec {
        namespace: EcoString::from("local"),
        name: EcoString::from(name),
        version,
    };
    ctx.processed.insert(spec.to_string());

    // Scan for @preview dependencies
    scan_deps(ctx, &dest_dir);
}

/// Copy directory contents, excluding files that match the exclude patterns.
fn copy_filtered(src: &Path, dest: &Path, excludes: &globset::GlobSet) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;

    for entry in WalkDir::new(src).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let relative = path.strip_prefix(src).unwrap_or(path);

        // Check if this path matches any exclude pattern
        if excludes.is_match(relative) {
            continue;
        }

        let dest_path = dest.join(relative);

        if path.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
        } else if path.is_file() {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &dest_path)?;
        }
    }

    Ok(())
}

fn cache_preview_with_deps(ctx: &mut GatherContext, spec: &PackageSpec) {
    // Skip @preview packages that are configured as @local (use local version instead)
    if ctx.configured_local.contains(spec.name.as_str()) {
        return;
    }

    let key = spec.to_string();
    if !ctx.processed.insert(key) {
        return;
    }

    let subdir = format!("{}/{}/{}", spec.namespace, spec.name, spec.version);
    let cached_path = ctx.storage.package_cache_path().map(|p| p.join(&subdir));

    if cached_path.as_ref().is_some_and(|p| p.exists()) {
        eprintln!("Skipping {spec} (cached)");
        ctx.stats.skipped += 1;
        scan_deps(ctx, cached_path.as_ref().unwrap());
        return;
    }

    eprintln!("Downloading {spec}...");
    match ctx.storage.prepare_package(spec, &mut ProgressSink) {
        Ok(path) => {
            eprintln!("  -> {}", display_path(&path));
            ctx.stats.downloaded += 1;
            scan_deps(ctx, &path);
        }
        Err(e) => {
            eprintln!("  Failed: {e:?}");
            ctx.stats.failed += 1;
        }
    }
}

fn scan_deps(ctx: &mut GatherContext, dir: &Path) {
    for spec in find_imports(dir) {
        if spec.namespace == "preview" {
            cache_preview_with_deps(ctx, &spec);
        }
    }
}

/// Display a path relative to the current working directory.
fn display_path(path: &Path) -> String {
    if let Ok(cwd) = env::current_dir() {
        if let Ok(relative) = path.strip_prefix(&cwd) {
            return relative.display().to_string();
        }
    }
    path.display().to_string()
}

/// Find all package imports in `.typ` files under a directory.
pub fn find_imports(dir: &Path) -> Vec<PackageSpec> {
    let mut imports = Vec::new();
    for entry in WalkDir::new(dir).into_iter().flatten() {
        if entry.path().extension().is_some_and(|e| e == "typ") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                collect_imports(&typst_syntax::parse(&content), &mut imports);
            }
        }
    }
    imports
}

/// Extract package imports from a Typst syntax tree.
pub fn collect_imports(node: &SyntaxNode, imports: &mut Vec<PackageSpec>) {
    if let Some(import) = node.cast::<ast::ModuleImport>() {
        if let Some(spec) = try_extract_spec(import.source()) {
            imports.push(spec);
        }
    }
    if let Some(include) = node.cast::<ast::ModuleInclude>() {
        if let Some(spec) = try_extract_spec(include.source()) {
            imports.push(spec);
        }
    }
    for child in node.children() {
        collect_imports(child, imports);
    }
}

/// Try to extract a PackageSpec from an expression (if it's an `@namespace/name:version` string).
pub fn try_extract_spec(expr: ast::Expr) -> Option<PackageSpec> {
    if let ast::Expr::Str(s) = expr {
        let val = s.get();
        if val.starts_with('@') {
            return val.parse().ok();
        }
    }
    None
}

/// Result of an analyze operation.
#[derive(Debug, Serialize)]
pub struct AnalyzeResult {
    pub imports: Vec<ImportInfo>,
    pub files: Vec<String>,
}

/// Information about a discovered import.
#[derive(Debug, Serialize, Clone)]
pub struct ImportInfo {
    pub namespace: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub direct: bool,
}

/// Analyze imports without downloading or copying anything.
///
/// Scans discover paths for @preview and @local imports, then follows
/// transitive @preview deps inside @local package source directories.
pub fn analyze(config: &Config) -> AnalyzeResult {
    let rootdir = config.rootdir.clone();
    let discover: Vec<PathBuf> = config
        .discover
        .iter()
        .map(|p| match &rootdir {
            Some(root) => root.join(p),
            None => p.clone(),
        })
        .collect();

    // (namespace, name, version) -> ImportInfo, for deduplication
    let mut import_map: HashMap<(String, String, String), ImportInfo> = HashMap::new();
    let mut files: Vec<String> = Vec::new();

    // Scan discover paths
    for path in &discover {
        if path.is_file() {
            if path.extension().is_some_and(|e| e == "typ") {
                eprintln!("Analyzing imports in {}...", display_path(path));
                let filename = path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                if !files.contains(&filename) {
                    files.push(filename.clone());
                }
                analyze_file(path, &filename, true, &mut import_map);
            }
        } else if path.is_dir() {
            eprintln!("Analyzing imports in {}...", display_path(path));
            let entries = match std::fs::read_dir(path) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("  Failed to read directory: {e}");
                    continue;
                }
            };
            for entry in entries.flatten() {
                let file_path = entry.path();
                if file_path.is_file() && file_path.extension().is_some_and(|e| e == "typ") {
                    let filename = file_path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| file_path.display().to_string());
                    if !files.contains(&filename) {
                        files.push(filename.clone());
                    }
                    analyze_file(&file_path, &filename, true, &mut import_map);
                }
            }
        } else {
            eprintln!(
                "Warning: discover path does not exist: {}",
                display_path(path)
            );
        }
    }

    // Process @local packages for transitive deps
    for (name, dir_str) in &config.local {
        let dir = match &rootdir {
            Some(root) => root.join(dir_str),
            None => PathBuf::from(dir_str),
        };

        // Read typst.toml to get version
        let manifest_path = dir.join("typst.toml");
        let manifest: Option<PackageManifest> = match std::fs::read_to_string(&manifest_path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str(&s).map_err(|e| e.to_string()))
        {
            Ok(m) => Some(m),
            Err(e) => {
                if dir.exists() {
                    eprintln!("Warning: could not read typst.toml for @local/{name}: {e}");
                } else {
                    eprintln!(
                        "Warning: source directory does not exist for @local/{name}: {}",
                        display_path(&dir)
                    );
                }
                None
            }
        };

        // If we have a manifest, add an @local import entry with its version
        // and scan for transitive @preview deps
        if let Some(ref manifest) = manifest {
            let version = manifest.package.version.to_string();
            let key = ("local".to_string(), name.clone(), version.clone());
            // Only add if not already present as direct import
            import_map.entry(key).or_insert(ImportInfo {
                namespace: "local".to_string(),
                name: name.clone(),
                version,
                source: format!("@local/{name}"),
                direct: false,
            });

            // Scan source dir for transitive @preview imports
            let source_label = format!("@local/{name}");
            for spec in find_imports(&dir) {
                let key = (
                    spec.namespace.to_string(),
                    spec.name.to_string(),
                    spec.version.to_string(),
                );
                let entry = import_map.entry(key);
                match entry {
                    std::collections::hash_map::Entry::Occupied(_) => {
                        // keep existing (direct wins over transitive)
                    }
                    std::collections::hash_map::Entry::Vacant(e) => {
                        e.insert(ImportInfo {
                            namespace: spec.namespace.to_string(),
                            name: spec.name.to_string(),
                            version: spec.version.to_string(),
                            source: source_label.clone(),
                            direct: false,
                        });
                    }
                }
            }
        }
    }

    let imports = import_map.into_values().collect();
    AnalyzeResult { imports, files }
}

/// Analyze a single .typ file for imports, adding them to the import map.
fn analyze_file(
    path: &Path,
    source: &str,
    direct: bool,
    import_map: &mut HashMap<(String, String, String), ImportInfo>,
) {
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut imports = Vec::new();
        collect_imports(&typst_syntax::parse(&content), &mut imports);

        for spec in imports {
            let key = (
                spec.namespace.to_string(),
                spec.name.to_string(),
                spec.version.to_string(),
            );
            let entry = import_map.entry(key);
            match entry {
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    // direct wins over transitive
                    if direct && !e.get().direct {
                        e.get_mut().direct = true;
                        e.get_mut().source = source.to_string();
                    }
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(ImportInfo {
                        namespace: spec.namespace.to_string(),
                        name: spec.name.to_string(),
                        version: spec.version.to_string(),
                        source: source.to_string(),
                        direct,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod config_parsing {
        use super::*;

        #[test]
        fn empty_config() {
            let config = Config::parse("").unwrap();
            assert!(config.destination.is_none());
            assert!(config.discover.is_empty());
            assert!(config.preview.is_empty());
            assert!(config.local.is_empty());
        }

        #[test]
        fn destination_only() {
            let toml = r#"destination = "/path/to/cache""#;
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.destination, Some(PathBuf::from("/path/to/cache")));
            assert!(config.discover.is_empty());
            assert!(config.preview.is_empty());
            assert!(config.local.is_empty());
        }

        #[test]
        fn with_discover_string() {
            let toml = r#"
destination = "/cache"
discover = "/path/to/templates"
"#;
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.destination, Some(PathBuf::from("/cache")));
            assert_eq!(config.discover, vec![PathBuf::from("/path/to/templates")]);
        }

        #[test]
        fn with_discover_array() {
            let toml = r#"
destination = "/cache"
discover = ["/path/to/templates", "template.typ", "other.typ"]
"#;
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.destination, Some(PathBuf::from("/cache")));
            assert_eq!(
                config.discover,
                vec![
                    PathBuf::from("/path/to/templates"),
                    PathBuf::from("template.typ"),
                    PathBuf::from("other.typ"),
                ]
            );
        }

        #[test]
        fn preview_only() {
            let toml = r#"
destination = "/cache"

[preview]
cetz = "0.4.1"
fletcher = "0.5.3"
"#;
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.destination, Some(PathBuf::from("/cache")));
            assert_eq!(config.preview.len(), 2);
            assert_eq!(config.preview.get("cetz"), Some(&"0.4.1".to_string()));
            assert_eq!(config.preview.get("fletcher"), Some(&"0.5.3".to_string()));
            assert!(config.local.is_empty());
        }

        #[test]
        fn local_only() {
            let toml = r#"
destination = "/cache"

[local]
my-pkg = "/path/to/pkg"
other = "../relative/path"
"#;
            let config = Config::parse(toml).unwrap();
            assert!(config.preview.is_empty());
            assert_eq!(config.local.len(), 2);
            assert_eq!(
                config.local.get("my-pkg"),
                Some(&"/path/to/pkg".to_string())
            );
            assert_eq!(
                config.local.get("other"),
                Some(&"../relative/path".to_string())
            );
        }

        #[test]
        fn mixed_config() {
            let toml = r#"
destination = "/cache"

[preview]
cetz = "0.4.1"

[local]
my-pkg = "/path/to/pkg"
"#;
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.destination, Some(PathBuf::from("/cache")));
            assert_eq!(config.preview.len(), 1);
            assert_eq!(config.local.len(), 1);
        }

        #[test]
        fn into_entries() {
            let toml = r#"
destination = "/cache"

[preview]
cetz = "0.4.1"

[local]
my-pkg = "/path/to/pkg"
"#;
            let config = Config::parse(toml).unwrap();
            let entries = config.into_entries();
            assert_eq!(entries.len(), 2);

            let has_preview = entries.iter().any(|e| {
                matches!(e, PackageEntry::Preview { name, version }
                    if name == "cetz" && version == "0.4.1")
            });
            let has_local = entries.iter().any(|e| {
                matches!(e, PackageEntry::Local { name, dir }
                    if name == "my-pkg" && dir == Path::new("/path/to/pkg"))
            });
            assert!(has_preview);
            assert!(has_local);
        }

        #[test]
        fn invalid_toml() {
            let result = Config::parse("not valid toml [[[");
            assert!(result.is_err());
        }

        #[test]
        fn extra_fields_ignored() {
            let toml = r#"
destination = "/cache"

[preview]
cetz = "0.4.1"

[unknown_section]
foo = "bar"
"#;
            // Should not error on unknown sections
            let config = Config::parse(toml).unwrap();
            assert_eq!(config.preview.len(), 1);
        }
    }

    mod import_parsing {
        use super::*;

        fn parse_imports(code: &str) -> Vec<PackageSpec> {
            let mut imports = Vec::new();
            collect_imports(&typst_syntax::parse(code), &mut imports);
            imports
        }

        #[test]
        fn simple_import() {
            let imports = parse_imports(r#"#import "@preview/cetz:0.4.1""#);
            assert_eq!(imports.len(), 1);
            assert_eq!(imports[0].namespace, "preview");
            assert_eq!(imports[0].name, "cetz");
            assert_eq!(imports[0].version.to_string(), "0.4.1");
        }

        #[test]
        fn import_with_items() {
            let imports = parse_imports(r#"#import "@preview/cetz:0.4.1": canvas, draw"#);
            assert_eq!(imports.len(), 1);
            assert_eq!(imports[0].name, "cetz");
        }

        #[test]
        fn multiple_imports() {
            let code = r#"
#import "@preview/cetz:0.4.1"
#import "@preview/fletcher:0.5.3"
"#;
            let imports = parse_imports(code);
            assert_eq!(imports.len(), 2);
        }

        #[test]
        fn include_statement() {
            let imports = parse_imports(r#"#include "@preview/template:1.0.0""#);
            assert_eq!(imports.len(), 1);
            assert_eq!(imports[0].name, "template");
        }

        #[test]
        fn local_import_ignored_in_extract() {
            // Local imports are valid but won't be recursively fetched
            let imports = parse_imports(r#"#import "@local/my-pkg:1.0.0""#);
            assert_eq!(imports.len(), 1);
            assert_eq!(imports[0].namespace, "local");
        }

        #[test]
        fn relative_import_ignored() {
            let imports = parse_imports(r#"#import "utils.typ""#);
            assert_eq!(imports.len(), 0);
        }

        #[test]
        fn no_imports() {
            let imports = parse_imports(r#"= Hello World"#);
            assert_eq!(imports.len(), 0);
        }

        #[test]
        fn nested_in_function() {
            let code = r#"
#let setup() = {
  import "@preview/cetz:0.4.1"
}
"#;
            let imports = parse_imports(code);
            assert_eq!(imports.len(), 1);
        }

        #[test]
        fn invalid_package_spec_ignored() {
            // Missing version
            let imports = parse_imports(r#"#import "@preview/cetz""#);
            assert_eq!(imports.len(), 0);
        }

        #[test]
        fn complex_document() {
            let code = r#"
#import "@preview/cetz:0.4.1": canvas
#import "@preview/fletcher:0.5.3": diagram, node, edge
#import "local-file.typ": helper

= My Document

#include "@preview/template:1.0.0"

Some content here.

#let f() = {
  import "@preview/codly:1.2.0"
}
"#;
            let imports = parse_imports(code);
            assert_eq!(imports.len(), 4);

            let names: Vec<_> = imports.iter().map(|s| s.name.as_str()).collect();
            assert!(names.contains(&"cetz"));
            assert!(names.contains(&"fletcher"));
            assert!(names.contains(&"template"));
            assert!(names.contains(&"codly"));
        }
    }

    mod stats {
        use super::*;

        #[test]
        fn default_stats() {
            let stats = Stats::default();
            assert_eq!(stats.downloaded, 0);
            assert_eq!(stats.copied, 0);
            assert_eq!(stats.skipped, 0);
            assert_eq!(stats.failed, 0);
        }
    }

    mod local_override {
        use super::*;

        /// When a package is configured in [local], @preview imports of the same
        /// package name should be skipped. This handles the case where a local
        /// package contains template examples that import from @preview.
        #[test]
        fn configured_local_contains_check() {
            let mut configured_local = HashSet::new();
            configured_local.insert("my-pkg".to_string());
            configured_local.insert("other-pkg".to_string());

            // These should be skipped (configured as local)
            assert!(configured_local.contains("my-pkg"));
            assert!(configured_local.contains("other-pkg"));

            // These should NOT be skipped (not configured)
            assert!(!configured_local.contains("cetz"));
            assert!(!configured_local.contains("fletcher"));
        }
    }

    mod copy_filtering {
        use super::*;

        #[test]
        fn default_excludes_match_git() {
            let mut builder = GlobSetBuilder::new();
            for pattern in DEFAULT_EXCLUDES {
                builder.add(Glob::new(pattern).unwrap());
            }
            let excludes = builder.build().unwrap();

            // Should match .git and contents
            assert!(excludes.is_match(".git"));
            assert!(excludes.is_match(".git/config"));
            assert!(excludes.is_match(".git/objects/pack/foo"));

            // Should match .github
            assert!(excludes.is_match(".github"));
            assert!(excludes.is_match(".github/workflows/ci.yml"));

            // Should match editor files
            assert!(excludes.is_match(".gitignore"));
            assert!(excludes.is_match("foo.bak"));
            assert!(excludes.is_match("foo.swp"));
            assert!(excludes.is_match("foo~"));

            // Should NOT match normal files
            assert!(!excludes.is_match("lib.typ"));
            assert!(!excludes.is_match("typst.toml"));
            assert!(!excludes.is_match("src/main.typ"));
            assert!(!excludes.is_match("template/main.typ"));
        }
    }

    mod analyze_tests {
        use super::*;
        use std::fs;
        use tempfile::TempDir;

        fn create_local_package(dir: &Path, name: &str, version: &str, typ_content: Option<&str>) {
            fs::create_dir_all(dir).unwrap();
            let manifest = format!(
                "[package]\nname = \"{name}\"\nversion = \"{version}\"\nentrypoint = \"lib.typ\"\n"
            );
            fs::write(dir.join("typst.toml"), manifest).unwrap();
            fs::write(
                dir.join("lib.typ"),
                typ_content.unwrap_or("// Empty package\n"),
            )
            .unwrap();
        }

        #[test]
        fn single_preview_import() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@preview/cetz:0.4.1""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.imports[0].namespace, "preview");
            assert_eq!(result.imports[0].name, "cetz");
            assert_eq!(result.imports[0].version, "0.4.1");
            assert_eq!(result.imports[0].source, "template.typ");
            assert!(result.imports[0].direct);
        }

        #[test]
        fn single_local_import() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(&pkg_dir, "my-pkg", "1.0.0", None);

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            let local_import = result
                .imports
                .iter()
                .find(|i| i.namespace == "local" && i.name == "my-pkg")
                .expect("should find @local/my-pkg");
            assert_eq!(local_import.version, "1.0.0");
            assert!(local_import.direct);
            assert_eq!(local_import.source, "template.typ");
        }

        #[test]
        fn multiple_imports_single_file() {
            let dir = TempDir::new().unwrap();
            let content = r#"
#import "@preview/cetz:0.4.1"
#import "@preview/fletcher:0.5.3"
#import "@local/my-pkg:1.0.0"
"#;
            fs::write(dir.path().join("template.typ"), content).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 3);
        }

        #[test]
        fn multiple_files() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@preview/cetz:0.4.1""#,
            )
            .unwrap();
            fs::write(
                dir.path().join("show.typ"),
                r#"#import "@preview/fletcher:0.5.3""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().to_path_buf()],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 2);
            assert_eq!(result.files.len(), 2);
        }

        #[test]
        fn import_with_items() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@preview/cetz:0.4.1": canvas, draw"#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.imports[0].name, "cetz");
        }

        #[test]
        fn include_statement() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#include "@preview/template:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.imports[0].name, "template");
        }

        #[test]
        fn nested_import_in_function() {
            let dir = TempDir::new().unwrap();
            let code = "#let f() = {\n  import \"@preview/cetz:0.4.1\"\n}\n";
            fs::write(dir.path().join("template.typ"), code).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
        }

        #[test]
        fn relative_import_ignored() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("template.typ"), r#"#import "utils.typ""#).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert!(result.imports.is_empty());
        }

        #[test]
        fn no_imports() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("template.typ"), "= Hello World").unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert!(result.imports.is_empty());
            assert_eq!(result.files, vec!["template.typ"]);
        }

        #[test]
        fn invalid_package_spec_ignored() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@preview/cetz""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert!(result.imports.is_empty());
        }

        #[test]
        fn duplicate_import_same_file() {
            let dir = TempDir::new().unwrap();
            let content = "#import \"@preview/cetz:0.4.1\"\n#import \"@preview/cetz:0.4.1\"\n";
            fs::write(dir.path().join("template.typ"), content).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
        }

        #[test]
        fn duplicate_import_across_files() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();
            fs::write(dir.path().join("b.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();

            let config = Config {
                discover: vec![dir.path().join("a.typ"), dir.path().join("b.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert!(result.imports[0].direct);
        }

        #[test]
        fn different_versions_not_deduped() {
            let dir = TempDir::new().unwrap();
            let content = "#import \"@preview/cetz:0.4.1\"\n#import \"@preview/cetz:0.5.0\"\n";
            fs::write(dir.path().join("template.typ"), content).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 2);
        }

        #[test]
        fn local_with_preview_dep() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(
                &pkg_dir,
                "my-pkg",
                "1.0.0",
                Some(r#"#import "@preview/oxifmt:0.2.1""#),
            );

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            let local_import = result
                .imports
                .iter()
                .find(|i| i.namespace == "local")
                .expect("should have @local import");
            assert_eq!(local_import.name, "my-pkg");
            assert!(local_import.direct);

            let transitive = result
                .imports
                .iter()
                .find(|i| i.name == "oxifmt")
                .expect("should have transitive @preview/oxifmt");
            assert_eq!(transitive.namespace, "preview");
            assert_eq!(transitive.version, "0.2.1");
            assert_eq!(transitive.source, "@local/my-pkg");
            assert!(!transitive.direct);
        }

        #[test]
        fn local_with_multiple_preview_deps() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(
                &pkg_dir,
                "my-pkg",
                "1.0.0",
                Some("#import \"@preview/oxifmt:0.2.1\"\n#import \"@preview/codly:1.2.0\"\n"),
            );

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config).unwrap();

            let oxifmt = result.imports.iter().find(|i| i.name == "oxifmt").unwrap();
            assert_eq!(oxifmt.source, "@local/my-pkg");
            assert!(!oxifmt.direct);

            let codly = result.imports.iter().find(|i| i.name == "codly").unwrap();
            assert_eq!(codly.source, "@local/my-pkg");
            assert!(!codly.direct);
        }

        #[test]
        fn local_no_preview_recursion() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(
                &pkg_dir,
                "my-pkg",
                "1.0.0",
                Some(r#"#import "@preview/cetz:0.4.1""#),
            );

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            // No package_cache — cetz's own transitive deps should NOT be resolved
            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config).unwrap();

            // my-pkg and cetz should be present, but nothing deeper
            assert!(result.imports.iter().any(|i| i.name == "my-pkg"));
            assert!(result.imports.iter().any(|i| i.name == "cetz"));
            assert_eq!(result.imports.len(), 2);
        }

        #[test]
        fn local_dep_already_direct() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(
                &pkg_dir,
                "my-pkg",
                "1.0.0",
                Some(r#"#import "@preview/cetz:0.4.1""#),
            );

            // Document also directly imports cetz
            let content = "#import \"@preview/cetz:0.4.1\"\n#import \"@local/my-pkg:1.0.0\"\n";
            fs::write(dir.path().join("template.typ"), content).unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            let cetz = result
                .imports
                .iter()
                .find(|i| i.name == "cetz")
                .expect("should find cetz");
            assert!(cetz.direct, "direct should win over transitive");
            assert_eq!(cetz.source, "template.typ");
        }

        #[test]
        fn local_with_no_deps() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(&pkg_dir, "my-pkg", "1.0.0", Some("// no imports"));

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.imports[0].namespace, "local");
        }

        #[test]
        fn local_missing_typst_toml() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            fs::create_dir_all(&pkg_dir).unwrap();
            fs::write(pkg_dir.join("lib.typ"), "// no manifest").unwrap();

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            // Should still have the direct @local import from discover scanning
            let local_import = result
                .imports
                .iter()
                .find(|i| i.namespace == "local" && i.name == "my-pkg");
            assert!(
                local_import.is_some(),
                "should still report @local import from discover"
            );
            assert!(local_import.unwrap().direct);
        }

        #[test]
        fn local_missing_source_dir() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), "/nonexistent/path".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            // Direct import still present from discover scanning
            let local_import = result
                .imports
                .iter()
                .find(|i| i.namespace == "local" && i.name == "my-pkg");
            assert!(local_import.is_some());
            assert!(local_import.unwrap().direct);
        }

        #[test]
        fn local_imports_another_local() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(
                &pkg_dir,
                "my-pkg",
                "1.0.0",
                Some(r#"#import "@local/other-pkg:2.0.0""#),
            );

            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@local/my-pkg:1.0.0""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            // Should report @local/other-pkg as transitive but not recurse into it
            let other = result
                .imports
                .iter()
                .find(|i| i.name == "other-pkg")
                .expect("should find transitive @local/other-pkg");
            assert_eq!(other.namespace, "local");
            assert!(!other.direct);
            assert_eq!(other.source, "@local/my-pkg");
        }

        #[test]
        fn local_version_from_toml() {
            let dir = TempDir::new().unwrap();
            let pkg_dir = dir.path().join("my-pkg-src");
            create_local_package(&pkg_dir, "my-pkg", "2.0.0", None);

            // Config has [local] but no discover imports
            let config = Config {
                local: [("my-pkg".to_string(), pkg_dir.display().to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            let import = result
                .imports
                .iter()
                .find(|i| i.name == "my-pkg")
                .expect("should find @local/my-pkg");
            assert_eq!(import.version, "2.0.0");
        }

        #[test]
        fn discover_directory() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();
            fs::write(
                dir.path().join("b.typ"),
                r#"#import "@preview/fletcher:0.5.3""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().to_path_buf()],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 2);
            assert_eq!(result.files.len(), 2);
        }

        #[test]
        fn discover_directory_non_recursive() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();
            let sub = dir.path().join("subdir");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("b.typ"), r#"#import "@preview/fletcher:0.5.3""#).unwrap();

            let config = Config {
                discover: vec![dir.path().to_path_buf()],
                ..Default::default()
            };
            let result = analyze(&config);

            // Only a.typ should be found (non-recursive)
            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.imports[0].name, "cetz");
        }

        #[test]
        fn discover_single_file() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("template.typ"),
                r#"#import "@preview/cetz:0.4.1""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
            assert_eq!(result.files, vec!["template.typ"]);
        }

        #[test]
        fn discover_nonexistent_path() {
            let config = Config {
                discover: vec![PathBuf::from("/nonexistent/path.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert!(result.imports.is_empty());
            assert!(result.files.is_empty());
        }

        #[test]
        fn discover_mixed() {
            let dir = TempDir::new().unwrap();
            let sub = dir.path().join("subdir");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("a.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();
            fs::write(
                dir.path().join("single.typ"),
                r#"#import "@preview/fletcher:0.5.3""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![sub, dir.path().join("single.typ")],
                ..Default::default()
            };
            let result = analyze(&config).unwrap();

            assert_eq!(result.imports.len(), 2);
            assert_eq!(result.files.len(), 2);
        }

        #[test]
        fn discover_non_typ_file() {
            let dir = TempDir::new().unwrap();
            fs::write(
                dir.path().join("notes.txt"),
                r#"#import "@preview/cetz:0.4.1""#,
            )
            .unwrap();

            let config = Config {
                discover: vec![dir.path().join("notes.txt")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert!(result.imports.is_empty());
        }

        #[test]
        fn rootdir_resolves_discover() {
            let dir = TempDir::new().unwrap();
            let sub = dir.path().join("subdir");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("template.typ"), r#"#import "@preview/cetz:0.4.1""#).unwrap();

            let config = Config {
                rootdir: Some(sub.clone()),
                discover: vec![PathBuf::from("template.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.imports.len(), 1);
        }

        #[test]
        fn rootdir_resolves_local() {
            let dir = TempDir::new().unwrap();
            let sub = dir.path().join("subdir");
            let pkg_dir = dir.path().join("pkg-src");
            fs::create_dir_all(&sub).unwrap();
            create_local_package(&pkg_dir, "my-pkg", "1.0.0", None);

            let config = Config {
                rootdir: Some(dir.path().to_path_buf()),
                local: [("my-pkg".to_string(), "pkg-src".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let result = analyze(&config);

            let import = result.imports.iter().find(|i| i.name == "my-pkg");
            assert!(import.is_some());
        }

        #[test]
        fn files_list_correct() {
            let dir = TempDir::new().unwrap();
            fs::write(dir.path().join("a.typ"), "= Hello").unwrap();
            fs::write(dir.path().join("b.typ"), "= World").unwrap();

            let config = Config {
                discover: vec![dir.path().join("a.typ"), dir.path().join("b.typ")],
                ..Default::default()
            };
            let result = analyze(&config);

            assert_eq!(result.files.len(), 2);
            assert!(result.files.contains(&"a.typ".to_string()));
            assert!(result.files.contains(&"b.typ".to_string()));
        }

        #[test]
        fn empty_discover() {
            let config = Config::default();
            let result = analyze(&config);

            assert!(result.imports.is_empty());
            assert!(result.files.is_empty());
        }
    }
}
