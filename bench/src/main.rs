mod dashboard;
mod load;
mod orchestrate;
mod setup;
mod stats;

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures::future::join_all;
use integration_test_tools::loaded_accounts::LoadedAccounts;
use load::{LoadConfig, Shard};
use orchestrate::{scrape_executed, ErHandle, Repo, CHAIN_RPC_PORT};
use stats::{AppState, SweepPoint};

/// End-to-end USDC-transfer throughput benchmark for MagicBlock ephemeral
/// validators, demonstrating horizontal scaling across account shards.
#[derive(Parser)]
#[command(name = "er-bench")]
struct Args {
    /// Path to the magicblock-validator repo checkout.
    #[arg(long, default_value = "magicblock-validator")]
    repo: PathBuf,
    /// Validator counts to sweep, e.g. "1,2,4".
    #[arg(long, default_value = "1,2,4", value_delimiter = ',')]
    sweep: Vec<usize>,
    /// Users (delegated eATA holders) per validator shard; must be even.
    #[arg(long, default_value_t = 512)]
    users_per_shard: usize,
    /// Load duration per sweep point, in seconds.
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,
    /// Parallel HTTP connections per validator.
    #[arg(long, default_value_t = 8)]
    connections: usize,
    /// Transactions per JSON-RPC batch.
    #[arg(long, default_value_t = 400)]
    batch_size: usize,
    /// Signer threads per validator shard.
    #[arg(long, default_value_t = 3)]
    signer_threads: usize,
    /// ER block time in milliseconds.
    #[arg(long, default_value_t = 50)]
    blocktime_ms: u64,
    /// Pre-sign this many transfers per shard before the timed window,
    /// removing the client's just-in-time signing cost from the measurement.
    /// Use with --blocktime-ms 400 so the blockhash outlives sign + send.
    #[arg(long, default_value_t = 0)]
    presign_txs: usize,
    /// Dashboard port.
    #[arg(long, default_value_t = 3777)]
    dashboard_port: u16,
    /// Keep the dashboard (and validators' chain) up after the run.
    #[arg(long, default_value_t = false)]
    hold: bool,
}

static CHILD_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn register_pid(pid: u32) {
    if let Ok(mut pids) = CHILD_PIDS.lock() {
        pids.push(pid);
    }
}

