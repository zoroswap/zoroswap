use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use alloy::primitives::{I256, U256};
use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::{
    Felt, Word,
    account::{
        Account, AccountBuilder, AccountComponent, AccountId, AccountStorageMode, AccountType,
        StorageMap, StorageSlot, StorageSlotName, component::BasicWallet,
    },
    asset::AssetVault,
    auth::{AuthFalcon512Rpo, AuthSecretKey},
    keystore::FilesystemKeyStore,
    note::{Note, NoteDetails, NoteId, NoteRecipient, NoteTag},
    rpc::Endpoint,
    transaction::{TransactionKernel, TransactionRequestBuilder},
    vm::AdviceMap,
};
use rand::RngCore;
use tracing::error;

use crate::{
    account::MidenAccount,
    client::{MidenClient, create_library},
    curve::get_curve_amount_out,
    note::{NoteInstructions, TrustedNote},
    pool_state::{PoolBalances, PoolMetadata, PoolSettings, PoolState},
    price::PriceData,
};

#[derive(Debug, Clone, Copy)]
pub struct LiquidityPoolConfig {
    pub name: &'static str,
    pub symbol: &'static str,
    pub decimals: u8,
    pub faucet_id: AccountId,
    pub oracle_id: &'static str,
}

pub struct ZoroPool {
    miden_account: MidenAccount,
    miden_client: MidenClient,
    pool_states: HashMap<AccountId, PoolState>,
    liquidity_pools: Vec<LiquidityPoolConfig>,
}

impl ZoroPool {
    pub fn pool_states(&self) -> &HashMap<AccountId, PoolState> {
        &self.pool_states
    }

    pub async fn new_from_existing_pool(
        endpoint: Endpoint,
        keystore_path: &str,
        store_path: &str,
        pool_account_id: &AccountId,
        liquidity_pools: Vec<LiquidityPoolConfig>,
    ) -> Result<Self> {
        let miden_account = MidenAccount::new(*pool_account_id, None);
        let miden_client = MidenClient::new(endpoint, keystore_path, store_path, None).await?;
        let mut zoro_pool = Self {
            miden_client,
            miden_account,
            pool_states: HashMap::with_capacity(liquidity_pools.len()),
            liquidity_pools,
        };
        zoro_pool.update_pool_state_from_chain().await?;
        Ok(zoro_pool)
    }

    pub async fn new_deployment(
        liquidity_pools: Vec<LiquidityPoolConfig>,
        endpoint: Endpoint,
        keystore_path: &str,
        store_path: &str,
    ) -> Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let masm_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
            .iter()
            .collect();
        let mut miden_client =
            MidenClient::new(endpoint.clone(), keystore_path, store_path, None).await?;
        let pool_reader_path = Path::new(&masm_path);
        let pool_code = std::fs::read_to_string(pool_reader_path)
            .unwrap_or_else(|err| panic!("unable to read from {pool_reader_path:?}: {err}"));

        let assembler = TransactionKernel::assembler();

        let mut assets_mapping = StorageMap::new();
        let mut curves_mapping = StorageMap::new();
        let mut fees_mapping = StorageMap::new();

        let pool_settings = PoolSettings {
            beta: I256::from_str("10000000000000000")?,
            c: I256::from_str("16000000000000000000")?,
            swap_fee: U256::from(200),
            backstop_fee: U256::from(300),
            protocol_fee: U256::from(0),
        };

        for (i, pool) in liquidity_pools.iter().enumerate() {
            let fees: Word = [
                Felt::new(pool_settings.swap_fee.to::<u64>()),
                Felt::new(pool_settings.backstop_fee.to::<u64>()),
                Felt::new(pool_settings.protocol_fee.to::<u64>()),
                Felt::new(0), // 0
            ]
            .into();
            let curve: Word = [
                Felt::new(pool_settings.beta.to_string().parse::<u64>()?),
                Felt::new(pool_settings.c.to_string().parse::<u64>()?),
                Felt::new(0),
                Felt::new(0),
            ]
            .into();
            let asset_index = [
                Felt::new(i as u64),
                Felt::new(0),
                Felt::new(0),
                Felt::new(0),
            ];
            let asset_id = [
                Felt::new(0),
                Felt::new(0),
                pool.faucet_id.suffix(),
                pool.faucet_id.prefix().as_felt(),
            ];
            assets_mapping
                .insert(asset_index.into(), asset_id.into())
                .unwrap_or_else(|err| panic!("Failed to insert asset into mapping: {err:?}"));
            fees_mapping
                .insert(asset_id.into(), fees)
                .unwrap_or_else(|err| panic!("Failed to insert fees into mapping: {err:?}"));
            curves_mapping
                .insert(asset_id.into(), curve)
                .unwrap_or_else(|err| panic!("Failed to insert curve into mapping: {err:?}"));
        }

