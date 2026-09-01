use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use base64::Engine;
use futures::{stream, StreamExt};
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    hash::Hash, program_pack::Pack, signature::Keypair, signer::Signer,
    transaction::Transaction,
};
use spl_token::instruction as spl_token_ix;

use crate::stats::ShardStats;

/// A shard of the world assigned to one ephemeral validator.
pub struct Shard {
    pub users: Vec<Arc<Keypair>>,
    pub atas: Vec<Pubkey>,
}

impl Shard {
    pub fn pairs(&self) -> usize {
        self.users.len() / 2
    }
    /// Pairs used by the bulk load; the last pair is reserved for the
    /// end-to-end latency probe so probe transactions never queue behind
    /// load transactions on the same accounts.
    pub fn load_pairs(&self) -> usize {
        (self.pairs() - 1).max(1)
    }
    fn probe_pair(&self) -> usize {
        self.pairs() - 1
    }
}

/// Measures end-to-end latency during load: every ~200ms sends one transfer
/// on the shard's reserved probe pair without skipPreflight, so the RPC
/// response returns only after the transaction has executed. The HTTP
/// roundtrip is therefore queue wait + execution + notification.
pub async fn latency_probe(
    er_url: String,
    shard: Arc<Shard>,
    stats: Arc<ShardStats>,
    stop: Arc<AtomicBool>,
) {
    let rpc = RpcClient::new_with_timeout(er_url, Duration::from_secs(30));
    let p = shard.probe_pair();
    let mut i: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        i += 1;
        let Ok(blockhash) = rpc.get_latest_blockhash().await else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let (from_idx, src, dst) = if i % 2 == 0 {
            (2 * p, shard.atas[2 * p], shard.atas[2 * p + 1])
        } else {
            (2 * p + 1, shard.atas[2 * p + 1], shard.atas[2 * p])
        };
        let amount = 1 + (i % 995);
        let Ok(tx) = transfer_tx(&shard.users[from_idx], &src, &dst, amount, blockhash) else {
            continue;
        };
        let start = Instant::now();
        if rpc.send_transaction(&tx).await.is_ok() {
            stats.record_latency(start.elapsed().as_secs_f64() * 1e3);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Materializes every projected ATA on the ER (triggers cloning), sends one
/// confirmed transfer per pair to warm the fee-payer/escrow path, and returns
/// the per-account baseline balances for post-run conservation checks.
pub async fn warmup(er_url: String, shard: Arc<Shard>) -> Result<Vec<u64>> {
    let rpc = Arc::new(RpcClient::new_with_timeout(
        er_url,
        Duration::from_secs(60),
    ));

    // Touching the ATA on the ER triggers clone + eATA projection.
    let results: Vec<Result<()>> = stream::iter(shard.atas.iter().map(|ata| {
        let rpc = rpc.clone();
        let ata = *ata;
        async move {
            let deadline = Instant::now() + Duration::from_secs(120);
            loop {
                match rpc.get_account(&ata).await {
                    Ok(_) => return Ok(()),
                    Err(_) if Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                    Err(err) => {
                        return Err(err).with_context(|| format!("materializing {ata}"))
                    }
                }
            }
        }
    }))
    .buffer_unordered(32)
    .collect()
    .await;
    for r in results {
        r?;
    }

    // One executed transfer per pair warms program cache and escrow lookups.
    // Without skipPreflight the RPC awaits the execution result, so an Ok
    // response means the transfer ran.
    let results: Vec<Result<()>> = stream::iter((0..shard.pairs()).map(|p| {
        let rpc = rpc.clone();
        let from = shard.users[2 * p].clone();
        let (src, dst) = (shard.atas[2 * p], shard.atas[2 * p + 1]);
        async move {
            let blockhash = rpc.get_latest_blockhash().await?;
            let tx = transfer_tx(&from, &src, &dst, 1, blockhash)?;
            rpc.send_transaction(&tx)
                .await
                .with_context(|| format!("warmup transfer for pair {p}"))?;
            Ok(())
        }
    }))
    .buffer_unordered(32)
    .collect()
    .await;
    for r in results {
        r?;
    }

    read_balances(&rpc, &shard.atas).await
}

pub async fn read_balances(rpc: &RpcClient, atas: &[Pubkey]) -> Result<Vec<u64>> {
    let mut balances = Vec::with_capacity(atas.len());
    for chunk in atas.chunks(100) {
        let accounts = rpc.get_multiple_accounts(chunk).await?;
        for (i, account) in accounts.into_iter().enumerate() {
            let account =
                account.with_context(|| format!("missing ATA {} on ER", chunk[i]))?;
            let token = spl_token::state::Account::unpack(&account.data)
                .with_context(|| format!("unpacking token account {}", chunk[i]))?;
            balances.push(token.amount);
        }
    }
    Ok(balances)
}

fn transfer_tx(
    from: &Keypair,
    src: &Pubkey,
    dst: &Pubkey,
    amount: u64,
    blockhash: Hash,
) -> Result<Transaction> {
    let ix = spl_token_ix::transfer(&spl_token::id(), src, dst, &from.pubkey(), &[], amount)?;
    let mut tx = Transaction::new_with_payer(&[ix], Some(&from.pubkey()));
    tx.sign(&[from], blockhash);
    Ok(tx)
}

pub struct LoadConfig {
    pub duration: Duration,
    pub connections: usize,
    pub batch_size: usize,
    pub signer_threads: usize,
}

/// Runs the load phase against one ER: signer threads produce base64
/// transactions just-in-time (fresh blockhash, rotating pairs, varying
/// amounts), sender tasks POST them in JSON-RPC batches.
pub async fn run_load(
    er_url: String,
    shard: Arc<Shard>,
    stats: Arc<ShardStats>,
    config: &LoadConfig,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let rpc = RpcClient::new_with_timeout(er_url.clone(), Duration::from_secs(10));
    let blockhash = Arc::new(RwLock::new(rpc.get_latest_blockhash().await?));

    // Refresh the blockhash well inside the ER's expiry window.
    let refresher = {
        let blockhash = blockhash.clone();
        let stop = stop.clone();
        let er_url = er_url.clone();
        tokio::spawn(async move {
            let rpc = RpcClient::new_with_timeout(er_url, Duration::from_secs(5));
            while !stop.load(Ordering::Relaxed) {
                if let Ok(hash) = rpc.get_latest_blockhash().await {
                    if let Ok(mut guard) = blockhash.write() {
                        *guard = hash;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        })
    };

    let (tx_sender, tx_receiver) =
        tokio::sync::mpsc::channel::<String>(config.batch_size * config.connections * 4);
    let receiver = Arc::new(tokio::sync::Mutex::new(tx_receiver));

    // Signer threads: each owns a disjoint slice of pairs.
    let pairs = shard.load_pairs();
    let threads = config.signer_threads.max(1).min(pairs);
    let mut signer_handles = Vec::new();
    for t in 0..threads {
        let shard = shard.clone();
        let blockhash = blockhash.clone();
        let stop = stop.clone();
        let sender = tx_sender.clone();
        let pair_range: Vec<usize> = (0..pairs).filter(|p| p % threads == t).collect();
        signer_handles.push(std::thread::spawn(move || {
            let engine = base64::engine::general_purpose::STANDARD;
            let mut counter: u64 = 0;
            'outer: loop {
                for &p in &pair_range {
                    if stop.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                    counter += 1;
                    // Alternate direction, vary amount to keep signatures unique.
                    let (from_idx, src, dst) = if counter % 2 == 0 {
                        (2 * p, shard.atas[2 * p], shard.atas[2 * p + 1])
                    } else {
                        (2 * p + 1, shard.atas[2 * p + 1], shard.atas[2 * p])
                    };
                    let amount = 1 + (counter % 995);
                    let hash = match blockhash.read() {
                        Ok(guard) => *guard,
                        Err(_) => break 'outer,
                    };
                    let Ok(tx) =
                        transfer_tx(&shard.users[from_idx], &src, &dst, amount, hash)
                    else {
                        continue;
                    };
                    let Ok(bytes) = bincode::serialize(&tx) else {
                        continue;
                    };
                    if sender.blocking_send(engine.encode(bytes)).is_err() {
                        break 'outer;
                    }
                }
            }
        }));
    }
    drop(tx_sender);

    // Sender tasks: drain batches and POST them.
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(config.connections * 2)
        .build()?;
    let mut sender_handles = Vec::new();
    for _ in 0..config.connections {
        let receiver = receiver.clone();
        let client = client.clone();
        let stats = stats.clone();
        let url = er_url.clone();
        let batch_size = config.batch_size;
        let stop = stop.clone();
        sender_handles.push(tokio::spawn(async move {
            let mut buffer = Vec::with_capacity(batch_size);
            loop {
                buffer.clear();
                {
                    let mut rx = receiver.lock().await;
                    if rx.recv_many(&mut buffer, batch_size).await == 0 {
                        break;
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut body = String::with_capacity(buffer.len() * 1750);
                body.push('[');
                for (i, b64) in buffer.iter().enumerate() {
                    if i > 0 {
                        body.push(',');
                    }
                    body.push_str(&format!(
                        r#"{{"jsonrpc":"2.0","id":{i},"method":"sendTransaction","params":["{b64}",{{"encoding":"base64","skipPreflight":true}}]}}"#
                    ));
                }
                body.push(']');
                stats
                    .submitted
                    .fetch_add(buffer.len() as u64, Ordering::Relaxed);
                let response = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match response {
                    Ok(resp) => match resp.json::<serde_json::Value>().await {
                        Ok(serde_json::Value::Array(entries)) => {
                            let ok = entries
                                .iter()
                                .filter(|e| e.get("result").is_some())
                                .count() as u64;
                            let failed = entries.len() as u64 - ok;
                            stats.accepted.fetch_add(ok, Ordering::Relaxed);
                            stats.errors.fetch_add(failed, Ordering::Relaxed);
                        }
                        _ => {
                            stats
                                .errors
                                .fetch_add(buffer.len() as u64, Ordering::Relaxed);
                        }
                    },
                    Err(_) => {
                        stats
                            .errors
                            .fetch_add(buffer.len() as u64, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    tokio::time::sleep(config.duration).await;
    stop.store(true, Ordering::Relaxed);

    for handle in sender_handles {
        let _ = handle.await;
    }
    let _ = refresher.await;
    for handle in signer_handles {
        let _ = handle.join();
    }
    Ok(())
}

/// Pre-signs `count` transfers for one shard against a single fresh
/// blockhash, in parallel on `threads` signer threads. Encoding of the tx
/// index guarantees unique (pair, direction, amount) triples, so no two
/// transactions share a signature. Pair with a long blocktime (e.g. 400ms)
/// so the blockhash outlives signing plus sending.
pub async fn presign(
    er_url: String,
    shard: Arc<Shard>,
    count: usize,
    threads: usize,
) -> Result<Arc<Vec<String>>> {
    let rpc = RpcClient::new_with_timeout(er_url, Duration::from_secs(10));
    let blockhash = rpc.get_latest_blockhash().await?;
    let pairs = shard.load_pairs();
    let threads = threads.max(1);
    tokio::task::spawn_blocking(move || {
        let stripes: Vec<Vec<String>> = std::thread::scope(|scope| {
            (0..threads)
                .map(|t| {
                    let shard = &shard;
                    scope.spawn(move || {
                        let engine = base64::engine::general_purpose::STANDARD;
                        let mut out = Vec::with_capacity(count / threads + 1);
                        let mut i = t;
                        while i < count {
                            let p = i % pairs;
                            let round = i / pairs;
                            let amount = 1 + (round / 2) as u64;
                            let (from_idx, src, dst) = if round % 2 == 0 {
                                (2 * p, shard.atas[2 * p], shard.atas[2 * p + 1])
                            } else {
                                (2 * p + 1, shard.atas[2 * p + 1], shard.atas[2 * p])
                            };
                            if let Ok(tx) = transfer_tx(
                                &shard.users[from_idx],
                                &src,
                                &dst,
                                amount,
                                blockhash,
                            ) {
                                if let Ok(bytes) = bincode::serialize(&tx) {
                                    out.push(engine.encode(bytes));
                                }
                            }
                            i += threads;
                        }
                        out
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .collect()
        });
        Ok(Arc::new(stripes.into_iter().flatten().collect()))
    })
    .await?
}

/// Blasts pre-signed transactions at one ER until the buffer is exhausted.
pub async fn send_presigned(
    er_url: String,
    txs: Arc<Vec<String>>,
    stats: Arc<ShardStats>,
    connections: usize,
    batch_size: usize,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(connections * 2)
        .build()?;
    let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..connections {
        let txs = txs.clone();
        let cursor = cursor.clone();
        let client = client.clone();
        let stats = stats.clone();
        let url = er_url.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let start = cursor.fetch_add(batch_size, Ordering::Relaxed);
                if start >= txs.len() {
                    break;
                }
                let end = (start + batch_size).min(txs.len());
                let slice = &txs[start..end];
                let mut body = String::with_capacity(slice.len() * 1750);
                body.push('[');
                for (i, b64) in slice.iter().enumerate() {
                    if i > 0 {
                        body.push(',');
                    }
                    body.push_str(&format!(
                        r#"{{"jsonrpc":"2.0","id":{i},"method":"sendTransaction","params":["{b64}",{{"encoding":"base64","skipPreflight":true}}]}}"#
                    ));
                }
                body.push(']');
                stats
                    .submitted
                    .fetch_add(slice.len() as u64, Ordering::Relaxed);
                let response = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await;
                match response {
                    Ok(resp) => match resp.json::<serde_json::Value>().await {
                        Ok(serde_json::Value::Array(entries)) => {
                            let ok = entries
                                .iter()
                                .filter(|e| e.get("result").is_some())
                                .count() as u64;
                            let failed = entries.len() as u64 - ok;
                            stats.accepted.fetch_add(ok, Ordering::Relaxed);
                            stats.errors.fetch_add(failed, Ordering::Relaxed);
                        }
                        _ => {
                            stats
                                .errors
                                .fetch_add(slice.len() as u64, Ordering::Relaxed);
                        }
                    },
                    Err(_) => {
                        stats
                            .errors
                            .fetch_add(slice.len() as u64, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
    Ok(())
}

/// Verifies conservation: for every pair, the sum of the two balances must
/// equal the baseline sum. Returns (ok_pairs, failed_pairs, changed_pairs);
/// `changed` counts pairs whose split moved, proving transfers executed.
pub async fn verify(
    er_url: &str,
    shard: &Shard,
    baseline: &[u64],
) -> Result<(usize, usize, usize)> {
    let rpc = RpcClient::new_with_timeout(er_url.to_string(), Duration::from_secs(60));
    let after = read_balances(&rpc, &shard.atas).await?;
    if after.len() != baseline.len() {
        bail!("balance count mismatch");
    }
    let mut ok = 0;
    let mut failed = 0;
    let mut changed = 0;
    for p in 0..shard.pairs() {
        let before = baseline[2 * p] as u128 + baseline[2 * p + 1] as u128;
        let now = after[2 * p] as u128 + after[2 * p + 1] as u128;
        if after[2 * p] != baseline[2 * p] {
            changed += 1;
        }
        if before == now {
            ok += 1;
        } else {
            failed += 1;
            if failed <= 5 {
                println!(
                    "[verify] pair {p} conservation broken: {before} -> {now} ({}, {})",
                    shard.atas[2 * p],
                    shard.atas[2 * p + 1]
                );
            }
        }
    }
    Ok((ok, failed, changed))
}
