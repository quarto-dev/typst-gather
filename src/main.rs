use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use typst_gather::{analyze, gather_packages, Config};

#[derive(Parser)]
#[command(version, about = "Gather Typst packages to a local directory")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Config file path (for backwards compatibility without subcommand)
    config: Option<String>,
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

/// Read config content from a file path or stdin (when path is "-").
fn read_config(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut content = String::new();
        std::io::stdin()
            .read_to_string(&mut content)
            .map_err(|e| format!("Error reading stdin: {e}"))?;
        Ok(content)
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("Error reading config file: {e}"))
    }
}

fn run_gather(config_path: &str) -> ExitCode {
    let content = match read_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let config = match Config::parse(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error parsing config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dest = match &config.destination {
        Some(d) => d.clone(),
        None => {
            eprintln!("Error: 'destination' field is required for gather");
            return ExitCode::FAILURE;
        }
    };

    // Resolve paths relative to rootdir if specified
    let rootdir = config.rootdir.clone();
    let dest = match &rootdir {
        Some(root) => root.join(&dest),
        None => dest,
    };
    let discover: Vec<PathBuf> = config
        .discover
        .iter()
        .map(|p| match &rootdir {
            Some(root) => root.join(p),
            None => p.clone(),
        })
        .collect();

    // Build set of configured local packages
    let configured_local: HashSet<String> = config.local.keys().cloned().collect();

    let entries = config.into_entries();
    let result = match gather_packages(&dest, entries, &discover, &configured_local) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Check for unconfigured @local imports FIRST (this is an error)
    if !result.unconfigured_local.is_empty() {
        eprintln!("\nError: Found @local imports not configured in [local] section:");
        for (name, source_file) in &result.unconfigured_local {
            eprintln!("  - {name} (in {source_file})");
        }
        eprintln!("\nAdd them to your config file:");
        eprintln!("  [local]");
        for (name, _) in &result.unconfigured_local {
            eprintln!("  {name} = \"/path/to/{name}\"");
        }
        return ExitCode::FAILURE;
    }

    eprintln!(
        "\nDone: {} downloaded, {} copied, {} skipped, {} failed",
        result.stats.downloaded, result.stats.copied, result.stats.skipped, result.stats.failed
    );

    if result.stats.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_analyze(config_path: &str) -> ExitCode {
    let content = match read_config(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let config = match Config::parse(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error parsing config: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match analyze(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match serde_json::to_string_pretty(&result) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Error serializing JSON: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Gather { config }) => run_gather(&config),
        Some(Command::Analyze { config }) => run_analyze(&config),
        None => {
            // Backwards compatibility: bare positional arg = gather
            match cli.config {
                Some(config) => run_gather(&config),
                None => {
                    eprintln!("Error: no config file specified");
                    eprintln!("Usage: typst-gather [gather|analyze] <config>");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