        let n = |name: &str| StorageSlotName::new(name).expect("valid slot name");

        let fees_mapping = StorageSlot::with_map(n("zoroswap::fees"), fees_mapping);
        let pool_states_mapping = StorageSlot::with_empty_map(n("zoroswap::pool_state"));
        let user_deposits_mapping = StorageSlot::with_empty_map(n("zoroswap::user_deposits"));

        // Compile the account code into a Library, then create AccountComponent
        let pool_library = create_library(assembler.clone(), "zoroswap::zoropool", &pool_code)
            .map_err(|e| anyhow!("Failed to create pool library: {e}"))?;
        let pool_component = AccountComponent::new(
            pool_library,
            vec![
                StorageSlot::with_empty_value(n("zoroswap::slot0")),
                StorageSlot::with_empty_value(n("zoroswap::slot1")),
                StorageSlot::with_map(n("zoroswap::assets"), assets_mapping),
                pool_states_mapping,
                user_deposits_mapping,
                StorageSlot::with_map(n("zoroswap::pool_curve"), curves_mapping),
                fees_mapping,
                StorageSlot::with_empty_value(n("zoroswap::pool_balances")),
                StorageSlot::with_empty_value(n("zoroswap::lp_supply")),
                StorageSlot::with_empty_value(n("zoroswap::pool_fees")),
                StorageSlot::with_empty_value(n("zoroswap::slot10")),
                StorageSlot::with_empty_value(n("zoroswap::slot11")),
                StorageSlot::with_empty_value(n("zoroswap::slot12")),
            ],
        )?
        .with_supports_all_types();

        // Init seed for the pool contract
        let mut init_seed = [0_u8; 32];
        miden_client.client_mut().rng().fill_bytes(&mut init_seed);

        let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(miden_client.client_mut().rng());

        // Build the new `Account` with the component
        let pool_contract = AccountBuilder::new(init_seed)
            .account_type(AccountType::RegularAccountUpdatableCode)
            .storage_mode(AccountStorageMode::Public)
            .with_component(pool_component.clone())
            .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
            .with_component(BasicWallet)
            .build()?;

        println!(
            "pool contract commitment hash: {:?}",
            pool_contract.commitment().to_hex()
        );
        println!(
            "contract id: {:?}",
            pool_contract.id().to_bech32(endpoint.to_network_id())
        );

        let keystore = FilesystemKeyStore::new(keystore_path.into())?;
        keystore.add_key(&key_pair)?;
        miden_client
            .client_mut()
            .add_account(&pool_contract.clone(), false)
            .await?;
        miden_client.sync_state().await?;

        let mut pool_states = HashMap::with_capacity(liquidity_pools.len());
        for pool in liquidity_pools.iter() {
            pool_states.insert(
                pool.faucet_id,
                PoolState::new(pool_settings, PoolBalances::default(), 0, pool.into()),
            );
        }

