use miden_client::{
    ClientError, Felt,
    account::{
        Account, AccountBuilder, AccountStorageMode, AccountType, component::BasicFungibleFaucet,
    },
    asset::TokenSymbol,
    auth::{AuthFalcon512Rpo, AuthSecretKey},
    keystore::FilesystemKeyStore,
};
use rand::RngCore;

use crate::client::MidenClient;

/// Creates a basic fungible faucet account.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore to store the faucet's authentication key
///
/// # Returns
/// The created faucet `Account`
pub async fn create_basic_faucet(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
) -> Result<Account, ClientError> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(client.rng());
    let symbol = TokenSymbol::new("MID")
        .unwrap_or_else(|err| panic!("Failed to create token symbol: {err:?}"));
    let decimals = 8;
    let max_supply = Felt::new(1_000_000_000);
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
        .with_component(BasicFungibleFaucet::new(symbol, decimals, max_supply).unwrap());
    let account = builder.build().unwrap();
    client.add_account(&account, false).await?;
    keystore.add_key(&key_pair).unwrap();
    Ok(account)
}
