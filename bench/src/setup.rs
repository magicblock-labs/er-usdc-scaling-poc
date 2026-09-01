use std::{sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use futures::{stream, StreamExt};
use integration_test_tools::dlp_interface::create_topup_ixs;
use magicblock_core::token_programs::{
    derive_ata, derive_eata, ASSOCIATED_TOKEN_PROGRAM_ID, EATA_PROGRAM_ID,
    TOKEN_PROGRAM_ID,
};
use solana_pubkey::Pubkey;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    native_token::LAMPORTS_PER_SOL,
    program_pack::Pack,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use solana_system_interface::{
    instruction as system_instruction, program as system_program,
};
use spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent;
use spl_token::{instruction as spl_token_ix, state::Mint};

pub const USDC_DECIMALS: u8 = 6;
/// 1,000 USDC per user, in base units.
pub const USER_DEPOSIT: u64 = 1_000_000_000;
/// Escrow lamports per user fee payer on the ER.
const ESCROW_LAMPORTS: u64 = LAMPORTS_PER_SOL / 100;

const INITIALIZE_EPHEMERAL_ATA: u8 = 0;
const INITIALIZE_GLOBAL_VAULT: u8 = 1;
const DEPOSIT_SPL_TOKENS: u8 = 2;
const DELEGATE_EPHEMERAL_ATA: u8 = 4;

/// All keypairs and derived addresses the benchmark operates on.
pub struct World {
    pub mint: Pubkey,
    pub users: Vec<Arc<Keypair>>,
    pub atas: Vec<Pubkey>,
    pub validator: Pubkey,
}

impl World {
    pub fn shard_range(&self, shard: usize, users_per_shard: usize) -> std::ops::Range<usize> {
        shard * users_per_shard..(shard + 1) * users_per_shard
    }
}

fn derive_global_vault(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[mint.as_ref()], &EATA_PROGRAM_ID).0
}

