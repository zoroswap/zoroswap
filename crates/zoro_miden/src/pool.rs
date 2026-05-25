use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr,
    time::{Duration, Instant},
};

use alloy::primitives::{I256, U256};
use anyhow::{Result, anyhow};
use miden_client::{
    Felt, Word,
    account::{
        Account, AccountBuilder, AccountComponent, AccountId, AccountStorageMode, AccountType,
        StorageMap, StorageMapKey, StorageSlot, StorageSlotName,
        component::{AccountComponentMetadata, BasicWallet},
    },
    asset::AssetVault,
    auth::{AuthScheme, AuthSecretKey, AuthSingleSig},
    keystore::{FilesystemKeyStore, Keystore},
    note::{Note, NoteDetails, NoteId, NoteRecipient, NoteTag},
    rpc::Endpoint,
    transaction::{TransactionRequest, TransactionRequestBuilder},
    vm::AdviceMap,
};
use rand::RngCore;
use tokio::time::sleep;
use tracing::{error, info};

use crate::{
    account::MidenAccount,
    assembly_utils::{
        link_asset_utils, link_note_common_lib, link_output_note_utils_lib, link_storage_utils,
        read_masm_file,
    },
    client::MidenClient,
    note::TrustedNote,
    pool_execution::{ExecutionResult, PoolExecution},
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
    endpoint: Endpoint,
}

#[derive(Default)]
struct BatchExecutionDetails {
    advice_map: AdviceMap,
    input_notes: Vec<(Note, Option<Word>)>,
    expected_future_notes: Vec<(NoteDetails, NoteTag)>,
    expected_output_recipients: Vec<NoteRecipient>,
    new_pool_states: HashMap<AccountId, PoolState>,
}

impl TryFrom<BatchExecutionDetails> for TransactionRequest {
    type Error = anyhow::Error;
    fn try_from(value: BatchExecutionDetails) -> std::result::Result<Self, Self::Error> {
        let consume_req = TransactionRequestBuilder::new()
            .extend_advice_map(value.advice_map)
            .input_notes(value.input_notes)
            .expected_future_notes(value.expected_future_notes)
            .expected_output_recipients(value.expected_output_recipients)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build batch transaction request: {}", e))?;
        Ok(consume_req)
    }
}

impl ZoroPool {
    pub fn pool_states(&self) -> &HashMap<AccountId, PoolState> {
        &self.pool_states
    }

    pub async fn new_from_existing_pool(
        endpoint: Endpoint,
        keystore_dir: &str,
        store_dir: &str,
        pool_account_id: &AccountId,
        liquidity_pools: Vec<LiquidityPoolConfig>,
    ) -> Result<Self> {
        info!(
            "Creating new ZORO pool from account: {} with tag {}",
            pool_account_id.to_bech32(endpoint.to_network_id()),
            NoteTag::with_account_target(*pool_account_id)
        );
        let miden_account = MidenAccount::new(*pool_account_id, None);
        let miden_client = MidenClient::new(endpoint.clone(), keystore_dir, store_dir).await?;
        let mut zoro_pool = Self {
            miden_client,
            miden_account,
            pool_states: HashMap::with_capacity(liquidity_pools.len()),
            liquidity_pools,
            endpoint,
        };
        zoro_pool.update_pool_state_from_chain().await?;
        zoro_pool.print_pool_states();
        Ok(zoro_pool)
    }

