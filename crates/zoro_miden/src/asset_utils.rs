use std::ops::Add;

use anyhow::Result;
use miden_client::{Felt, Word, asset::FungibleAsset};
use miden_protocol::asset::AssetCallbackFlag;

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
    if word[2].as_canonical_u64().ne(&0) {
        Ok(asset.with_callbacks(AssetCallbackFlag::Enabled))
    } else {
        Ok(asset)
    }
}

#[cfg(test)]
mod tests {
    use miden_client::account::AccountId;
    use miden_protocol::asset::AssetCallbackFlag;

    use super::*;

    #[test]
    fn parse_asset_to_word() -> Result<()> {
        let (_, faucet_id) = AccountId::from_bech32("mtst1aqv4yfn4fef8zgzfew6jeanydy8qdz0d")?;
        let asset = FungibleAsset::new(faucet_id, 10000)?;
        let word = asset_to_word(asset);
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
        let (_, faucet_id_parsed) =
            AccountId::from_bech32("mtst1aqv4yfn4fef8zgzfew6jeanydy8qdz0d")?;
        let prefix = faucet_id_parsed.prefix().as_u64();
        let suffix = faucet_id_parsed.suffix().as_canonical_u64();
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
        assert_eq!(asset.faucet_id(), faucet_id_parsed);
        Ok(())
    }

    #[test]
    fn parse_asset_with_callback_to_word() -> Result<()> {
        let (_, faucet_id) = AccountId::from_bech32("mtst1azsvnn4chyfkvgp8a46whpuhlsy50vns")?;
        let asset =
            FungibleAsset::new(faucet_id, 10000)?.with_callbacks(AssetCallbackFlag::Enabled);
        let word = asset_to_word(asset);
        assert_eq!(
            word[0].as_canonical_u64(),
            asset.faucet_id().prefix().as_u64()
        );
        assert_eq!(
            word[1].as_canonical_u64(),
            asset.faucet_id().suffix().as_canonical_u64()
        );
        assert_eq!(1, asset.callbacks().as_u8());
        assert_eq!(word[3].as_canonical_u64(), asset.amount());
        Ok(())
    }

    #[test]
    fn parse_word_to_asset_with_callback() -> Result<()> {
        let (_, faucet_id_parsed) =
            AccountId::from_bech32("mtst1azsvnn4chyfkvgp8a46whpuhlsy50vns")?;
        let prefix = faucet_id_parsed.prefix().as_u64();
        let suffix = faucet_id_parsed.suffix().as_canonical_u64();
        let amount = 10000;
        let word: Word = [
            Felt::new(prefix),
            Felt::new(suffix),
            Felt::new(1),
            Felt::new(amount),
        ]
        .into();
        let asset = word_to_asset(word)?;
        assert_eq!(
            asset.faucet_id().prefix().as_felt().as_canonical_u64(),
            prefix
        );
        assert_eq!(asset.faucet_id().suffix().as_canonical_u64(), suffix);
        assert_eq!(asset.callbacks().as_u8(), 1);
        assert_eq!(asset.amount(), amount);
        assert_eq!(asset.faucet_id(), faucet_id_parsed);
        Ok(())
    }
}