fn initialize_global_vault_ix(payer: Pubkey, mint: Pubkey) -> Instruction {
    let vault = derive_global_vault(&mint);
    Instruction {
        program_id: EATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(vault, false),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(derive_eata(&vault, &mint), false),
            AccountMeta::new(derive_ata(&vault, &mint), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: vec![INITIALIZE_GLOBAL_VAULT],
    }
}

fn initialize_eata_ix(payer: Pubkey, user: Pubkey, mint: Pubkey) -> Instruction {
    Instruction {
        program_id: EATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(derive_eata(&user, &mint), false),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(user, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: vec![INITIALIZE_EPHEMERAL_ATA],
    }
}

fn deposit_spl_tokens_ix(user: Pubkey, mint: Pubkey, amount: u64) -> Instruction {
    let vault = derive_global_vault(&mint);
    let mut data = Vec::with_capacity(9);
    data.push(DEPOSIT_SPL_TOKENS);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: EATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(derive_eata(&user, &mint), false),
            AccountMeta::new_readonly(vault, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new(derive_ata(&user, &mint), false),
            AccountMeta::new(derive_ata(&vault, &mint), false),
            AccountMeta::new_readonly(user, true),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

fn delegate_eata_ix(payer: Pubkey, user: Pubkey, mint: Pubkey, validator: Pubkey) -> Instruction {
    let eata = derive_eata(&user, &mint);
    let delegation_buffer =
        dlp_api::pda::delegate_buffer_pda_from_delegated_account_and_owner_program(
            &eata,
            &EATA_PROGRAM_ID,
        );
    let mut data = Vec::with_capacity(33);
    data.push(DELEGATE_EPHEMERAL_ATA);
    data.extend_from_slice(validator.as_ref());
    Instruction {
        program_id: EATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(eata, false),
            AccountMeta::new_readonly(EATA_PROGRAM_ID, false),
            AccountMeta::new(delegation_buffer, false),
            AccountMeta::new(
                dlp_api::pda::delegation_record_pda_from_delegated_account(&eata),
                false,
            ),
            AccountMeta::new(
                dlp_api::pda::delegation_metadata_pda_from_delegated_account(&eata),
                false,
            ),
            AccountMeta::new_readonly(dlp_api::id(), false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    }
}

/// One transaction of a setup plan: instructions plus the signers (payer first).
struct PlannedTx {
    ixs: Vec<Instruction>,
    signer_indexes: Vec<SignerRef>,
}

#[derive(Clone, Copy)]
enum SignerRef {
    Payer,
    Mint,
    User(usize),
}

pub struct Setup {
    pub payer: Arc<Keypair>,
    mint_kp: Arc<Keypair>,
    pub world: World,
    rpc: Arc<RpcClient>,
}

impl Setup {
    pub fn new(chain_url: &str, validator: Pubkey, total_users: usize) -> Self {
        let users: Vec<Arc<Keypair>> =
            (0..total_users).map(|_| Arc::new(Keypair::new())).collect();
        let mint_kp = Arc::new(Keypair::new());
        let mint = mint_kp.pubkey();
        let atas = users
            .iter()
            .map(|u| derive_ata(&u.pubkey(), &mint))
            .collect();
        Self {
            payer: Arc::new(Keypair::new()),
            mint_kp,
            world: World {
                mint,
                users,
                atas,
                validator,
            },
            rpc: Arc::new(RpcClient::new_with_timeout(
                chain_url.to_string(),
                Duration::from_secs(30),
            )),
        }
    }

    fn resolve_signers(&self, refs: &[SignerRef]) -> Vec<Arc<Keypair>> {
        refs.iter()
            .map(|r| match r {
                SignerRef::Payer => self.payer.clone(),
                SignerRef::Mint => self.mint_kp.clone(),
                SignerRef::User(i) => self.world.users[*i].clone(),
            })
            .collect()
    }

    pub async fn run(&self, progress: impl Fn(&str)) -> Result<()> {
        let mint = self.world.mint;
        let payer = self.payer.pubkey();

        progress("funding master payer");
        self.fund_payer().await?;

        progress("creating USDC mint + global vault");
        let mint_rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(Mint::LEN)
            .await?;
        let mint_plan = vec![PlannedTx {
            ixs: vec![
                system_instruction::create_account(
                    &payer,
                    &mint,
                    mint_rent,
                    Mint::LEN as u64,
                    &spl_token::id(),
                ),
                spl_token_ix::initialize_mint(
                    &spl_token::id(),
                    &mint,
                    &payer,
                    None,
                    USDC_DECIMALS,
                )?,
                initialize_global_vault_ix(payer, mint),
            ],
            signer_indexes: vec![SignerRef::Payer, SignerRef::Mint],
        }];
        self.execute_plan(mint_plan, "mint").await?;

        progress("creating ATAs, minting USDC, initializing eATAs");
        let plan = self.chunked_plan(3, |i| {
            vec![
                create_associated_token_account_idempotent(
                    &payer,
                    &self.world.users[i].pubkey(),
                    &mint,
                    &spl_token::id(),
                ),
                spl_token_ix::mint_to(
                    &spl_token::id(),
                    &mint,
                    &self.world.atas[i],
                    &payer,
                    &[],
                    USER_DEPOSIT,
                )
                .expect("mint_to ix"),
                initialize_eata_ix(payer, self.world.users[i].pubkey(), mint),
            ]
        });
        self.execute_plan(plan, "ata+mint+eata").await?;

        progress("depositing USDC into eATAs");
        let plan = self
            .world
            .users
            .chunks(3)
            .enumerate()
            .map(|(chunk, users)| {
                let base = chunk * 3;
                let mut ixs = Vec::new();
                let mut signers = vec![SignerRef::Payer];
                for (j, user) in users.iter().enumerate() {
                    ixs.push(deposit_spl_tokens_ix(user.pubkey(), mint, USER_DEPOSIT));
                    signers.push(SignerRef::User(base + j));
                }
                PlannedTx {
                    ixs,
                    signer_indexes: signers,
                }
            })
            .collect();
        self.execute_plan(plan, "deposit").await?;

        progress("delegating eATAs to the validator");
        let plan = self.chunked_plan(3, |i| {
            vec![delegate_eata_ix(
                payer,
                self.world.users[i].pubkey(),
                mint,
                self.world.validator,
            )]
        });
        self.execute_plan(plan, "delegate").await?;

        progress("funding + delegating fee escrows");
        // delegate_ephemeral_balance requires the escrow owner's signature.
        let plan = self
            .world
            .users
            .chunks(2)
            .enumerate()
            .map(|(chunk, users)| {
                let base = chunk * 2;
                let mut ixs = Vec::new();
                let mut signers = vec![SignerRef::Payer];
                for (j, user) in users.iter().enumerate() {
                    ixs.extend(create_topup_ixs(
                        payer,
                        user.pubkey(),
                        ESCROW_LAMPORTS,
                        Some(self.world.validator),
                    ));
                    signers.push(SignerRef::User(base + j));
                }
                PlannedTx {
                    ixs,
                    signer_indexes: signers,
                }
            })
            .collect();
        self.execute_plan(plan, "escrow").await?;

        Ok(())
    }

    /// Builds a plan grouping `per_tx` users into each transaction, payer-signed.
    fn chunked_plan(
        &self,
        per_tx: usize,
        mut ixs_for_user: impl FnMut(usize) -> Vec<Instruction>,
    ) -> Vec<PlannedTx> {
        (0..self.world.users.len())
            .collect::<Vec<_>>()
            .chunks(per_tx)
            .map(|chunk| PlannedTx {
                ixs: chunk.iter().flat_map(|&i| ixs_for_user(i)).collect(),
                signer_indexes: vec![SignerRef::Payer],
            })
            .collect()
    }

    async fn fund_payer(&self) -> Result<()> {
        // Rent for ATA + eATA + escrow + delegation PDAs, with headroom.
        let per_user = ESCROW_LAMPORTS + LAMPORTS_PER_SOL / 50;
        let needed = per_user * self.world.users.len() as u64 + 10 * LAMPORTS_PER_SOL;
        let mut funded = 0u64;
        while funded < needed {
            let chunk = (needed - funded).min(500 * LAMPORTS_PER_SOL);
            let sig = self
                .rpc
                .request_airdrop(&self.payer.pubkey(), chunk)
                .await
                .context("airdrop request failed")?;
            self.await_confirmation(&[sig], Duration::from_secs(30))
                .await?;
            funded += chunk;
        }
        Ok(())
    }

    /// Signs, sends, and confirms every planned transaction, with bounded
    /// concurrency and blockhash-refresh retries.
    async fn execute_plan(&self, plan: Vec<PlannedTx>, label: &str) -> Result<()> {
        let total = plan.len();
        let mut pending = plan;
        for attempt in 0..4 {
            if pending.is_empty() {
                break;
            }
            let blockhash = self.rpc.get_latest_blockhash().await?;
            let results: Vec<(usize, Option<Signature>)> = stream::iter(
                pending.iter().enumerate().map(|(i, planned)| {
                    let rpc = self.rpc.clone();
                    let signers = self.resolve_signers(&planned.signer_indexes);
                    let tx = sign_tx(&planned.ixs, &signers, blockhash);
                    async move {
                        let sig = rpc
                            .send_transaction_with_config(
                                &tx,
                                RpcSendTransactionConfig {
                                    skip_preflight: true,
                                    ..Default::default()
                                },
                            )
                            .await
                            .ok();
                        (i, sig)
                    }
                }),
            )
            .buffer_unordered(64)
            .collect()
            .await;

            let sent: Vec<(usize, Signature)> = results
                .into_iter()
                .filter_map(|(i, sig)| sig.map(|s| (i, s)))
                .collect();
            let confirmed = self
                .confirm_all(sent.iter().map(|(_, s)| *s).collect(), Duration::from_secs(45))
                .await?;

            let mut still_pending = Vec::new();
            for (idx, planned) in pending.into_iter().enumerate() {
                let done = sent
                    .iter()
                    .position(|(i, _)| *i == idx)
                    .map(|pos| confirmed[pos])
                    .unwrap_or(false);
                if !done {
                    still_pending.push(planned);
                }
            }
            pending = still_pending;
            if !pending.is_empty() {
                println!(
                    "[setup:{label}] attempt {}: {}/{} txs unconfirmed, retrying",
                    attempt + 1,
                    pending.len(),
                    total
                );
            }
        }
        if !pending.is_empty() {
            bail!("[setup:{label}] {} of {} txs failed after retries", pending.len(), total);
        }
        println!("[setup:{label}] {total} txs confirmed");
        Ok(())
    }

    /// Returns a bool per signature: confirmed without error.
    async fn confirm_all(&self, sigs: Vec<Signature>, timeout: Duration) -> Result<Vec<bool>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut confirmed = vec![false; sigs.len()];
        while std::time::Instant::now() < deadline {
            let mut all = true;
            for (chunk_start, chunk) in sigs.chunks(200).enumerate().map(|(c, s)| (c * 200, s)) {
                let statuses = self.rpc.get_signature_statuses(chunk).await?;
                for (j, status) in statuses.value.into_iter().enumerate() {
                    let idx = chunk_start + j;
                    if confirmed[idx] {
                        continue;
                    }
                    match status {
                        Some(s) if s.err.is_some() => {
                            bail!("setup tx {} failed: {:?}", sigs[idx], s.err)
                        }
                        Some(_) => confirmed[idx] = true,
                        None => all = false,
                    }
                }
            }
            if all {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(confirmed)
    }

    async fn await_confirmation(&self, sigs: &[Signature], timeout: Duration) -> Result<()> {
        let ok = self.confirm_all(sigs.to_vec(), timeout).await?;
        if !ok.iter().all(|c| *c) {
            bail!("transaction(s) not confirmed within {:?}", timeout);
        }
        Ok(())
    }
}

fn sign_tx(ixs: &[Instruction], signers: &[Arc<Keypair>], blockhash: Hash) -> Transaction {
    let keypairs: Vec<&Keypair> = signers.iter().map(|k| k.as_ref()).collect();
    let mut tx = Transaction::new_with_payer(ixs, Some(&keypairs[0].pubkey()));
    tx.sign(&keypairs, blockhash);
    tx
}