fn kill_registered() {
    if let Ok(pids) = CHILD_PIDS.lock() {
        for pid in pids.iter() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.users_per_shard % 2 != 0 {
        bail!("--users-per-shard must be even (users are paired)");
    }

    ctrlc::set_handler(|| {
        eprintln!("\ninterrupted; stopping validators");
        kill_registered();
        std::process::exit(130);
    })?;

    let result = run(args).await;
    // Kill any validator still running, also on the error path.
    kill_registered();
    result
}

async fn run(args: Args) -> Result<()> {
    let max_n = *args.sweep.iter().max().context("empty sweep")?;

    let state = AppState::new();
    tokio::spawn(dashboard::serve(state.clone(), args.dashboard_port));

    state.set_phase("build", "building magicblock-validator (release)");
    let repo_for_build = Repo::locate(&args.repo)?;
    let bin = tokio::task::spawn_blocking(move || orchestrate::build_validator(&repo_for_build))
        .await??;

    let accounts = LoadedAccounts::with_delegation_program_test_authority();
    state.set_phase("chain", "starting local base chain (solana-test-validator)");
    let chain = {
        let repo = Repo::locate(&args.repo)?;
        let accounts = LoadedAccounts::with_delegation_program_test_authority();
        tokio::task::spawn_blocking(move || orchestrate::start_chain(&repo, &accounts)).await??
    };
    register_pid(chain.id());
    let mut chain = Some(chain);
    let chain_url = format!("http://127.0.0.1:{CHAIN_RPC_PORT}");

    let total_users = max_n * args.users_per_shard;
    state.set_phase(
        "setup",
        &format!("delegating {total_users} USDC accounts ({max_n} shards)"),
    );
    let setup = setup::Setup::new(&chain_url, accounts.validator_authority(), total_users);
    {
        let state = state.clone();
        setup
            .run(move |step| state.set_phase("setup", step))
            .await
            .context("setup failed")?;
    }

    let run_dir = PathBuf::from(".run");
    std::fs::create_dir_all(&run_dir)?;
    let http = reqwest::Client::new();
    let mut report_lines = Vec::new();

    for &n in &args.sweep {
        state.set_run(n, args.users_per_shard, n * args.users_per_shard);
        state.set_phase("start", &format!("starting {n} ephemeral validator(s)"));

        let mut ers: Vec<ErHandle> = Vec::new();
        for i in 0..n {
            let bin = bin.clone();
            let run_dir = run_dir.clone();
            let accounts = LoadedAccounts::with_delegation_program_test_authority();
            let blocktime = args.blocktime_ms;
            let er = tokio::task::spawn_blocking(move || {
                orchestrate::spawn_er(i, &bin, &run_dir, &accounts, blocktime)
            })
            .await??;
            register_pid(er.child.id());
            ers.push(er);
        }

        let shards: Vec<Arc<Shard>> = (0..n)
            .map(|s| {
                let range = setup.world.shard_range(s, args.users_per_shard);
                Arc::new(Shard {
                    users: setup.world.users[range.clone()].to_vec(),
                    atas: setup.world.atas[range].to_vec(),
                })
            })
            .collect();

        state.set_phase(
            "warmup",
            &format!("cloning {} accounts into {n} validator(s)", n * args.users_per_shard),
        );
        let baselines: Vec<Vec<u64>> = join_all(
            ers.iter()
                .zip(&shards)
                .map(|(er, shard)| load::warmup(er.rpc_url(), shard.clone())),
        )
        .await
        .into_iter()
        .collect::<Result<_>>()?;

        let executed_before = join_all(
            ers.iter()
                .map(|er| scrape_executed(http.clone(), er.metrics_url())),
        )
        .await;

        state.set_phase("load", &format!("blasting USDC transfers at {n} validator(s)"));
        let shard_stats = state.reset_shards(n);
        let stop = Arc::new(AtomicBool::new(false));

        // Sampler feeds the dashboard's live TPS chart.
        let sampler = {
            let state = state.clone();
            let stats = shard_stats.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let start = Instant::now();
                let mut last: Vec<u64> = vec![0; stats.len()];
                let mut last_t = 0f64;
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let t = start.elapsed().as_secs_f64();
                    let dt = (t - last_t).max(1e-6);
                    let mut per_shard = Vec::with_capacity(stats.len());
                    let mut aggregate = 0f64;
                    for (i, s) in stats.iter().enumerate() {
                        let now = s.accepted();
                        let tps = (now - last[i]) as f64 / dt;
                        last[i] = now;
                        aggregate += tps;
                        per_shard.push(tps);
                    }
                    last_t = t;
                    if let Ok(mut h) = state.history.lock() {
                        h.push(stats::HistoryPoint {
                            t,
                            aggregate_tps: aggregate,
                            per_shard,
                        });
                    }
                }
            })
        };

        let elapsed = if args.presign_txs > 0 {
            state.set_phase(
                "presign",
                &format!("pre-signing {} transfers per shard", args.presign_txs),
            );
            let batches: Vec<_> = join_all(ers.iter().zip(&shards).map(|(er, shard)| {
                load::presign(
                    er.rpc_url(),
                    shard.clone(),
                    args.presign_txs,
                    args.signer_threads.max(2) * 2,
                )
            }))
            .await
            .into_iter()
            .collect::<Result<_>>()?;

            state.set_phase("load", &format!("blasting pre-signed transfers at {n} validator(s)"));
            let start = Instant::now();
            let results = join_all(ers.iter().zip(batches).zip(&shard_stats).map(
                |((er, txs), stats)| {
                    load::send_presigned(
                        er.rpc_url(),
                        txs,
                        stats.clone(),
                        args.connections,
                        args.batch_size,
                    )
                },
            ))
            .await;
            let elapsed = start.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            let _ = sampler.await;
            for r in results {
                r?;
            }
            elapsed
        } else {
            let load_config = LoadConfig {
                duration: Duration::from_secs(args.duration_secs),
                connections: args.connections,
                batch_size: args.batch_size,
                signer_threads: args.signer_threads,
            };
            let start = Instant::now();
            let results = join_all(ers.iter().zip(&shards).zip(&shard_stats).map(
                |((er, shard), stats)| {
                    load::run_load(
                        er.rpc_url(),
                        shard.clone(),
                        stats.clone(),
                        &load_config,
                        stop.clone(),
                    )
                },
            ))
            .await;
            let elapsed = start.elapsed().as_secs_f64();
            stop.store(true, Ordering::Relaxed);
            let _ = sampler.await;
            for r in results {
                r?;
            }
            elapsed
        };

        // Give the engine a moment to drain, then read server-side counters.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let executed_after = join_all(
            ers.iter()
                .map(|er| scrape_executed(http.clone(), er.metrics_url())),
        )
        .await;
        let executed: Option<u64> = executed_before
            .iter()
            .zip(&executed_after)
            .map(|(b, a)| match (b, a) {
                (Some(b), Some(a)) => Some(a.saturating_sub(*b)),
                _ => None,
            })
            .sum();

        state.set_phase("verify", "checking balance conservation per pair");
        let mut ok_pairs = 0;
        let mut failed_pairs = 0;
        let mut changed_pairs = 0;
        for ((er, shard), baseline) in ers.iter().zip(&shards).zip(&baselines) {
            let (ok, failed, changed) =
                load::verify(&er.rpc_url(), shard.as_ref(), baseline).await?;
            ok_pairs += ok;
            failed_pairs += failed;
            changed_pairs += changed;
        }

        let accepted: u64 = shard_stats.iter().map(|s| s.accepted()).sum();
        let submitted: u64 = shard_stats.iter().map(|s| s.submitted()).sum();
        // Prefer the server-side executed count: client acceptance overcounts
        // transactions the sequencer later dropped under backpressure.
        let tps = executed.unwrap_or(accepted) as f64 / elapsed;
        let point = SweepPoint {
            validators: n,
            tps,
            per_node_tps: tps / n as f64,
            accepted,
            executed,
            verified_pairs: ok_pairs,
            failed_pairs,
            changed_pairs,
        };
        let line = format!(
            "validators={n} tps={tps:.0} per_node={:.0} accepted={accepted}/{submitted} executed={} pairs_ok={ok_pairs} pairs_failed={failed_pairs} pairs_active={changed_pairs}",
            point.per_node_tps,
            executed.map_or("n/a".to_string(), |e| e.to_string()),
        );
        println!("[result] {line}");
        report_lines.push(line);
        state.push_sweep(point);

        for er in ers.iter_mut() {
            er.stop();
        }
    }

    state.set_phase("done", "benchmark complete");
    println!("\n===== ER 1M TPS PoC results =====");
    for line in &report_lines {
        println!("{line}");
    }
    if let Ok(sweep) = state.sweep.lock() {
        if let Some(best) = sweep
            .iter()
            .map(|p| p.per_node_tps)
            .max_by(|a, b| a.total_cmp(b))
        {
            println!(
                "extrapolation: at {best:.0} TPS/node, 1M TPS needs ~{} validators",
                (1_000_000f64 / best).ceil() as u64
            );
        }
        let json = serde_json::to_string_pretty(&*sweep)?;
        std::fs::write("results.json", json)?;
        println!("wrote results.json");
    }

    if args.hold {
        println!("--hold: dashboard stays up; ctrl+c to exit");
        futures::future::pending::<()>().await;
    }

    if let Some(mut chain) = chain.take() {
        integration_test_tools::validator::stop_validator(&mut chain, Duration::from_secs(10));
    }
    Ok(())
}
