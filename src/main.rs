use clap::{Parser, Subcommand};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs,
    io::{Cursor, Read, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};
use tar::Archive;

const USER_AGENT: &str = concat!("nab/", env!("CARGO_PKG_VERSION"));

// CLI

#[derive(Parser)]
#[command(name = "nab", version, about = "Binary-only fetch & install")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a tool (e.g., drbh/tool, drbh/tool@v1.0.0, or configured-name@latest)
    Install {
        /// Tool spec: owner/repo[@version] or configured-name[@version]
        spec: String,
    },
    /// Upgrade tool(s) to latest version
    Upgrade {
        /// Tool name (omit for --all)
        tool: Option<String>,
        /// Upgrade all configured tools
        #[arg(long)]
        all: bool,
    },
    /// List configured tools
    List,
    /// Show the installation path of a tool
    Where {
        /// Tool name
        tool: String,
    },
    /// Remove a tool (config and binary)
    Remove {
        /// Tool name
        tool: String,
        /// Only remove from config, keep the binary
        #[arg(long)]
        keep_binary: bool,
    },
    /// Configure a tool manually (for non-standard release assets)
    Config {
        /// Tool name (how you'll refer to it)
        name: String,
        /// Source (e.g., github:owner/repo)
        source: String,
        /// Asset URL pattern for macOS ARM64 (use {version} placeholder)
        #[arg(long)]
        darwin_arm64: Option<String>,
        /// Asset URL pattern for Linux x86_64 (use {version} placeholder)
        #[arg(long)]
        linux_amd64: Option<String>,
        /// Asset URL pattern for Linux RISCV64 (use {version} placeholder)
        #[arg(long)]
        linux_riscv64: Option<String>,
        /// Binary name in archive (if different from tool name)
        #[arg(long)]
        bin_name: Option<String>,
    },
}

// Platform Detection

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Platform {
    DarwinArm64,
    LinuxAmd64,
    LinuxRiscv64,
}

impl Platform {
    fn detect() -> Result<Self, String> {
        match (env::consts::OS, env::consts::ARCH) {
            ("macos", "aarch64") => Ok(Platform::DarwinArm64),
            ("linux", "x86_64") => Ok(Platform::LinuxAmd64),
            ("linux", "riscv64") => Ok(Platform::LinuxRiscv64),
            (os, arch) => Err(format!("Unsupported platform: {os}/{arch}")),
        }
    }

    fn key(&self) -> &'static str {
        match self {
            Platform::DarwinArm64 => "darwin_arm64",
            Platform::LinuxAmd64 => "linux_amd64",
            Platform::LinuxRiscv64 => "linux_riscv64",
        }
    }

    /// Patterns to match against release asset names (case-insensitive)
    fn asset_patterns(&self) -> &[&[&str]] {
        match self {
            Platform::DarwinArm64 => &[
                &["darwin", "arm64"],
                &["darwin", "aarch64"],
                &["macos", "arm64"],
                &["macos", "aarch64"],
                &["apple", "aarch64"],
                &["apple", "arm64"],
            ],
            Platform::LinuxAmd64 => &[
                &["linux", "amd64"],
                &["linux", "x86_64"],
                &["linux", "x64"],
            ],
            Platform::LinuxRiscv64 => &[
                &["linux", "riscv64"],
                &["linux", "riscv"],
            ],
        }
    }

    /// Check if an asset name matches this platform
    fn matches_asset(&self, name: &str) -> bool {
        let lower = name.to_lowercase();
        self.asset_patterns()
            .iter()
            .any(|parts| parts.iter().all(|p| lower.contains(p)))
    }
}

// Configuration

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    #[serde(default = "default_install_dir")]
    install_dir: String,
    #[serde(default = "default_owners")]
    owners: Vec<String>,
    #[serde(default)]
    tools: HashMap<String, ToolConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            install_dir: default_install_dir(),
            owners: default_owners(),
            tools: HashMap::new(),
        }
    }
}

fn default_install_dir() -> String {
    "~/.local/bin".to_string()
}

fn default_owners() -> Vec<String> {
    vec!["drbh".to_string()]
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ToolConfig {
    version: String,
    source: String,
    #[serde(default)]
    assets: HashMap<String, String>,
    #[serde(default)]
    bin_name: Option<String>,
}

impl Config {
    fn load() -> Result<Self, String> {
        let config_path = config_path();
        if !config_path.exists() {
            return Ok(Config::default());
        }
        let content =
            fs::read_to_string(&config_path).map_err(|e| format!("Failed to read config: {e}"))?;
        toml::from_str(&content).map_err(|e| format!("Failed to parse config: {e}"))
    }

    fn save(&self) -> Result<(), String> {
        let config_path = config_path();
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| format!("Failed to serialize config: {e}"))?;
        fs::write(&config_path, content).map_err(|e| format!("Failed to write config: {e}"))?;
        Ok(())
    }

    fn install_dir(&self) -> PathBuf {
        expand_tilde(&self.install_dir)
    }
}