    pub async fn new_deployment(
        liquidity_pools: Vec<LiquidityPoolConfig>,
        endpoint: Endpoint,
        keystore_dir: &str,
        store_dir: &str,
    ) -> Result<Self> {
        let mut miden_client: MidenClient =
            MidenClient::new(endpoint.clone(), keystore_dir, store_dir).await?;

        let pool_code = read_masm_file(&["accounts", "zoropool.masm"])
            .map_err(|e| anyhow!("Failed to read zoropool.masm: {e:?}"))?;

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
                Felt::new(0),
            ]
            .into();
            let curve: Word = [
                Felt::new(pool_settings.beta.to_string().parse::<u64>()?),
                Felt::new(pool_settings.c.to_string().parse::<u64>()?),
                Felt::new(0),
                Felt::new(0),
            ]
            .into();
            let asset_index = StorageMapKey::new(Word::from([
                Felt::new(i as u64),
                Felt::new(0),
                Felt::new(0),
                Felt::new(0),
            ]));
            let asset_id = StorageMapKey::new(Word::from([
                pool.faucet_id.suffix(),
                pool.faucet_id.prefix().as_felt(),
                Felt::new(0),
                Felt::new(0),
            ]));
            assets_mapping
                .insert(asset_index, Word::from(asset_id))
                .unwrap_or_else(|err| panic!("Failed to insert asset into mapping: {err:?}"));
            fees_mapping
                .insert(asset_id, fees)
                .unwrap_or_else(|err| panic!("Failed to insert fees into mapping: {err:?}"));
            curves_mapping
                .insert(asset_id, curve)
                .unwrap_or_else(|err| panic!("Failed to insert curve into mapping: {err:?}"));
        }

        let n = |name: &str| StorageSlotName::new(name).expect("valid slot name");

        let fees_mapping = StorageSlot::with_map(n("zoroswap::fees"), fees_mapping);
        let pool_states_mapping = StorageSlot::with_empty_map(n("zoroswap::pool_state"));
        let user_deposits_mapping = StorageSlot::with_empty_map(n("zoroswap::user_deposits"));

        let code_builder = link_asset_utils(miden_client.client_mut().code_builder())?;
        let code_builder = link_storage_utils(code_builder)?;
        let code_builder = link_note_common_lib(code_builder)?;
        let code_builder = link_output_note_utils_lib(code_builder)?;
        let pool_library = code_builder.compile_component_code("zoroswap::zoropool", &pool_code)?;
        let pool_metadata = AccountComponentMetadata::new("zoroswap::zoropool", AccountType::all());

        let pool_component = AccountComponent::new(
            //(*pool_library).clone(),
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
            pool_metadata,
        )?;
        // Init seed for the pool contract
        let mut init_seed = [0_u8; 32];
        miden_client.client_mut().rng().fill_bytes(&mut init_seed);

        let key_pair =
            AuthSecretKey::new_falcon512_poseidon2_with_rng(miden_client.client_mut().rng());

        // Build the new `Account` with the component
        let pool_contract = AccountBuilder::new(init_seed)
            .account_type(AccountType::RegularAccountUpdatableCode)
            .storage_mode(AccountStorageMode::Public)
            .with_component(pool_component.clone())
            .with_auth_component(AuthSingleSig::new(
                key_pair.public_key().to_commitment(),
                AuthScheme::Falcon512Poseidon2,
            ))
            .with_component(BasicWallet)
            .build()?;

        println!(
            "pool contract commitment hash: {:?}",
            pool_contract.to_commitment().to_hex()
        );
        println!(
            "contract id: {:?}",
            pool_contract.id().to_bech32(endpoint.to_network_id())
        );

        let keystore = FilesystemKeyStore::new(miden_client.keystore_path())?;
        keystore
            .add_key(&key_pair, pool_contract.id())
            .await
            .map_err(|e| anyhow!("Failed to add key: {e:?}"))?;
        miden_client
            .client_mut()
            .add_account(&pool_contract.clone(), false)
            .await?;
        miden_client.sync_state().await?;
        let init_tx = TransactionRequestBuilder::new().build()?;
        miden_client
            .client_mut()
            .submit_new_transaction(pool_contract.id(), init_tx)
            .await?;
        miden_client.sync_state().await?;
        println!("Waiting for acc to be included in block ...",);
        sleep(Duration::from_secs(5)).await;

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
            endpoint,
        })
    }

    pub async fn update_pool_state_from_chain(&mut self) -> Result<()> {
        let id = *self.miden_account.id();
        self.miden_client
            .import_account(&id)
            .await
            .map_err(|e| anyhow!("Error on account update from client: {e}"))?;
        let acc = self
            .miden_client
            .client_mut()
            .get_account(id)
            .await
            .map_err(|e| anyhow!("Account {id} not found in client: {e:?}"))?
            .ok_or(anyhow!("Account {id} not found in client"))?;
        self.miden_account.set_account(acc.clone());
        for pool in self.liquidity_pools.iter() {
            let (settings, balances, lp_total_supply) =
                Self::extract_liqudity_pool_state_from_account(&acc, pool.faucet_id).await?;
            println!("pool {settings:?}, {balances:?}, {lp_total_supply:?}");
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
            faucet_id.suffix(),
            faucet_id.prefix().as_felt(),
            Felt::new(0),
            Felt::new(0),
        ]
        .into();

        let pool_balances_raw = storage.get_map_item(&balances_slot, asset_address)?;
        let pool_curve = storage.get_map_item(&curve_slot, asset_address)?;
        let pool_fees = storage.get_map_item(&fees_slot, asset_address)?;

        let balances = PoolBalances {
            total_liabilities: U256::from(pool_balances_raw[0].as_canonical_u64()),
            reserve: U256::from(pool_balances_raw[1].as_canonical_u64()),
            reserve_with_slippage: U256::from(pool_balances_raw[2].as_canonical_u64()),
        };
        let settings = PoolSettings {
            beta: I256::from_str(&pool_curve[0].as_canonical_u64().to_string())?,
            c: I256::from_str(&pool_curve[1].as_canonical_u64().to_string())?,
            swap_fee: U256::from(pool_fees[0].as_canonical_u64()),
            backstop_fee: U256::from(pool_fees[1].as_canonical_u64()),
            protocol_fee: U256::from(pool_fees[2].as_canonical_u64()),
        };
        let total_supply_raw = storage.get_map_item(&lp_supply_slot, asset_address)?;
        let lp_total_supply = total_supply_raw[0].as_canonical_u64();

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
    ) -> Result<HashMap<NoteId, ExecutionResult>> {
        info!("Executing {} notes on the zoro pool", notes.len());
        if notes.is_empty() {
            return Ok(HashMap::default());
        }
        self.miden_client.sync_state().await?;
        let (note_execution_results, batch_execution_details) =
            self.prepare_execution_batch(notes, prices).await?;
        if batch_execution_details.input_notes.is_empty() {
            // no notes are eligible for execution
            return Ok(note_execution_results);
        }
        let start = Instant::now();
        let (len_input_notes, len_advice_map, len_future_notes, len_output_recipients) = (
            batch_execution_details.input_notes.len(),
            batch_execution_details.advice_map.len(),
            batch_execution_details.expected_future_notes.len(),
            batch_execution_details.expected_output_recipients.len(),
        );
        let new_pool_states = batch_execution_details.new_pool_states.clone();
        let tx_request: TransactionRequest = batch_execution_details.try_into()?;
        let tx_result = self
            .miden_client
            .client_mut()
            .execute_transaction(*self.miden_account.id(), tx_request.clone())
            .await?;
        info!("Tx result: {:?}", tx_result.account_delta().storage());

        let tx_id = self
            .miden_client
            .client_mut()
            .submit_new_transaction(*self.miden_account.id(), tx_request)
            .await
            .inspect_err(|e| {
                error!(
                    // error = ?e,
                    error = e.to_string(),
                    pool_id = %self.miden_account.id().to_hex(),
                    input_notes = len_input_notes,
                    advice_map = len_advice_map,
                    expected_future_notes = len_future_notes,
                    expected_output_recipients = len_output_recipients,
                    "Failed to submit batch transaction",
                );
            })?;
        info!(len_notes = len_input_notes, time_elapsed= ?start.elapsed(), "Executed notes");
        MidenClient::print_transaction_info(&tx_id);
        self.miden_client.sync_state().await?;
        self.pool_states = new_pool_states;
        self.print_pool_states();
        Ok(note_execution_results)
    }

    async fn prepare_execution_details(
        &mut self,
        notes: Vec<TrustedNote>,
        prices: HashMap<AccountId, PriceData>,
    ) -> Result<(HashMap<NoteId, ExecutionResult>, BatchExecutionDetails)> {
        info!("Preparing execution details for {} notes", notes.len());
        let mut advice_map = AdviceMap::default();
        let mut input_notes = Vec::with_capacity(notes.len());
        let mut expected_future_notes = Vec::with_capacity(notes.len());
        let mut expected_output_recipients = Vec::with_capacity(notes.len());
        let mut pool_states = self.pool_states.clone();
        let mut note_execution_results: HashMap<NoteId, ExecutionResult> =
            HashMap::with_capacity(notes.len());
        for note in notes {
            let note_id = note.note().id();
            let (execution_result, execution_details) =
                PoolExecution::new(note, &pool_states, &prices)?;
            execution_details
                .print_execution_details(self.endpoint.to_network_id(), &execution_result);
            let PoolExecution {
                advice_map_value,
                input_note,
                expected_future_note,
                expected_output_recipient,
                new_pool_states,
                counterparty_account,
            } = execution_details;

            // Import the account if unknown
            if let Some(counterparty_account) = counterparty_account
                && let Ok(acc_query) = self
                    .miden_client
                    .client()
                    .get_account(counterparty_account)
                    .await
                && acc_query.is_none()
            {
                self.miden_client
                    .import_account(&counterparty_account)
                    .await?;
            }

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

            note_execution_results.insert(note_id, execution_result);
        }
        Ok((
            note_execution_results,
            BatchExecutionDetails {
                advice_map,
                input_notes,
                expected_future_notes,
                expected_output_recipients,
                new_pool_states: pool_states,
            },
        ))
    }

    async fn prepare_execution_batch(
        &mut self,
        notes: Vec<TrustedNote>,
        prices: HashMap<AccountId, PriceData>,
    ) -> Result<(HashMap<NoteId, ExecutionResult>, BatchExecutionDetails)> {
        info!("Preparing batch execution for {} notes", notes.len());
        let mut note_results = HashMap::with_capacity(notes.len());
        let mut valid_notes = notes;
        // simulate until all notes go thru
        // must do it this way because of the sequential nature of updates to the account and pool states
        // loop {
        if valid_notes.is_empty() {
            return Ok((note_results, BatchExecutionDetails::default()));
        }

        // TODO: Are prices still fresh here?
        let (note_execution_details, batch_execution_details) = self
            .prepare_execution_details(valid_notes.clone(), prices.clone())
            .await?;
        note_results.extend(note_execution_details);
        // let note_screener = self.miden_client.client().note_screener();

        let mut note_args = BTreeMap::new();
        for (note, args) in &batch_execution_details.input_notes {
            if let Some(args) = args {
                note_args.insert(note.id(), *args);
            }
        }
        Ok((note_results, batch_execution_details))

        // let tx_args = TransactionArgs::new(batch_execution_details.advice_map.clone())
        //     .with_note_args(note_args);
        // let note_screener = note_screener.with_transaction_args(tx_args);
        // let raw_notes: Vec<Note> = valid_notes.iter().map(|n| n.note().clone()).collect();
        // let NoteConsumptionInfo { successful, failed } = note_screener
        //     .check_notes_consumability(*self.miden_account.id(), raw_notes)
        //     .await?;
        // if !failed.is_empty() {
        //     for n in &failed {
        //         note_results.insert(n.note.id(), ExecutionResult::FailedConsuming);
        //     }
        //     let failed_ids = failed.iter().map(|n| n.note.id());
        //     let ids_string = failed_ids
        //         .clone()
        //         .map(|id| id.to_hex())
        //         .collect::<Vec<String>>()
        //         .join(", ");
        //     valid_notes.retain(|n| successful.contains(n.note()));
        //     warn!(
        //         "{} notes cant be consumed. Failed note ids: {}",
        //         failed_ids.len(),
        //         ids_string
        //     );
        // } else {
        //     return Ok((note_results, batch_execution_details));
        // }
        // }
    }
}