        Ok(Self {
            miden_account: MidenAccount::new(pool_contract.id(), Some(pool_contract)),
            pool_states,
            liquidity_pools,
            miden_client,
        })
    }

    pub async fn update_pool_state_from_chain(&mut self) -> Result<()> {
        let acc = self.miden_account.account().await?;
        for pool in self.liquidity_pools.iter() {
            let (settings, balances, lp_total_supply) =
                Self::extract_liqudity_pool_state_from_account(&acc, pool.faucet_id).await?;
            let pool_state = PoolState::new(
                settings,
                balances,
                lp_total_supply,
                PoolMetadata::from(pool),
            );
            self.pool_states.insert(pool.faucet_id, pool_state);
        }
        Ok(())
    }

    async fn extract_liqudity_pool_state_from_account(
        account: &Account,
        faucet_id: AccountId,
    ) -> Result<(PoolSettings, PoolBalances, u64)> {
        let storage = account.storage();
        let balances_slot = StorageSlotName::new("zoroswap::pool_state").expect("valid slot name");
        let curve_slot = StorageSlotName::new("zoroswap::pool_curve").expect("valid slot name");
        let fees_slot = StorageSlotName::new("zoroswap::fees").expect("valid slot name");
        let lp_supply_slot =
            StorageSlotName::new("zoroswap::user_deposits").expect("valid slot name");
        let asset_address: Word = [
            Felt::new(0),
            Felt::new(0),
            faucet_id.suffix(),
            faucet_id.prefix().as_felt(),
        ]
        .into();

        let pool_balances_raw = storage.get_map_item(&balances_slot, asset_address)?;
        let pool_curve = storage.get_map_item(&curve_slot, asset_address)?;
        let pool_fees = storage.get_map_item(&fees_slot, asset_address)?;

        let balances = PoolBalances {
            reserve_with_slippage: U256::from(pool_balances_raw[1].as_int()),
            reserve: U256::from(pool_balances_raw[2].as_int()),
            total_liabilities: U256::from(pool_balances_raw[3].as_int()),
        };
        let settings = PoolSettings {
            beta: I256::from_str(&pool_curve[0].as_int().to_string())?,
            c: I256::from_str(&pool_curve[1].as_int().to_string())?,
            swap_fee: U256::from(pool_fees[0].as_int()),
            backstop_fee: U256::from(pool_fees[1].as_int()),
            protocol_fee: U256::from(pool_fees[2].as_int()),
        };
        let total_supply_raw = storage.get_map_item(&lp_supply_slot, asset_address)?;
        let lp_total_supply = total_supply_raw[0].as_int();

        Ok((settings, balances, lp_total_supply))
    }

    pub async fn vault(&mut self) -> Result<AssetVault> {
        Ok(self.miden_account_mut().account().await?.vault().clone())
    }

    pub fn miden_account(&self) -> &MidenAccount {
        &self.miden_account
    }
    pub fn miden_account_mut(&mut self) -> &mut MidenAccount {
        &mut self.miden_account
    }

    pub fn print_pool_states(&self) {
        for pool in self.liquidity_pools.iter() {
            if let Some(pool_state) = self.pool_states.get(&pool.faucet_id) {
                pool_state.print_pool_state();
            }
        }
    }

    pub async fn execute_notes(
        &mut self,
        notes: Vec<TrustedNote>,
        prices: HashMap<AccountId, PriceData>,
    ) -> Result<Vec<(NoteId, ExecutionResult)>> {
        self.miden_client.sync_state().await?;
        let mut advice_map = AdviceMap::default();
        let mut input_notes = Vec::with_capacity(notes.len());
        let mut expected_future_notes = Vec::with_capacity(notes.len());
        let mut expected_output_recipients = Vec::with_capacity(notes.len());
        let mut pool_states = self.pool_states.clone();
        let mut results: Vec<(NoteId, ExecutionResult)> = Vec::new();
        for note in notes {
            let note_id = note.note().id();
            let ExecutionDetails {
                advice_map_value,
                input_note,
                expected_future_note,
                expected_output_recipient,
                new_pool_states,
                result,
            } = self.prepare_note_execution_details(note, &pool_states, &prices)?;
            if let Some(advice_map_value) = advice_map_value {
                advice_map.insert(advice_map_value.0, advice_map_value.1);
            };
            if let Some(input_note) = input_note {
                input_notes.push(input_note);
            };
            if let Some(expected_future_note) = expected_future_note {
                expected_future_notes.push(expected_future_note);
            };
            if let Some(expected_output_recipient) = expected_output_recipient {
                expected_output_recipients.push(expected_output_recipient);
            };
            if let Some(new_pool_states) = new_pool_states {
                pool_states = new_pool_states
            };
            results.push((note_id, result));
        }
        let len_future_notes = expected_future_notes.len();
        let len_output_recipients = expected_output_recipients.len();
        let len_input_notes = input_notes.len();
        let len_advice_map = advice_map.len();
        let consume_req = TransactionRequestBuilder::new()
            .extend_advice_map(advice_map)
            .input_notes(input_notes)
            .expected_future_notes(expected_future_notes)
            .expected_output_recipients(expected_output_recipients)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build batch transaction request: {}", e))?;
        let tx_id = self
            .miden_client
            .client_mut()
            .submit_new_transaction(*self.miden_account.id(), consume_req)
            .await
            .map_err(|e| {
                error!(
                    "Failed to submit batch transaction",
                    error = ?e,
                    pool_id = %self.miden_account.id().to_hex(),
                    input_notes = len_input_notes,
                    advice_map = len_advice_map,
                    expected_future_notes = len_future_notes,
                    expected_output_recipients = len_output_recipients
                );
                e
            })?;
        MidenClient::print_transaction_info(&tx_id);
        self.pool_states = pool_states;
        Ok(results)
    }

    fn prepare_note_execution_details(
        &self,
        note: TrustedNote,
        pool_states: &HashMap<AccountId, PoolState>,
        prices: &HashMap<AccountId, PriceData>,
    ) -> Result<ExecutionDetails> {
        let created_at = note.created_at();
        let note_instructions: NoteInstructions = note.clone().try_into()?;
        let now = Utc::now().timestamp_millis();
        let mut new_pool_states = pool_states.clone();
        match note_instructions {
            NoteInstructions::Deposit(instructions) => {
                let past_deadline = now > created_at + instructions.deadline as i64;
                let mut pool_state = *pool_states
                    .get(&instructions.asset_in)
                    .ok_or(anyhow!("Trying to execute deposit for an unknown asset."))?;
                if !past_deadline
                    && let Ok((lp_amount, new_lp_total_supply, new_pool_balances)) =
                        pool_state.get_deposit_lp_amount_out(U256::from(instructions.amount_in))
                {
                    pool_state.update_state(new_pool_balances, new_lp_total_supply);
                    new_pool_states.insert(instructions.asset_in, pool_state);
                    Ok(ExecutionDetails {
                        advice_map_value: None,
                        input_note: Some((
                            note.note().clone(),
                            Some(pool_state.to_lp_note_args(0)), // amount 0 will reject the deposit in masm
                        )),
                        expected_future_note: None,
                        new_pool_states: None,
                        expected_output_recipient: None,
                        result: ExecutionResult::DepositSuccess(lp_amount.to::<u64>()),
                    })
                } else {
                    // Return the asset back to the creator of this failed deposit
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.asset_in,
                        instructions.amount_in,
                        Some(note.serial_number()),
                    )?;
                    Ok(ExecutionDetails {
                        advice_map_value: None,
                        input_note: Some((
                            note.note().clone(),
                            Some(pool_state.to_lp_note_args(0)), // amount 0 will reject the deposit in masm
                        )),
                        expected_future_note: Some((
                            p2id.note().clone().into(),
                            p2id.note().metadata().tag(),
                        )),
                        new_pool_states: None,
                        expected_output_recipient: Some(p2id.note().recipient().clone()),
                        result: if past_deadline {
                            ExecutionResult::PastDeadline
                        } else {
                            ExecutionResult::Failed
                        },
                    })
                }
            }
            NoteInstructions::Withdraw(instructions) => {
                let past_deadline = now > created_at + instructions.deadline as i64;
                let mut pool_state = *pool_states.get(&instructions.asset_out).ok_or(anyhow!(
                    "Trying to execute withdrawal for an unknown asset."
                ))?;
                if !past_deadline
                    && let Ok((amount_out, new_lp_total_supply, new_pool_balances)) = pool_state
                        .get_withdraw_asset_amount_out(U256::from(instructions.lp_amount_in))
                    && amount_out >= instructions.min_amount_out
                {
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.asset_out,
                        amount_out.to::<u64>(),
                        Some(note.serial_number()),
                    )?;
                    pool_state.update_state(new_pool_balances, new_lp_total_supply);
                    new_pool_states.insert(instructions.asset_out, pool_state);
                    Ok(ExecutionDetails {
                        advice_map_value: None,
                        input_note: Some((
                            note.note().clone(),
                            Some(pool_state.to_lp_note_args(amount_out.to::<u64>())),
                        )),
                        expected_future_note: Some((
                            p2id.note().clone().into(),
                            p2id.note().metadata().tag(),
                        )),
                        new_pool_states: Some(new_pool_states),
                        expected_output_recipient: Some(p2id.note().recipient().clone()),
                        result: ExecutionResult::WithdrawSuccess(amount_out.to::<u64>()),
                    })
                } else {
                    Ok(ExecutionDetails {
                        result: if past_deadline {
                            ExecutionResult::PastDeadline
                        } else {
                            ExecutionResult::Failed
                        },
                        ..Default::default()
                    })
                }
            }
            NoteInstructions::Swap(instructions) => {
                let past_deadline = now > created_at + instructions.deadline as i64;
                let mut new_pool_states = pool_states.clone();
                let mut pool_state_base = *new_pool_states
                    .get_mut(&instructions.asset_in)
                    .ok_or(anyhow!("Trying to execute swap for an unknown asset."))?;
                let mut pool_state_quote = *new_pool_states
                    .get_mut(&instructions.asset_out)
                    .ok_or(anyhow!("Trying to execute swap for an unknown asset."))?;
                let base_price = prices
                    .get(&instructions.asset_in)
                    .ok_or(anyhow!("No price for asset {}", instructions.asset_in))?;
                let quote_price = prices
                    .get(&instructions.asset_out)
                    .ok_or(anyhow!("No price for asset {}", instructions.asset_out))?;
                let price = base_price.quote_with(quote_price.price);
                let (p2id, amount_out, result) = if !past_deadline
                    && let Ok((amount_out, new_base_pool_balances, new_quote_pool_balances)) =
                        get_curve_amount_out(
                            &pool_state_base,
                            &pool_state_quote,
                            U256::from(pool_state_base.metadata().asset_decimals),
                            U256::from(pool_state_quote.metadata().asset_decimals),
                            U256::from(instructions.amount_in),
                            price,
                        )
                    && amount_out >= instructions.min_amount_out
                {
                    let beneficiary = if let Some(beneficiary) = instructions.beneficiary {
                        beneficiary
                    } else {
                        instructions.creator
                    };
                    let p2id = TrustedNote::build_p2id(
                        beneficiary,
                        instructions.asset_out,
                        amount_out.to::<u64>(),
                        Some(note.serial_number()),
                    )?;
                    pool_state_base.update_balances(new_base_pool_balances);
                    pool_state_quote.update_balances(new_quote_pool_balances);
                    (
                        p2id,
                        amount_out.to::<u64>(),
                        ExecutionResult::SwapSuccess(amount_out.to::<u64>()),
                    )
                } else {
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.asset_in,
                        instructions.amount_in,
                        Some(note.serial_number()),
                    )?;
                    let result = if past_deadline {
                        ExecutionResult::PastDeadline
                    } else {
                        ExecutionResult::Failed
                    };
                    (p2id, instructions.amount_in, result)
                };

                let advice_map_value = vec![
                    Felt::new(pool_state_base.balances().total_liabilities.to::<u64>()),
                    Felt::new(pool_state_base.balances().reserve.to::<u64>()),
                    Felt::new(pool_state_base.balances().reserve_with_slippage.to::<u64>()),
                    Felt::new(amount_out),
                    Felt::new(pool_state_quote.balances().total_liabilities.to::<u64>()),
                    Felt::new(pool_state_quote.balances().reserve.to::<u64>()),
                    Felt::new(
                        pool_state_quote
                            .balances()
                            .reserve_with_slippage
                            .to::<u64>(),
                    ),
                    Felt::new(0),
                ];

                Ok(ExecutionDetails {
                    advice_map_value: Some((note.serial_number(), advice_map_value)),
                    input_note: Some((note.note().clone(), None)),
                    expected_future_note: Some((
                        p2id.note().clone().into(),
                        p2id.note().metadata().tag(),
                    )),
                    new_pool_states: Some(new_pool_states),
                    expected_output_recipient: Some(p2id.note().recipient().clone()),
                    result,
                })
            }
            NoteInstructions::P2ID(_) => Err(anyhow!("Cant execute a P2ID against zoro pool.")),
        }
    }
}

#[derive(Default)]
pub struct ExecutionDetails {
    advice_map_value: Option<(Word, Vec<Felt>)>,
    input_note: Option<(Note, Option<Word>)>,
    expected_future_note: Option<(NoteDetails, NoteTag)>,
    expected_output_recipient: Option<NoteRecipient>,
    new_pool_states: Option<HashMap<AccountId, PoolState>>,
    result: ExecutionResult,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ExecutionResult {
    SwapSuccess(u64),
    DepositSuccess(u64),
    WithdrawSuccess(u64),
    #[default]
    Failed,
    PastDeadline,
}