fn config_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/nab/tools.toml")
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(path)
    }
}

// GitHub API

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

fn fetch_release(owner: &str, repo: &str, version: Option<&str>) -> Result<GitHubRelease, String> {
    let url = match version {
        Some(v) => format!("https://api.github.com/repos/{owner}/{repo}/releases/tags/{v}"),
        None => format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
    };

    let response = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| {
            if e.to_string().contains("404") {
                match version {
                    Some(v) => format!("Release '{v}' not found for {owner}/{repo}"),
                    None => format!(
                        "No releases found for {owner}/{repo}.\n\
                         Check: https://github.com/{owner}/{repo}/releases"
                    ),
                }
            } else {
                format!("Failed to fetch release: {e}")
            }
        })?;

    response
        .into_json()
        .map_err(|e| format!("Failed to parse release JSON: {e}"))
}

/// Find the best matching asset for the current platform
fn find_platform_asset(assets: &[GitHubAsset], platform: Platform) -> Option<&GitHubAsset> {
    // Filter out non-binary assets
    let binary_assets: Vec<_> = assets
        .iter()
        .filter(|a| {
            let lower = a.name.to_lowercase();
            !lower.ends_with(".sha256")
                && !lower.ends_with(".sha512")
                && !lower.ends_with(".sig")
                && !lower.ends_with(".asc")
                && !lower.ends_with(".sbom")
                && !lower.ends_with(".json")
        })
        .collect();

    // Try platform-specific matching first
    let mut matches: Vec<_> = binary_assets
        .iter()
        .filter(|a| platform.matches_asset(&a.name))
        .filter(|a| {
            let lower = a.name.to_lowercase();
            lower.ends_with(".tar.gz")
                || lower.ends_with(".tgz")
                || lower.ends_with(".zip")
                || !lower.contains('.') // raw binary (no extension)
        })
        .copied()
        .collect();

    // If no platform-specific match but only one binary asset, use it
    if matches.is_empty() && binary_assets.len() == 1 {
        return Some(binary_assets[0]);
    }

    // Prefer .tar.gz over .zip over raw
    matches.sort_by(|a, b| {
        let score = |name: &str| {
            if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
                2
            } else if name.ends_with(".zip") {
                1
            } else {
                0
            }
        };
        score(&b.name).cmp(&score(&a.name))
    });

    matches.first().copied()
}

// Download & Extract

fn download(url: &str) -> Result<Vec<u8>, String> {
    eprintln!("Downloading {url}...");
    let response = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;

    let mut data = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut data)
        .map_err(|e| format!("Failed to read response: {e}"))?;

    eprintln!("Downloaded {} bytes", data.len());
    Ok(data)
}

fn extract_binary(data: &[u8], url: &str, bin_name: Option<&str>) -> Result<Vec<u8>, String> {
    let lower = url.to_lowercase();

    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        extract_from_targz(data, bin_name)
    } else if lower.ends_with(".zip") {
        extract_from_zip(data, bin_name)
    } else {
        Ok(data.to_vec())
    }
}

/// Check if a file entry looks like a binary (executable, not metadata)
fn is_candidate_binary(file_name: &str, path_str: &str) -> bool {
    if file_name.is_empty() || file_name.starts_with('.') || file_name.starts_with('_') {
        return false;
    }
    if path_str.contains("__MACOSX") || path_str.contains("autocomplete") || path_str.contains("completions") {
        return false;
    }
    // Skip common non-binary files
    let dominated = [
        "LICENSE", "LICENCE", "COPYING", "README", "CHANGELOG", "NOTICE", "AUTHORS",
        ".md", ".txt", ".1", ".fish", ".zsh", ".bash", ".ps1", ".elv", ".nu", ".csh",
    ];
    !dominated.iter().any(|s| file_name.contains(s))
}

