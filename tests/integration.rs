//! Integration tests for typst-gather.
//!
//! These tests verify the full gathering workflow including:
//! - Local package copying
//! - Dependency scanning from .typ files
//! - Preview package caching (requires network)

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use tempfile::TempDir;
use typst_gather::{find_imports, gather_packages, Config, PackageEntry};

/// Helper to create a minimal local package with typst.toml
fn create_local_package(dir: &Path, name: &str, version: &str, typ_content: Option<&str>) {
    fs::create_dir_all(dir).unwrap();

    let manifest = format!(
        r#"[package]
name = "{name}"
version = "{version}"
entrypoint = "lib.typ"
"#
    );
    fs::write(dir.join("typst.toml"), manifest).unwrap();

    let content = typ_content.unwrap_or("// Empty package\n");
    fs::write(dir.join("lib.typ"), content).unwrap();
}

mod local_packages {
    use super::*;

    #[test]
    fn cache_single_local_package() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        create_local_package(src_dir.path(), "my-pkg", "1.0.0", None);

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 1);
        assert_eq!(result.stats.failed, 0);

        // Verify package was copied to correct location
        let cached = cache_dir.path().join("local/my-pkg/1.0.0");
        assert!(cached.exists());
        assert!(cached.join("typst.toml").exists());
        assert!(cached.join("lib.typ").exists());
    }

    #[test]
    fn cache_local_package_overwrites_existing() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create initial version
        create_local_package(src_dir.path(), "my-pkg", "1.0.0", Some("// v1"));

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        gather_packages(cache_dir.path(), entries.clone(), &[], &configured_local);

        // Update source
        fs::write(src_dir.path().join("lib.typ"), "// v2").unwrap();

        // Cache again
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);
        assert_eq!(result.stats.copied, 1);

        // Verify new content
        let cached_lib = cache_dir.path().join("local/my-pkg/1.0.0/lib.typ");
        let content = fs::read_to_string(cached_lib).unwrap();
        assert_eq!(content, "// v2");
    }

    #[test]
    fn cache_multiple_local_packages() {
        let src1 = TempDir::new().unwrap();
        let src2 = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        create_local_package(src1.path(), "pkg-one", "1.0.0", None);
        create_local_package(src2.path(), "pkg-two", "2.0.0", None);

        let entries = vec![
            PackageEntry::Local {
                name: "pkg-one".to_string(),
                dir: src1.path().to_path_buf(),
            },
            PackageEntry::Local {
                name: "pkg-two".to_string(),
                dir: src2.path().to_path_buf(),
            },
        ];

        let configured_local: HashSet<String> = ["pkg-one".to_string(), "pkg-two".to_string()]
            .into_iter()
            .collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 2);
        assert!(cache_dir.path().join("local/pkg-one/1.0.0").exists());
        assert!(cache_dir.path().join("local/pkg-two/2.0.0").exists());
    }

    #[test]
    fn fail_on_name_mismatch() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create package with different name in manifest
        create_local_package(src_dir.path(), "actual-name", "1.0.0", None);

        let entries = vec![PackageEntry::Local {
            name: "wrong-name".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["wrong-name".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 0);
        assert_eq!(result.stats.failed, 1);
    }

    #[test]
    fn fail_on_missing_manifest() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create directory without typst.toml
        fs::create_dir_all(src_dir.path()).unwrap();
        fs::write(src_dir.path().join("lib.typ"), "// no manifest").unwrap();

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 0);
        assert_eq!(result.stats.failed, 1);
    }

    #[test]
    fn fail_on_nonexistent_directory() {
        let cache_dir = TempDir::new().unwrap();

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: "/nonexistent/path/to/package".into(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 0);
        assert_eq!(result.stats.failed, 1);
    }

    #[test]
    fn preserves_subdirectories() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        create_local_package(src_dir.path(), "my-pkg", "1.0.0", None);

        // Add subdirectory with files
        let sub = src_dir.path().join("src/utils");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("helper.typ"), "// helper").unwrap();

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 1);

        let cached_helper = cache_dir
            .path()
            .join("local/my-pkg/1.0.0/src/utils/helper.typ");
        assert!(cached_helper.exists());
    }
}

mod dependency_scanning {
    use super::*;

