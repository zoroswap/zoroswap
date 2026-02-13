use std::{env, fs};

use anyhow::{Result, anyhow};
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    Felt,
    account::{AccountBuilder, AccountStorageMode, AccountType},
    asset::TokenSymbol,
    auth::AuthSecretKey,
    keystore::FilesystemKeyStore,
    rpc::Endpoint,
    transaction::TransactionRequestBuilder,
};
use miden_standards::account::{auth::AuthFalcon512Rpo, faucets::BasicFungibleFaucet};
use rand::RngCore;
use serde::Deserialize;
use zoro_miden_client::instantiate_simple_client;

#[derive(Deserialize, Debug)]
struct FaucetConfig {
    symbol: String,
    max_supply: u64,
    decimals: u8,
}
#[derive(Deserialize, Debug)]
struct FaucetsConfig {
    pub faucets: Vec<FaucetConfig>,
}

#[derive(Parser, Debug)]
#[command(name = "spawn_faucets")]
#[command(about = "Spawn faucets for Zoro DEX", long_about = None)]
struct Args {
    /// Path to the faucets config file
    #[arg(short, long, default_value = "./faucets.toml")]
    faucets_config: String,

    /// Path to the keystore directory
    #[arg(short, long, default_value = "./keystore")]
    keystore_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter("info,zoro=debug")
        .init();

    dotenv().ok();

    let contents = fs::read_to_string(&args.faucets_config)
        .map_err(|e| anyhow!("Error opening {}: {e}", args.faucets_config))?;
    let miden_endpoint =
        env::var("MIDEN_NODE_ENDPOINT").expect("Missing MIDEN_NODE_ENDPOINT in .env file.");
    let parsed: FaucetsConfig = toml::from_str(&contents)?;
    let faucets = parsed.faucets;
    let miden_endpoint = match miden_endpoint.as_str() {
        "testnet" => Endpoint::testnet(),
        "devnet" => Endpoint::devnet(),
        _ => Endpoint::localhost(),
    };

    let mut client = instantiate_simple_client(&args.keystore_path, &miden_endpoint).await?;

    let sync_summary = client.sync_state().await?;
    println!("Latest block: {}", sync_summary.block_num);

    let keystore: FilesystemKeyStore =
        FilesystemKeyStore::new(args.keystore_path.into())
            .unwrap_or_else(|err| panic!("Failed to create keystore: {err:?}"));

    println!("\nDeploying a new fungible faucet.");

    // Generate key pair
    let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(client.rng());
    for faucet in faucets {
        // Faucet parameters
        let symbol = TokenSymbol::new(&faucet.symbol)
            .unwrap_or_else(|err| panic!("Failed to create token symbol: {err:?}"));
        let decimals = faucet.decimals;
        let max_supply = Felt::new(faucet.max_supply);

        // Faucet seed
        let mut init_seed = [0u8; 32];
        client.rng().fill_bytes(&mut init_seed);

        // Build the account
        let builder = AccountBuilder::new(init_seed)
            .account_type(AccountType::FungibleFaucet)
            .storage_mode(AccountStorageMode::Public)
            .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
            .with_component(
                BasicFungibleFaucet::new(symbol, decimals, max_supply)
                    .unwrap_or_else(|err| panic!("Failed to create BasicFungibleFaucet: {err:?}")),
            );

        let faucet_account = builder
            .build()
            .unwrap_or_else(|err| panic!("Failed to build faucet account: {err:?}"));

        // Add the faucet to the client
        client.add_account(&faucet_account, true).await?;

        // Add the key pair to the keystore
        keystore
            .add_key(&key_pair)
            .unwrap_or_else(|err| panic!("Failed to add key to keystore: {err:?}"));

        println!(
            "Faucet account ID ({}): {:?}",
            faucet.symbol,
            faucet_account
                .id()
                .to_bech32(miden_endpoint.to_network_id())
        );

        // Deploy faucet to node by submitting a transaction
        println!("Deploying faucet {}.", faucet.symbol);
        let transaction_request = TransactionRequestBuilder::new().build()?;
        let _tx_id = client
            .submit_new_transaction(faucet_account.id(), transaction_request)
            .await?;

        println!("Faucet {} successfully deployed.", faucet.symbol);

        // Sync state from chain to client
        client.sync_state().await?;
    }

    println!("All faucets deployed successfully.");

    Ok(())
}