fn extract_from_targz(data: &[u8], bin_name: Option<&str>) -> Result<Vec<u8>, String> {
    let decoder = GzDecoder::new(Cursor::new(data));
    let mut archive = Archive::new(decoder);

    let entries = archive
        .entries()
        .map_err(|e| format!("Failed to read tar: {e}"))?;

    let mut candidates = Vec::new();

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Failed to read tar entry: {e}"))?;

        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry
            .path()
            .map_err(|e| format!("Invalid path: {e}"))?
            .to_path_buf();

        let path_str = path.to_string_lossy();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let matches = match bin_name {
            Some(name) => file_name == name,
            None => is_candidate_binary(file_name, &path_str),
        };

        if matches {
            let mut content = Vec::new();
            entry
                .read_to_end(&mut content)
                .map_err(|e| format!("Failed to extract: {e}"))?;

            if bin_name.is_some() {
                eprintln!("Extracted: {}", path.display());
                return Ok(content);
            }
            candidates.push((path.to_string_lossy().to_string(), content));
        }
    }

    // If no bin_name specified, we need exactly one candidate
    match candidates.len() {
        0 => Err(format!(
            "No binary found in archive{}",
            bin_name.map(|n| format!(" (looking for '{n}')")).unwrap_or_default()
        )),
        1 => {
            eprintln!("Extracted: {}", candidates[0].0);
            Ok(candidates.remove(0).1)
        }
        _ => Err(format!(
            "Multiple binaries found in archive: {}. Use --bin-name to specify.",
            candidates.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn extract_from_zip(data: &[u8], bin_name: Option<&str>) -> Result<Vec<u8>, String> {
    let cursor = Cursor::new(data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| format!("Failed to read zip: {e}"))?;

    let mut candidates = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry: {e}"))?;

        if file.is_dir() {
            continue;
        }

        let path_str = file.name().to_string();
        let file_name = PathBuf::from(&path_str)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let matches = match bin_name {
            Some(name) => file_name == name,
            None => is_candidate_binary(&file_name, &path_str),
        };

        if matches {
            let mut content = Vec::new();
            file.read_to_end(&mut content)
                .map_err(|e| format!("Failed to extract: {e}"))?;

            if bin_name.is_some() {
                eprintln!("Extracted: {path_str}");
                return Ok(content);
            }
            candidates.push((path_str, content));
        }
    }

    match candidates.len() {
        0 => Err(format!(
            "No binary found in archive{}",
            bin_name.map(|n| format!(" (looking for '{n}')")).unwrap_or_default()
        )),
        1 => {
            eprintln!("Extracted: {}", candidates[0].0);
            Ok(candidates.remove(0).1)
        }
        _ => Err(format!(
            "Multiple binaries found in archive: {}. Use --bin-name to specify.",
            candidates.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

// Installation

fn atomic_install(binary: &[u8], dest: &PathBuf) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create install dir: {e}"))?;
    }

    let temp_path = dest.with_extension("tmp");
    {
        let mut file =
            fs::File::create(&temp_path).map_err(|e| format!("Failed to create temp file: {e}"))?;
        file.write_all(binary)
            .map_err(|e| format!("Failed to write binary: {e}"))?;
    }

    fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Failed to set permissions: {e}"))?;

    fs::rename(&temp_path, dest).map_err(|e| format!("Failed to install: {e}"))?;

    Ok(())
}

// Parse spec into components

struct InstallSpec {
    owner: Option<String>,
    repo: String,
    version: Option<String>,
}

fn parse_spec(spec: &str) -> InstallSpec {
    // Format: [owner/]repo[@version]
    let (main, version) = match spec.rsplit_once('@') {
        Some((m, v)) => (m, Some(v.to_string())),
        None => (spec, None),
    };

    match main.split_once('/') {
        Some((owner, repo)) => InstallSpec {
            owner: Some(owner.to_string()),
            repo: repo.to_string(),
            version,
        },
        None => InstallSpec {
            owner: None,
            repo: main.to_string(),
            version,
        },
    }
}

// Commands

fn cmd_install(config: &mut Config, spec: &str) -> Result<(), String> {
    let platform = Platform::detect()?;
    let parsed = parse_spec(spec);

    // Try convention-based GitHub install if owner/repo format
    if let Some(owner) = &parsed.owner {
        return install_from_github(config, owner, &parsed.repo, parsed.version.as_deref(), platform);
    }

    // Check if already in config
    if let Some(tool_config) = config.tools.get(&parsed.repo).cloned() {
        // Check if this is a github source we can use convention for
        if tool_config.assets.is_empty() {
            if let Some(gh) = tool_config.source.strip_prefix("github:") {
                if let Some((owner, repo)) = gh.split_once('/') {
                    return install_from_github(
                        config,
                        owner,
                        repo,
                        parsed.version.as_deref(),
                        platform,
                    );
                }
            }
        }

        // Use explicit asset URLs from config
        return install_from_config(config, &parsed.repo, &tool_config, parsed.version.as_deref(), platform);
    }

    // Try each owner in the default owners list
    let owners = config.owners.clone();
    for owner in &owners {
        eprintln!("Trying {owner}/{}", parsed.repo);
        match install_from_github(config, owner, &parsed.repo, parsed.version.as_deref(), platform) {
            Ok(()) => return Ok(()),
            Err(e) if e.contains("No releases found") => continue,
            Err(e) if e.contains("No matching asset") => continue,
            Err(e) => return Err(e),
        }
    }

    Err(format!(
        "Tool '{}' not found in configured owners: {}\n\
         Use 'nab install owner/repo' to specify explicitly.",
        parsed.repo,
        owners.join(", ")
    ))
}

fn install_from_config(
    config: &mut Config,
    name: &str,
    tool_config: &ToolConfig,
    version: Option<&str>,
    platform: Platform,
) -> Result<(), String> {
    let asset_pattern = tool_config
        .assets
        .get(platform.key())
        .ok_or_else(|| format!("No asset configured for {name} on {}", platform.key()))?;

    let resolved_version = match version {
        Some("latest") | None => {
            if let Some(gh) = tool_config.source.strip_prefix("github:") {
                let (owner, repo) = gh.split_once('/')
                    .ok_or("Invalid github source")?;
                fetch_release(owner, repo, None)?.tag_name
            } else {
                return Err("Cannot resolve 'latest' for non-GitHub source".into());
            }
        }
        Some(v) => v.to_string(),
    };

    let download_url = asset_pattern.replace("{version}", &resolved_version);

    eprintln!("Installing {name}@{resolved_version} for {}...", platform.key());

    let data = download(&download_url)?;
    let binary = extract_binary(&data, &download_url, tool_config.bin_name.as_deref())?;

    let install_path = config.install_dir().join(name);
    atomic_install(&binary, &install_path)?;

    if let Some(tc) = config.tools.get_mut(name) {
        tc.version = resolved_version;
    }
    config.save()?;

    eprintln!("Installed {name} to {}", install_path.display());
    Ok(())
}

fn install_from_github(
    config: &mut Config,
    owner: &str,
    repo: &str,
    version: Option<&str>,
    platform: Platform,
) -> Result<(), String> {
    let version_query = match version {
        Some("latest") | None => None,
        Some(v) => Some(v),
    };

    eprintln!("Fetching release info for {owner}/{repo}...");
    let release = fetch_release(owner, repo, version_query)?;

    let asset = find_platform_asset(&release.assets, platform).ok_or_else(|| {
        let available: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
        format!(
            "No matching asset for {} in release {}.\nAvailable: {}",
            platform.key(),
            release.tag_name,
            available.join(", ")
        )
    })?;

    // Determine the binary name to install as
    let install_name = if is_raw_binary(&asset.name) {
        // Raw binary: strip platform suffix (e.g., "nab-darwin-arm64" -> "nab")
        strip_platform_suffix(&asset.name)
    } else {
        // Archive: use the repo name
        repo.to_string()
    };

    eprintln!(
        "Installing {install_name}@{} for {}...",
        release.tag_name,
        platform.key()
    );
    eprintln!("Found asset: {}", asset.name);

    let data = download(&asset.browser_download_url)?;
    let binary = extract_binary(&data, &asset.browser_download_url, None)?;

    let install_path = config.install_dir().join(&install_name);
    atomic_install(&binary, &install_path)?;

    // Save to config for future reference (keyed by install_name)
    config.tools.insert(
        install_name.clone(),
        ToolConfig {
            version: release.tag_name.clone(),
            source: format!("github:{owner}/{repo}"),
            assets: HashMap::new(), // Convention-based, no explicit URLs
            bin_name: None,
        },
    );
    config.save()?;

    eprintln!("Installed {install_name} to {}", install_path.display());
    Ok(())
}

/// Check if an asset is a raw binary (not an archive)
fn is_raw_binary(name: &str) -> bool {
    let lower = name.to_lowercase();
    !lower.ends_with(".tar.gz")
        && !lower.ends_with(".tgz")
        && !lower.ends_with(".zip")
        && !lower.ends_with(".tar")
        && !lower.ends_with(".gz")
}

/// Strip platform suffix from binary name (e.g., "nab-darwin-arm64" -> "nab")
fn strip_platform_suffix(name: &str) -> String {
    let platform_patterns = [
        "-darwin-arm64", "-darwin-amd64", "-darwin-x86_64", "-darwin-aarch64",
        "-macos-arm64", "-macos-amd64", "-macos-x86_64", "-macos-aarch64",
        "-linux-arm64", "-linux-amd64", "-linux-x86_64", "-linux-aarch64", "-linux-riscv64",
        "-windows-arm64", "-windows-amd64", "-windows-x86_64",
        "_darwin_arm64", "_darwin_amd64", "_darwin_x86_64", "_darwin_aarch64",
        "_macos_arm64", "_macos_amd64", "_macos_x86_64", "_macos_aarch64",
        "_linux_arm64", "_linux_amd64", "_linux_x86_64", "_linux_aarch64", "_linux_riscv64",
        "_windows_arm64", "_windows_amd64", "_windows_x86_64",
    ];

    let lower = name.to_lowercase();
    for pattern in platform_patterns {
        if lower.ends_with(pattern) {
            return name[..name.len() - pattern.len()].to_string();
        }
    }
    name.to_string()
}

fn cmd_upgrade(config: &mut Config, tool: Option<&str>, all: bool) -> Result<(), String> {
    if all {
        let tools: Vec<String> = config.tools.keys().cloned().collect();
        if tools.is_empty() {
            eprintln!("No tools configured.");
            return Ok(());
        }
        for tool in tools {
            eprintln!("\n--- Upgrading {tool} ---");
            if let Err(e) = cmd_install(config, &format!("{tool}@latest")) {
                eprintln!("Failed to upgrade {tool}: {e}");
            }
        }
        Ok(())
    } else if let Some(tool) = tool {
        cmd_install(config, &format!("{tool}@latest"))
    } else {
        Err("Specify a tool name or use --all".into())
    }
}

fn cmd_list(config: &Config) -> Result<(), String> {
    let install_dir = config.install_dir();

    if config.tools.is_empty() {
        println!("No tools configured.");
        return Ok(());
    }

    println!("{:<20} {:<15} {:<10} SOURCE", "TOOL", "VERSION", "INSTALLED");
    println!("{}", "-".repeat(70));

    for (name, tc) in &config.tools {
        let installed = install_dir.join(name).exists();
        let status = if installed { "✓" } else { "✗" };
        let source = if tc.assets.is_empty() {
            &tc.source
        } else {
            "(custom)"
        };
        println!("{:<20} {:<15} {:<10} {}", name, tc.version, status, source);
    }

    Ok(())
}

fn cmd_where(config: &Config, tool: &str) -> Result<(), String> {
    let path = config.install_dir().join(tool);
    if path.exists() {
        println!("{}", path.display());
        Ok(())
    } else {
        Err(format!("Tool '{tool}' not installed"))
    }
}

fn cmd_remove(config: &mut Config, tool: &str, keep_binary: bool) -> Result<(), String> {
    if config.tools.remove(tool).is_none() {
        return Err(format!("Tool '{tool}' not found in config"));
    }

    config.save()?;
    eprintln!("Removed '{tool}' from config");

    if !keep_binary {
        let binary_path = config.install_dir().join(tool);
        if binary_path.exists() {
            fs::remove_file(&binary_path)
                .map_err(|e| format!("Failed to remove binary: {e}"))?;
            eprintln!("Deleted {}", binary_path.display());
        }
    }

    Ok(())
}

fn cmd_config(
    config: &mut Config,
    name: String,
    source: String,
    darwin_arm64: Option<String>,
    linux_amd64: Option<String>,
    linux_riscv64: Option<String>,
    bin_name: Option<String>,
) -> Result<(), String> {
    let mut assets = HashMap::new();

    if let Some(url) = darwin_arm64 {
        assets.insert("darwin_arm64".to_string(), url);
    }
    if let Some(url) = linux_amd64 {
        assets.insert("linux_amd64".to_string(), url);
    }
    if let Some(url) = linux_riscv64 {
        assets.insert("linux_riscv64".to_string(), url);
    }

    let tool_config = ToolConfig {
        version: "latest".to_string(),
        source,
        assets,
        bin_name,
    };

    config.tools.insert(name.clone(), tool_config);
    config.save()?;

    eprintln!("Configured tool '{name}'");
    Ok(())
}

fn main() {
    let cli = Cli::parse();

    let mut config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Command::Install { spec } => cmd_install(&mut config, &spec),
        Command::Upgrade { tool, all } => cmd_upgrade(&mut config, tool.as_deref(), all),
        Command::List => cmd_list(&config),
        Command::Where { tool } => cmd_where(&config, &tool),
        Command::Remove { tool, keep_binary } => cmd_remove(&mut config, &tool, keep_binary),
        Command::Config {
            name,
            source,
            darwin_arm64,
            linux_amd64,
            linux_riscv64,
            bin_name,
        } => cmd_config(
            &mut config,
            name,
            source,
            darwin_arm64,
            linux_amd64,
            linux_riscv64,
            bin_name,
        ),
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