    #[test]
    fn find_imports_in_single_file() {
        let dir = TempDir::new().unwrap();

        let content = r#"
#import "@preview/cetz:0.4.1": canvas
#import "@preview/fletcher:0.5.3"

= Document
"#;
        fs::write(dir.path().join("main.typ"), content).unwrap();

        let imports = find_imports(dir.path());

        assert_eq!(imports.len(), 2);
        let names: Vec<_> = imports.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cetz"));
        assert!(names.contains(&"fletcher"));
    }

    #[test]
    fn find_imports_in_nested_files() {
        let dir = TempDir::new().unwrap();

        fs::write(
            dir.path().join("main.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let sub = dir.path().join("chapters");
        fs::create_dir_all(&sub).unwrap();
        fs::write(
            sub.join("intro.typ"),
            r#"#import "@preview/fletcher:0.5.3""#,
        )
        .unwrap();

        let imports = find_imports(dir.path());

        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn ignore_non_typ_files() {
        let dir = TempDir::new().unwrap();

        fs::write(
            dir.path().join("main.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();
        fs::write(
            dir.path().join("notes.txt"),
            r#"#import "@preview/ignored:1.0.0""#,
        )
        .unwrap();

        let imports = find_imports(dir.path());

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "cetz");
    }

    #[test]
    fn find_includes() {
        let dir = TempDir::new().unwrap();

        let content = r#"#include "@preview/template:1.0.0""#;
        fs::write(dir.path().join("main.typ"), content).unwrap();

        let imports = find_imports(dir.path());

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "template");
    }

    #[test]
    fn ignore_relative_imports() {
        let dir = TempDir::new().unwrap();

        let content = r#"
#import "@preview/cetz:0.4.1"
#import "utils.typ"
#import "../shared/common.typ"
"#;
        fs::write(dir.path().join("main.typ"), content).unwrap();

        let imports = find_imports(dir.path());

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "cetz");
    }

    #[test]
    fn empty_directory() {
        let dir = TempDir::new().unwrap();
        let imports = find_imports(dir.path());
        assert!(imports.is_empty());
    }
}

mod config_integration {
    use super::*;

    #[test]
    fn parse_and_cache_local_from_toml() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        create_local_package(src_dir.path(), "my-pkg", "1.0.0", None);

        let toml = format!(
            r#"
destination = "{}"

[local]
my-pkg = "{}"
"#,
            cache_dir.path().display().to_string().replace('\\', "/"),
            src_dir.path().display().to_string().replace('\\', "/")
        );

        let config = Config::parse(&toml).unwrap();
        let dest = config.destination.clone().unwrap();
        let configured_local: HashSet<String> = config.local.keys().cloned().collect();
        let entries = config.into_entries();
        let result = gather_packages(&dest, entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 1);
        assert!(cache_dir.path().join("local/my-pkg/1.0.0").exists());
    }

    #[test]
    fn empty_config_does_nothing() {
        let cache_dir = TempDir::new().unwrap();

        let toml = format!(
            r#"destination = "{}""#,
            cache_dir.path().display().to_string().replace('\\', "/")
        );
        let config = Config::parse(&toml).unwrap();
        let dest = config.destination.clone().unwrap();
        let configured_local: HashSet<String> = config.local.keys().cloned().collect();
        let entries = config.into_entries();
        let result = gather_packages(&dest, entries, &[], &configured_local);

        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.copied, 0);
        assert_eq!(result.stats.skipped, 0);
        assert_eq!(result.stats.failed, 0);
    }

    #[test]
    fn missing_destination_returns_none() {
        let config = Config::parse("").unwrap();
        assert!(config.destination.is_none());
    }

    #[test]
    fn parse_discover_field() {
        let toml = r#"
destination = "/cache"
discover = "/path/to/templates"
"#;
        let config = Config::parse(toml).unwrap();
        assert_eq!(
            config.discover,
            vec![std::path::PathBuf::from("/path/to/templates")]
        );
    }

    #[test]
    fn parse_discover_array() {
        let toml = r#"
destination = "/cache"
discover = ["template.typ", "typst-show.typ"]
"#;
        let config = Config::parse(toml).unwrap();
        assert_eq!(
            config.discover,
            vec![
                std::path::PathBuf::from("template.typ"),
                std::path::PathBuf::from("typst-show.typ"),
            ]
        );
    }
}

mod unconfigured_local {
    use super::*;

