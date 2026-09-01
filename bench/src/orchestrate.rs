use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use integration_test_tools::{
    loaded_accounts::LoadedAccounts,
    toml_to_args::ProgramLoader,
    validator::{
        start_test_validator_with_config, stop_validator, wait_for_validator,
        TestRunnerPaths,
    },
};

pub const CHAIN_RPC_PORT: u16 = 7799;
pub const ER_BASE_PORT: u16 = 8899;
pub const ER_PORT_STRIDE: u16 = 10;

pub struct Repo {
    pub root: PathBuf,
    pub test_integration: PathBuf,
}

impl Repo {
    pub fn locate(root: &Path) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("repo root {} not found", root.display()))?;
        let test_integration = root.join("test-integration");
        if !test_integration.join("configs").is_dir() {
            bail!(
                "{} does not look like the magicblock-validator repo",
                root.display()
            );
        }
        Ok(Self {
            root,
            test_integration,
        })
    }
}

/// Builds the validator binary once in release mode and returns its path.
/// Pins the toolchain from the repo's rust-toolchain.toml so a rustup
/// directory override cannot select an incompatible compiler.
pub fn build_validator(repo: &Repo) -> Result<PathBuf> {
    println!("[build] cargo build --release -p magicblock-validator");
    let mut command = Command::new("cargo");
    command
        .args(["build", "--release", "-p", "magicblock-validator"])
        .current_dir(&repo.root);
    if let Some(channel) = repo_toolchain_channel(&repo.root) {
        command.env("RUSTUP_TOOLCHAIN", channel);
    }
    let status = command.status().context("failed to run cargo build")?;
    if !status.success() {
        bail!("cargo build --release -p magicblock-validator failed");
    }
    let bin = repo.root.join("target/release/magicblock-validator");
    if !bin.is_file() {
        bail!("built binary not found at {}", bin.display());
    }
    Ok(bin)
}

fn repo_toolchain_channel(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("rust-toolchain.toml")).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    Some(value.get("toolchain")?.get("channel")?.as_str()?.to_string())
}

/// Starts the local base chain (solana-test-validator) with DLP + eATA programs.
pub fn start_chain(repo: &Repo, accounts: &LoadedAccounts) -> Result<Child> {
    let paths = TestRunnerPaths {
        config_path: repo
            .test_integration
            .join("configs/cloning-conf.devnet.toml"),
        root_dir: repo.root.clone(),
        workspace_dir: repo.test_integration.clone(),
    };
    start_test_validator_with_config(
        &paths,
        Some(ProgramLoader::UpgradeableProgram),
        accounts,
        "CHAIN",
    )
    .context("failed to start solana-test-validator (is it installed?)")
}

pub struct ErHandle {
    pub child: Child,
    pub rpc_port: u16,
    pub metrics_port: u16,
    storage: PathBuf,
}

impl ErHandle {
    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port)
    }
    pub fn metrics_url(&self) -> String {
        format!("http://127.0.0.1:{}/metrics", self.metrics_port)
    }
    pub fn stop(&mut self) {
        stop_validator(&mut self.child, Duration::from_secs(10));
        let _ = fs::remove_dir_all(&self.storage);
    }
}

/// Spawns one ephemeral validator with deterministic ports:
/// rpc = 8899 + i*10 (ws = rpc+1), metrics = rpc+2, replication = rpc+3.
pub fn spawn_er(
    index: usize,
    bin: &Path,
    run_dir: &Path,
    accounts: &LoadedAccounts,
    blocktime_ms: u64,
) -> Result<ErHandle> {
    let rpc_port = ER_BASE_PORT + (index as u16) * ER_PORT_STRIDE;
    let metrics_port = rpc_port + 2;
    let replication_port = rpc_port + 3;

    let storage = run_dir.join(format!("er-{index}"));
    let _ = fs::remove_dir_all(&storage);
    fs::create_dir_all(&storage)?;

    let config = format!(
        r#"lifecycle = "ephemeral"
remotes = ["http://127.0.0.1:{CHAIN_RPC_PORT}", "ws://127.0.0.1:{}"]

[aperture]
listen = "0.0.0.0:{rpc_port}"

[commit]
compute-unit-price = 1_000_000

[engine.accountsdb]
lru-capacity = 1000000

[engine.blockstore]
blocktime = "{blocktime_ms}ms"

[ledger]
reset = true

[metrics]
address = "127.0.0.1:{metrics_port}"
"#,
        CHAIN_RPC_PORT + 1,
    );
    let config_path = storage.join("config.toml");
    fs::write(&config_path, config)?;

    let mut command = Command::new(bin);
    command
        .arg(&config_path)
        .env("RUST_LOG", "warn")
        .env("RUST_LOG_STYLE", format!("ER{index}"))
        .env(
            "MBV_ENGINE__AUTHORITY__LOCAL",
            accounts.validator_authority_base58(),
        )
        .env(
            "MBV_ENGINE__REPLICATION__BIND_ADDRESS",
            format!("127.0.0.1:{replication_port}"),
        )
        .env("MBV_ENGINE__LEDGER__DIRECTORY", storage.join("ledger"))
        .env(
            "MBV_ENGINE__ACCOUNTSDB__DIRECTORY",
            storage.join("accountsdb"),
        )
        .stdout(Stdio::from(fs::File::create(storage.join("stdout.log"))?))
        .stderr(Stdio::from(fs::File::create(storage.join("stderr.log"))?));

    println!("[er-{index}] starting on rpc={rpc_port} metrics={metrics_port}");
    let child = command.spawn().context("failed to spawn magicblock-validator")?;
    let child = wait_for_validator(child, rpc_port).ok_or_else(|| {
        anyhow::anyhow!("er-{index} never became ready; see {}", storage.display())
    })?;

    Ok(ErHandle {
        child,
        rpc_port,
        metrics_port,
        storage,
    })
}

/// Scrapes the executed-transaction estimate from a validator metrics
/// endpoint: transactions that entered processing minus terminal failures.
pub async fn scrape_executed(client: reqwest::Client, metrics_url: String) -> Option<u64> {
    let text = client
        .get(&metrics_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let received = metric_family_sum(&text, "mbv_transaction_processing_time_count")?;
    let failed =
        metric_family_sum(&text, "engine_processor_failed_transactions").unwrap_or(0);
    Some(received.saturating_sub(failed))
}

fn metric_family_sum(text: &str, family: &str) -> Option<u64> {
    let mut sum = 0f64;
    let mut found = false;
    for line in text.lines() {
        if line.starts_with('#') || !line.starts_with(family) {
            continue;
        }
        // Only exact family matches: next char must be '{' or ' '.
        match line.as_bytes().get(family.len()) {
            Some(b'{') | Some(b' ') => {}
            _ => continue,
        }
        if let Some(value) = line.rsplit(' ').next().and_then(|v| v.parse::<f64>().ok()) {
            sum += value;
            found = true;
        }
    }
    found.then_some(sum as u64)
}
