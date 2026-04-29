use std::ops::Add;

use anyhow::Result;
use miden_client::{Felt, Word, asset::FungibleAsset};

pub fn asset_to_word(asset: FungibleAsset) -> Word {
    let faucet_id = asset.faucet_id();
    let value = asset.to_value_word();
    let callbacks = asset.callbacks();
    [
        faucet_id.prefix().as_felt(),
        Felt::new(faucet_id.suffix().as_canonical_u64()),
        callbacks.as_u8().into(),
        value[0],
    ]
    .into()
}

pub fn word_to_asset(word: Word) -> Result<FungibleAsset> {
    let asset = FungibleAsset::from_key_value_words(
        [Felt::ZERO, Felt::ZERO, word[1].add(word[2]), word[0]].into(),
        [word[3], Felt::ZERO, Felt::ZERO, Felt::ZERO].into(),
    )?;
    Ok(asset)
}

#[cfg(test)]
mod tests {
    use miden_client::account::AccountId;

    use super::*;

    #[test]
    fn parse_asset_to_word() -> Result<()> {
        let (_, faucet_id) = AccountId::from_bech32("mtst1aqv4yfn4fef8zgzfew6jeanydy8qdz0d")?;
        let asset = FungibleAsset::new(faucet_id, 10000)?;
        let word = asset_to_word(asset);
        println!("word {:?}", word);
        assert_eq!(
            word[0].as_canonical_u64(),
            asset.faucet_id().prefix().as_u64()
        );
        assert_eq!(
            word[1].as_canonical_u64(),
            asset.faucet_id().suffix().as_canonical_u64()
        );
        assert_eq!(word[2].as_canonical_u64() as u8, asset.callbacks().as_u8());
        assert_eq!(word[3].as_canonical_u64(), asset.amount());
        Ok(())
    }

    #[test]
    fn parse_word_to_asset() -> Result<()> {
        let prefix = 1824563084305527072;
        let suffix = 5317542989757966592;
        let amount = 10000;
        let word: Word = [
            Felt::new(prefix),
            Felt::new(suffix),
            Felt::ZERO,
            Felt::new(amount),
        ]
        .into();
        let asset = word_to_asset(word)?;
        assert_eq!(
            asset.faucet_id().prefix().as_felt().as_canonical_u64(),
            prefix
        );
        assert_eq!(asset.faucet_id().suffix().as_canonical_u64(), suffix);
        assert_eq!(asset.callbacks().as_u8(), 0);
        assert_eq!(asset.amount(), amount);
        Ok(())
    }
}