    #[test]
    fn detects_unconfigured_local_imports() {
        let cache_dir = TempDir::new().unwrap();
        let discover_dir = TempDir::new().unwrap();

        // Create a .typ file that imports @local/my-pkg
        let content = r#"#import "@local/my-pkg:1.0.0""#;
        fs::write(discover_dir.path().join("template.typ"), content).unwrap();

        // Don't configure my-pkg in the local section
        let configured_local: HashSet<String> = HashSet::new();
        let discover = vec![discover_dir.path().to_path_buf()];

        let result = gather_packages(cache_dir.path(), vec![], &discover, &configured_local);

        // Should have one unconfigured local
        assert_eq!(result.unconfigured_local.len(), 1);
        assert_eq!(result.unconfigured_local[0].0, "my-pkg");
    }

    #[test]
    fn configured_local_not_reported() {
        let cache_dir = TempDir::new().unwrap();
        let discover_dir = TempDir::new().unwrap();

        // Create a .typ file that imports @local/my-pkg
        let content = r#"#import "@local/my-pkg:1.0.0""#;
        fs::write(discover_dir.path().join("template.typ"), content).unwrap();

        // Configure my-pkg (even though we don't actually copy it)
        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let discover = vec![discover_dir.path().to_path_buf()];

        let result = gather_packages(cache_dir.path(), vec![], &discover, &configured_local);

        // Should have no unconfigured local
        assert!(result.unconfigured_local.is_empty());
    }
}

mod analyze_integration {
    use super::*;
    use serde_json::Value;
    use std::process::Command;

    fn cargo_bin() -> std::path::PathBuf {
        // Build first, then find the binary
        let output = Command::new("cargo")
            .args(["build"])
            .output()
            .expect("failed to build");
        assert!(output.status.success(), "cargo build failed");

        let output = Command::new("cargo")
            .args(["metadata", "--format-version=1", "--no-deps"])
            .output()
            .expect("failed to get metadata");
        let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
        let target_dir = metadata["target_directory"].as_str().unwrap();
        std::path::PathBuf::from(target_dir).join("debug/typst-gather")
    }

    #[test]
    fn analyze_from_file() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("template.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success(), "exit code was not 0");
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON on stdout");
        assert!(json["imports"].is_array());
        assert!(json["files"].is_array());
        assert_eq!(json["imports"].as_array().unwrap().len(), 1);
        assert_eq!(json["imports"][0]["name"], "cetz");
    }

