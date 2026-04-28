use anyhow::{Result, anyhow};
use std::{fs::read_to_string, path::PathBuf};

use miden_client::assembly::CodeBuilder;

pub fn link_all_libraries(mut code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let asset_utils_code = read_masm_file(&["lib", "asset_utils.masm"])?;
    code_builder.link_module("zoro_miden::lib::asset_utils", &asset_utils_code)?;

    let pool_code = read_masm_file(&["accounts", "zoropool.masm"])?;

    code_builder.link_module("zoroswap::zoropool", &pool_code)?;
    Ok(code_builder)
}

pub fn read_masm_file(path_steps: &[&str]) -> Result<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from_iter(
        [manifest_dir, "masm"]
            .into_iter()
            .chain(path_steps.iter().copied()),
    );
    read_to_string(&path).map_err(|e| anyhow!("Error reading MASM file at path {path:?}: {e:?}"))
}
