use anyhow::Result;
use zoro_miden::test_utils::TestUtils;

#[tokio::main]
async fn main() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    test_utils.add_cached_accounts(1).await?;
    test_utils.add_cached_pools(1).await?;
    Ok(())
}