    #[test]
    fn analyze_from_stdin() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("template.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );

        let mut child = Command::new(cargo_bin())
            .args(["analyze", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn");

        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config_content.as_bytes())
            .unwrap();

        let output = child.wait_with_output().expect("failed to wait");
        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        assert_eq!(json["imports"][0]["name"], "cetz");
    }

    #[test]
    fn gather_still_works() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();

        let config_content = format!(
            "destination = \"{}\"\n",
            dest.display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["gather", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
    }

    #[test]
    fn bare_file_backwards_compat() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("dest");
        fs::create_dir_all(&dest).unwrap();

        let config_content = format!(
            "destination = \"{}\"\n",
            dest.display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        // No subcommand, just the file path
        let output = Command::new(cargo_bin())
            .args([config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
    }

    #[test]
    fn analyze_no_destination_ok() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("template.typ"), "= Hello").unwrap();

        // Config with no destination field
        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
    }

    #[test]
    fn gather_no_destination_fails() {
        let dir = TempDir::new().unwrap();
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, "").unwrap();

        let output = Command::new(cargo_bin())
            .args(["gather", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(!output.status.success());
    }

    #[test]
    fn json_output_no_extra_on_stdout() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("template.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        let stdout = String::from_utf8(output.stdout).unwrap();
        // Should parse as valid JSON with no extra content
        let parsed: Result<Value, _> = serde_json::from_str(stdout.trim());
        assert!(parsed.is_ok(), "stdout should be valid JSON only");
    }

    #[test]
    fn e2e_extension_like_config() {
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
            "#import \"@preview/cetz:0.4.1\"\n#import \"@local/my-pkg:1.0.0\"\n",
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{dir}/template.typ\"]\n\n[local]\nmy-pkg = \"{pkg}\"\n",
            dir = dir.path().display().to_string().replace('\\', "/"),
            pkg = pkg_dir.display().to_string().replace('\\', "/"),
        );

        let mut child = Command::new(cargo_bin())
            .args(["analyze", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn");

        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config_content.as_bytes())
            .unwrap();

        let output = child.wait_with_output().expect("failed to wait");
        assert!(output.status.success());

        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();

        // Direct @preview/cetz
        let cetz = imports.iter().find(|i| i["name"] == "cetz").unwrap();
        assert_eq!(cetz["namespace"], "preview");
        assert!(cetz["direct"].as_bool().unwrap());

        // Direct @local/my-pkg
        let my_pkg = imports.iter().find(|i| i["name"] == "my-pkg").unwrap();
        assert_eq!(my_pkg["namespace"], "local");
        assert!(my_pkg["direct"].as_bool().unwrap());

        // Transitive @preview/oxifmt
        let oxifmt = imports.iter().find(|i| i["name"] == "oxifmt").unwrap();
        assert_eq!(oxifmt["namespace"], "preview");
        assert!(!oxifmt["direct"].as_bool().unwrap());
        assert_eq!(oxifmt["source"], "@local/my-pkg");

        // Files list
        let files = json["files"].as_array().unwrap();
        assert!(files.iter().any(|f| f == "template.typ"));
    }

    #[test]
    fn e2e_empty_project() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("template.typ"), "= Hello World").unwrap();

        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        assert!(json["imports"].as_array().unwrap().is_empty());
        assert!(!json["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn json_import_fields() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("template.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{}/template.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let import = &json["imports"][0];
        assert!(import["namespace"].is_string());
        assert!(import["name"].is_string());
        assert!(import["version"].is_string());
        assert!(import["source"].is_string());
        assert!(import["direct"].is_boolean());
    }

    #[test]
    fn e2e_local_only() {
        let dir = TempDir::new().unwrap();
        let pkg_dir = dir.path().join("my-pkg-src");
        create_local_package(
            &pkg_dir,
            "my-pkg",
            "1.0.0",
            Some(r#"#import "@preview/oxifmt:0.2.1""#),
        );

        // No discover paths, only [local]
        let config_content = format!(
            "[local]\nmy-pkg = \"{}\"\n",
            pkg_dir.display().to_string().replace('\\', "/"),
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();

        assert!(imports.iter().any(|i| i["name"] == "my-pkg"));
        assert!(imports.iter().any(|i| i["name"] == "oxifmt"));
        assert!(json["files"].as_array().unwrap().is_empty());
    }

    #[test]
    fn e2e_discover_only() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("doc.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        // No [local] section
        let config_content = format!(
            "discover = [\"{}/doc.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 1);
        assert!(imports[0]["direct"].as_bool().unwrap());
    }

    #[test]
    fn e2e_staging_scenario() {
        let dir = TempDir::new().unwrap();

        // Create cache
        let cache = dir.path().join("cache");
        let cetz_dir = cache.join("preview/cetz/0.4.1");
        fs::create_dir_all(&cetz_dir).unwrap();
        fs::write(
            cetz_dir.join("lib.typ"),
            r#"#import "@preview/oxifmt:0.2.1""#,
        )
        .unwrap();
        let oxifmt_dir = cache.join("preview/oxifmt/0.2.1");
        fs::create_dir_all(&oxifmt_dir).unwrap();
        fs::write(oxifmt_dir.join("lib.typ"), "// no imports").unwrap();

        // Create document
        let doc_dir = dir.path().join("doc");
        fs::create_dir_all(&doc_dir).unwrap();
        fs::write(
            doc_dir.join("document.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{doc}/document.typ\"]\npackage-cache = [\"{cache}\"]\n",
            doc = doc_dir.display().to_string().replace('\\', "/"),
            cache = cache.display().to_string().replace('\\', "/"),
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();

        let cetz = imports.iter().find(|i| i["name"] == "cetz").unwrap();
        assert!(cetz["direct"].as_bool().unwrap());

        let oxifmt = imports.iter().find(|i| i["name"] == "oxifmt").unwrap();
        assert!(!oxifmt["direct"].as_bool().unwrap());
        assert_eq!(oxifmt["source"], "@preview/cetz:0.4.1");
    }

    #[test]
    fn e2e_staging_with_local() {
        let dir = TempDir::new().unwrap();

        // Create local package that imports fontawesome
        let pkg_dir = dir.path().join("my-theme-src");
        create_local_package(
            &pkg_dir,
            "my-theme",
            "1.0.0",
            Some(r#"#import "@preview/fontawesome:0.5.0""#),
        );

        // Create cache with cetz (depends on oxifmt) and fontawesome
        let cache = dir.path().join("cache");
        let cetz_dir = cache.join("preview/cetz/0.4.1");
        fs::create_dir_all(&cetz_dir).unwrap();
        fs::write(
            cetz_dir.join("lib.typ"),
            r#"#import "@preview/oxifmt:0.2.1""#,
        )
        .unwrap();
        let oxifmt_dir = cache.join("preview/oxifmt/0.2.1");
        fs::create_dir_all(&oxifmt_dir).unwrap();
        fs::write(oxifmt_dir.join("lib.typ"), "// leaf").unwrap();
        let fa_dir = cache.join("preview/fontawesome/0.5.0");
        fs::create_dir_all(&fa_dir).unwrap();
        fs::write(fa_dir.join("lib.typ"), "// leaf").unwrap();

        // Document imports cetz and local theme
        fs::write(
            dir.path().join("doc.typ"),
            "#import \"@preview/cetz:0.4.1\"\n#import \"@local/my-theme:1.0.0\"\n",
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{dir}/doc.typ\"]\npackage-cache = [\"{cache}\"]\n\n[local]\nmy-theme = \"{pkg}\"\n",
            dir = dir.path().display().to_string().replace('\\', "/"),
            cache = cache.display().to_string().replace('\\', "/"),
            pkg = pkg_dir.display().to_string().replace('\\', "/"),
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();

        // cetz (direct), my-theme (direct, local), fontawesome (transitive from local),
        // oxifmt (transitive from cetz via cache)
        assert!(imports.iter().any(|i| i["name"] == "cetz"));
        assert!(imports.iter().any(|i| i["name"] == "my-theme"));
        assert!(imports.iter().any(|i| i["name"] == "fontawesome"));
        assert!(imports.iter().any(|i| i["name"] == "oxifmt"));
    }

    #[test]
    fn e2e_no_package_cache() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("doc.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{}/doc.typ\"]\n",
            dir.path().display().to_string().replace('\\', "/")
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
    }

    #[test]
    fn e2e_multiple_caches() {
        let dir = TempDir::new().unwrap();
        let cache1 = dir.path().join("cache1");
        let cache2 = dir.path().join("cache2");

        let a_dir = cache1.join("preview/pkg-a/1.0.0");
        fs::create_dir_all(&a_dir).unwrap();
        fs::write(a_dir.join("lib.typ"), "// leaf").unwrap();

        let b_dir = cache2.join("preview/pkg-b/1.0.0");
        fs::create_dir_all(&b_dir).unwrap();
        fs::write(b_dir.join("lib.typ"), "// leaf").unwrap();

        fs::write(
            dir.path().join("doc.typ"),
            "#import \"@preview/pkg-a:1.0.0\"\n#import \"@preview/pkg-b:1.0.0\"\n",
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{dir}/doc.typ\"]\npackage-cache = [\"{c1}\", \"{c2}\"]\n",
            dir = dir.path().display().to_string().replace('\\', "/"),
            c1 = cache1.display().to_string().replace('\\', "/"),
            c2 = cache2.display().to_string().replace('\\', "/"),
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        let imports = json["imports"].as_array().unwrap();
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn e2e_cache_via_stdin() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        let cetz_dir = cache.join("preview/cetz/0.4.1");
        fs::create_dir_all(&cetz_dir).unwrap();
        fs::write(
            cetz_dir.join("lib.typ"),
            r#"#import "@preview/oxifmt:0.2.1""#,
        )
        .unwrap();
        let oxifmt_dir = cache.join("preview/oxifmt/0.2.1");
        fs::create_dir_all(&oxifmt_dir).unwrap();
        fs::write(oxifmt_dir.join("lib.typ"), "// leaf").unwrap();

        fs::write(
            dir.path().join("doc.typ"),
            r#"#import "@preview/cetz:0.4.1""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{dir}/doc.typ\"]\npackage-cache = [\"{cache}\"]\n",
            dir = dir.path().display().to_string().replace('\\', "/"),
            cache = cache.display().to_string().replace('\\', "/"),
        );

        let mut child = Command::new(cargo_bin())
            .args(["analyze", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn");

        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config_content.as_bytes())
            .unwrap();

        let output = child.wait_with_output().expect("failed to wait");
        assert!(output.status.success());
        let json: Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        assert!(json["imports"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["name"] == "oxifmt"));
    }

    #[test]
    fn e2e_preview_imports_local_error() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("cache");
        let foo_dir = cache.join("preview/foo/1.0.0");
        fs::create_dir_all(&foo_dir).unwrap();
        fs::write(foo_dir.join("lib.typ"), r#"#import "@local/bar:1.0.0""#).unwrap();

        fs::write(
            dir.path().join("doc.typ"),
            r#"#import "@preview/foo:1.0.0""#,
        )
        .unwrap();

        let config_content = format!(
            "discover = [\"{dir}/doc.typ\"]\npackage-cache = [\"{cache}\"]\n",
            dir = dir.path().display().to_string().replace('\\', "/"),
            cache = cache.display().to_string().replace('\\', "/"),
        );
        let config_file = dir.path().join("config.toml");
        fs::write(&config_file, &config_content).unwrap();

        let output = Command::new(cargo_bin())
            .args(["analyze", config_file.to_str().unwrap()])
            .output()
            .expect("failed to run");

        assert!(!output.status.success(), "should fail with non-zero exit");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("@preview/foo:1.0.0"));
        assert!(stderr.contains("@local/bar:1.0.0"));
        // Stdout should be empty (no partial JSON)
        assert!(output.stdout.is_empty());
    }
}

/// Tests that require network access.
/// Run with: cargo test -- --ignored
mod network {
    use super::*;

    #[test]
    #[ignore = "requires network access"]
    fn download_preview_package() {
        let cache_dir = TempDir::new().unwrap();

        let entries = vec![PackageEntry::Preview {
            name: "example".to_string(),
            version: "0.1.0".to_string(),
        }];

        let configured_local = HashSet::new();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.downloaded, 1);
        assert_eq!(result.stats.failed, 0);

        let cached = cache_dir.path().join("preview/example/0.1.0");
        assert!(cached.exists());
        assert!(cached.join("typst.toml").exists());
    }

    #[test]
    #[ignore = "requires network access"]
    fn download_package_with_dependencies() {
        let cache_dir = TempDir::new().unwrap();

        // cetz has dependencies that should be auto-downloaded
        let entries = vec![PackageEntry::Preview {
            name: "cetz".to_string(),
            version: "0.3.4".to_string(),
        }];

        let configured_local = HashSet::new();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        // Should download cetz plus its dependencies
        assert!(result.stats.downloaded >= 1);
        assert_eq!(result.stats.failed, 0);
    }

    #[test]
    #[ignore = "requires network access"]
    fn skip_already_cached() {
        let cache_dir = TempDir::new().unwrap();

        let entries = vec![PackageEntry::Preview {
            name: "example".to_string(),
            version: "0.1.0".to_string(),
        }];

        let configured_local = HashSet::new();

        // First download
        let result1 = gather_packages(cache_dir.path(), entries.clone(), &[], &configured_local);
        assert_eq!(result1.stats.downloaded, 1);

        // Second run should skip
        let result2 = gather_packages(cache_dir.path(), entries, &[], &configured_local);
        assert_eq!(result2.stats.downloaded, 0);
        assert_eq!(result2.stats.skipped, 1);
    }

    #[test]
    #[ignore = "requires network access"]
    fn fail_on_nonexistent_package() {
        let cache_dir = TempDir::new().unwrap();

        let entries = vec![PackageEntry::Preview {
            name: "this-package-does-not-exist-12345".to_string(),
            version: "0.0.0".to_string(),
        }];

        let configured_local = HashSet::new();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.failed, 1);
    }

    #[test]
    #[ignore = "requires network access"]
    fn local_package_triggers_preview_deps() {
        let src_dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();

        // Create local package that imports a preview package
        let content = r#"
#import "@preview/example:0.1.0"

#let my-func() = []
"#;
        create_local_package(src_dir.path(), "my-pkg", "1.0.0", Some(content));

        let entries = vec![PackageEntry::Local {
            name: "my-pkg".to_string(),
            dir: src_dir.path().to_path_buf(),
        }];

        let configured_local: HashSet<String> = ["my-pkg".to_string()].into_iter().collect();
        let result = gather_packages(cache_dir.path(), entries, &[], &configured_local);

        assert_eq!(result.stats.copied, 1);
        assert!(result.stats.downloaded >= 1); // Should have downloaded example

        assert!(cache_dir.path().join("local/my-pkg/1.0.0").exists());
        assert!(cache_dir.path().join("preview/example/0.1.0").exists());
    }
}
