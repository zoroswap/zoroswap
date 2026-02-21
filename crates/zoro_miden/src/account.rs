use anyhow::{Result, anyhow};
use miden_client::{
    ClientError,
    account::{
        Account, AccountBuilder, AccountId, AccountStorageMode, AccountType, component::BasicWallet,
    },
    auth::{AuthFalcon512Rpo, AuthSecretKey},
    keystore::FilesystemKeyStore,
    store::AccountRecordData,
};
use rand::RngCore;

use crate::client::MidenClient;

pub struct MidenAccount {
    id: AccountId,
}

impl MidenAccount {
    pub fn id(&self) -> &AccountId {
        &self.id
    }

    pub async fn full(&self, client: &mut MidenClient) -> Result<AccountId> {
        let acc = client.get_account(*self.id()).await?.ok_or(anyhow!(
            "Account {} not found in local store",
            self.id().to_hex()
        ))?;
        let acc = match acc.account_data() {
            AccountRecordData::Full(a) => Ok(a),
            _ => Err(anyhow!(
                "Expected full account data for {}",
                self.id().to_hex()
            )),
        }?;
        Ok(acc.id())
    }
}

/// Creates a basic regular account with updatable code.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore to store the account's authentication key
///
/// # Returns
/// Tuple of `(Account, AuthSecretKey)`
pub async fn create_basic_account(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(client.rng());
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
        .with_component(BasicWallet);
    let account = builder.build().unwrap();
    client.add_account(&account, false).await?;
    keystore.add_key(&key_pair).unwrap();
    client.sync_state().await?;
    Ok((account, key_pair))
}