#[cfg(test)]
mod tests {
    use miden_client::{
        asset::FungibleAsset, testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
    };

    use crate::test_utils::{PoolWithMeta, TestUtils};

    use super::*;

    #[tokio::test]
    async fn deploying_new_pool() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        test_utils.add_cached_pools(1).await?;
        Ok(())
    }

    #[tokio::test]
    async fn create_from_existing_pool() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let pool = &test_utils.get_pools(1).await?[..][0];
        let client = test_utils.miden_client();
        ZoroPool::new_from_existing_pool(
            test_utils.miden_endpoint(),
            &client.keystore_dir(),
            &client.store_dir(),
            pool.miden_account.id(),
            pool.pool_configs.clone(),
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn execute_empty() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let PoolWithMeta {
            zoro_pool,
            test_pool: _,
        } = &mut test_utils.get_initialized_pools(1).await?[..][0];
        zoro_pool.execute_notes(vec![], HashMap::default()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn execute_p2id() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let PoolWithMeta {
            zoro_pool,
            test_pool,
        } = &mut test_utils.get_initialized_pools(1).await?[..][0];
        let p2id = TrustedNote::build_p2id(
            *test_pool.miden_account.id(),
            FungibleAsset::new(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into()?, 1000)?,
            None,
        )?;
        let p2id_id = p2id.note().id();
        let res = zoro_pool
            .execute_notes(vec![p2id], HashMap::default())
            .await?;
        let res_for_note = res.get(&p2id_id).unwrap();
        assert_eq!(res_for_note, &ExecutionResult::FailedConsuming);
        Ok(())
    }
}
