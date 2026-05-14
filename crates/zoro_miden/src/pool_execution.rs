use std::collections::HashMap;

use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::{
    Felt, Word,
    account::AccountId,
    address::NetworkId,
    note::{Note, NoteDetails, NoteRecipient, NoteTag},
};
use tracing::info;

use crate::{
    curve::get_curve_amount_out,
    note::{NoteInstructions, TrustedNote},
    pool_state::PoolState,
    price::PriceData,
};

#[derive(Default, Clone)]
pub struct PoolExecution {
    pub advice_map_value: Option<(Word, Vec<Felt>)>,
    pub input_note: Option<(Note, Option<Word>)>,
    pub expected_future_note: Option<(NoteDetails, NoteTag)>,
    pub expected_output_recipient: Option<NoteRecipient>,
    pub new_pool_states: Option<HashMap<AccountId, PoolState>>,
    pub counterparty_account: Option<AccountId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum ExecutionResult {
    SwapSuccess(u64),
    DepositSuccess(u64),
    WithdrawSuccess(u64),
    #[default]
    Failed,
    PastDeadline,
    FailedConsuming,
}

impl PoolExecution {
    pub fn new(
        note: TrustedNote,
        pool_states: &HashMap<AccountId, PoolState>,
        prices: &HashMap<AccountId, PriceData>,
    ) -> Result<(ExecutionResult, Self)> {
        let note_instructions = NoteInstructions::try_from(note.clone())?;
        let now = Utc::now().timestamp_millis();
        let mut new_pool_states = pool_states.clone();
        match note_instructions {
            NoteInstructions::Deposit(instructions) => {
                let past_deadline = now > instructions.deadline as i64;
                let mut pool_state = *pool_states
                    .get(&instructions.asset_in.faucet_id())
                    .ok_or(anyhow!("Trying to execute deposit for an unknown asset."))?;
                if !past_deadline
                    && let Ok((lp_amount, new_lp_total_supply, new_pool_balances)) = pool_state
                        .get_deposit_lp_amount_out(U256::from(instructions.asset_in.amount()))
                {
                    pool_state.update_state(new_pool_balances, new_lp_total_supply);
                    new_pool_states.insert(instructions.asset_in.faucet_id(), pool_state);
                    Ok((
                        ExecutionResult::DepositSuccess(lp_amount.to::<u64>()),
                        PoolExecution {
                            advice_map_value: None,
                            input_note: Some((
                                note.note().clone(),
                                Some(pool_state.to_lp_note_args(instructions.asset_in.amount())), // amount 0 will reject the deposit in masm
                            )),
                            expected_future_note: None,
                            new_pool_states: Some(new_pool_states),
                            expected_output_recipient: None,
                            counterparty_account: Some(instructions.creator),
                        },
                    ))
                } else {
                    // Return the asset back to the creator of this failed deposit
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.asset_in,
                        Some(note.serial_number()),
                    )?;
                    Ok((
                        if past_deadline {
                            ExecutionResult::PastDeadline
                        } else {
                            ExecutionResult::Failed
                        },
                        PoolExecution {
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
                            counterparty_account: Some(instructions.creator),
                        },
                    ))
                }
            }
            NoteInstructions::Withdraw(instructions) => {
                let past_deadline = now > instructions.deadline as i64;
                let mut pool_state = *pool_states
                    .get(&instructions.min_asset_out.faucet_id())
                    .ok_or(anyhow!(
                        "Trying to execute withdrawal for an unknown asset."
                    ))?;
                if !past_deadline
                    && let Ok((amount_out, new_lp_total_supply, new_pool_balances)) = pool_state
                        .get_withdraw_asset_amount_out(U256::from(instructions.lp_amount_in))
                    && amount_out >= instructions.min_asset_out.amount()
                {
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.min_asset_out,
                        Some(note.serial_number()),
                    )?;
                    pool_state.update_state(new_pool_balances, new_lp_total_supply);
                    new_pool_states.insert(instructions.min_asset_out.faucet_id(), pool_state);
                    Ok((
                        ExecutionResult::WithdrawSuccess(amount_out.to::<u64>()),
                        PoolExecution {
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
                            counterparty_account: Some(instructions.creator),
                        },
                    ))
                } else {
                    Ok((
                        if past_deadline {
                            ExecutionResult::PastDeadline
                        } else {
                            ExecutionResult::Failed
                        },
                        PoolExecution::default(),
                    ))
                }
            }
            NoteInstructions::Swap(instructions) => {
                let past_deadline = now > instructions.deadline as i64;
                let mut pool_state_base = *new_pool_states
                    .get_mut(&instructions.asset_in.faucet_id())
                    .ok_or(anyhow!("Trying to execute swap for an unknown asset."))?;
                let mut pool_state_quote = *new_pool_states
                    .get_mut(&instructions.min_asset_out.faucet_id())
                    .ok_or(anyhow!("Trying to execute swap for an unknown asset."))?;
                let base_price = prices
                    .get(&instructions.asset_in.faucet_id())
                    .ok_or(anyhow!("No price for asset {}", instructions.asset_in))?;
                let quote_price = prices
                    .get(&instructions.min_asset_out.faucet_id())
                    .ok_or(anyhow!("No price for asset {}", instructions.min_asset_out))?;
                let price = base_price.quote_with(quote_price.price);
                let (p2id, amount_out, result, counterparty_account) = if !past_deadline
                    && let Ok((amount_out, new_base_pool_balances, new_quote_pool_balances)) =
                        get_curve_amount_out(
                            &pool_state_base,
                            &pool_state_quote,
                            U256::from(pool_state_base.metadata().asset_decimals),
                            U256::from(pool_state_quote.metadata().asset_decimals),
                            U256::from(instructions.asset_in.amount()),
                            price,
                        )
                    && amount_out >= instructions.min_asset_out.amount()
                {
                    let beneficiary = if let Some(beneficiary) = instructions.beneficiary {
                        beneficiary
                    } else {
                        instructions.creator
                    };
                    let p2id = TrustedNote::build_p2id(
                        beneficiary,
                        instructions.asset_in,
                        Some(note.serial_number()),
                    )?;

                    pool_state_base.update_balances(new_base_pool_balances);
                    pool_state_quote.update_balances(new_quote_pool_balances);
                    new_pool_states.insert(instructions.asset_in.faucet_id(), pool_state_base);
                    new_pool_states
                        .insert(instructions.min_asset_out.faucet_id(), pool_state_quote);
                    (
                        p2id,
                        amount_out.to::<u64>(),
                        ExecutionResult::SwapSuccess(amount_out.to::<u64>()),
                        Some(beneficiary),
                    )
                } else {
                    let p2id = TrustedNote::build_p2id(
                        instructions.creator,
                        instructions.asset_in,
                        Some(note.serial_number()),
                    )?;
                    let result = if past_deadline {
                        ExecutionResult::PastDeadline
                    } else {
                        ExecutionResult::Failed
                    };
                    (
                        p2id,
                        0, //instructions.asset_in.amount(),
                        result,
                        Some(instructions.creator),
                    )
                };

                info!(" swap execution result: {:?}", result);

                info!(
                    "P2ID: serial {:?} recipient {:?}",
                    p2id.note().serial_num(),
                    p2id.note().recipient().digest()
                );

                let advice_map_value = vec![
                    Felt::new(
                        pool_state_base
                            .balances()
                            .total_liabilities
                            .saturating_to::<u64>(),
                    ),
                    Felt::new(pool_state_base.balances().reserve.saturating_to::<u64>()),
                    Felt::new(
                        pool_state_base
                            .balances()
                            .reserve_with_slippage
                            .saturating_to::<u64>(),
                    ),
                    Felt::new(amount_out),
                    Felt::new(
                        pool_state_quote
                            .balances()
                            .total_liabilities
                            .saturating_to::<u64>(),
                    ),
                    Felt::new(pool_state_quote.balances().reserve.saturating_to::<u64>()),
                    Felt::new(
                        pool_state_quote
                            .balances()
                            .reserve_with_slippage
                            .saturating_to::<u64>(),
                    ),
                    Felt::new(0),
                ];

                Ok((
                    result,
                    PoolExecution {
                        advice_map_value: Some((note.serial_number(), advice_map_value)),
                        input_note: Some((note.note().clone(), None)),
                        expected_future_note: Some((
                            p2id.note().clone().into(),
                            p2id.note().metadata().tag(),
                        )),
                        new_pool_states: Some(new_pool_states),
                        expected_output_recipient: Some(p2id.note().recipient().clone()),
                        counterparty_account,
                    },
                ))
            }
            NoteInstructions::P2ID(_) => Ok((ExecutionResult::Failed, PoolExecution::default())),
        }
    }

    pub fn print_execution_details(&self, network_id: NetworkId, result: &ExecutionResult) {
        let acc = if let Some(acc) = self.counterparty_account {
            acc.to_bech32(network_id)
        } else {
            "none".to_string()
        };
        let input_note_id = if let Some(input_note) = &self.input_note {
            input_note.0.id().to_string()
        } else {
            "none".to_string()
        };
        info!(
            account = acc,
            result = ?result,
            note_id = input_note_id,
            "Execution details"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #[test]
    // fn test_min_out_fail() {
    //     todo!("Missing test")
    // }

    // #[test]
    // fn over_coverage_ratio() {
    //     todo!("Missing test")
    // }

    // #[test]
    // fn over_coverage_ratio() {
    //     todo!("Missing test")
    // }
}
