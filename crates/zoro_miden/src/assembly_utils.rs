use anyhow::{Result, anyhow};
use std::{fs::read_to_string, path::PathBuf, sync::Arc};

use miden_assembly::{
    Assembler, DefaultSourceManager, Library,
    ast::{Module, ModuleKind},
};
use miden_client::assembly::CodeBuilder;

/// Universal functions
pub fn read_masm_file(path_steps: &[&str]) -> Result<String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let path = PathBuf::from_iter(
        [manifest_dir, "masm"]
            .into_iter()
            .chain(path_steps.iter().copied()),
    );
    read_to_string(&path).map_err(|e| anyhow!("Error reading MASM file at path {path:?}: {e:?}"))
}

/// Creates a Miden assembly library from source code.
///
/// # Arguments
/// * `assembler`: The assembler instance to use
/// * `library_path`: Path identifier for the library
/// * `source_code`: MASM source code
pub fn create_library_with_assembler(
    assembler: Assembler,
    library_path: &str,
    source_code: &str,
) -> Result<Arc<Library>> {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let path = miden_assembly::Path::new(library_path);
    let module = Module::parser(ModuleKind::Library)
        .parse_str(path, source_code, source_manager)
        .map_err(|e| anyhow!("Error assembling module: {e:?}"))?;
    let library = assembler
        .clone()
        .assemble_library([module])
        .map_err(|e| anyhow!("Error assembling library: {e:?}"))?;
    Ok(library)
}

/// Zoro specific functions

pub fn link_math(mut code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let math_code = read_masm_file(&["lib", "math.masm"])?;
    code_builder.link_module("zoro_miden::lib::math", &math_code)?;
    Ok(code_builder)
}

pub fn link_storage_utils(code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let mut code_builder = link_math(code_builder)?;
    let storage_utils_code = read_masm_file(&["lib", "storage_utils.masm"])?;
    code_builder.link_module("zoro_miden::lib::storage_utils", &storage_utils_code)?;
    Ok(code_builder)
}

pub fn link_asset_utils(mut code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let asset_utils_code = read_masm_file(&["lib", "asset_utils.masm"])?;
    code_builder.link_module("zoro_miden::lib::asset_utils", &asset_utils_code)?;
    Ok(code_builder)
}

pub fn link_zoropool(code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let code_builder = link_asset_utils(code_builder)?;
    let mut code_builder = link_storage_utils(code_builder)?;

    let pool_code = read_masm_file(&["accounts", "zoropool.masm"])?;
    code_builder.link_module("zoroswap::zoropool", &pool_code)?;
    Ok(code_builder)
}

pub fn link_note_common_lib(code_builder: CodeBuilder) -> Result<CodeBuilder> {
    let mut code_builder = code_builder.clone();
    let note_common_lib_code = read_masm_file(&["notes", "lib", "common.masm"])?;
    code_builder.link_module("zoro_miden::note::common", &note_common_lib_code)?;
    Ok(code_builder)
}

pub fn link_all_libraries(code_builder: CodeBuilder) -> Result<CodeBuilder> {
    link_note_common_lib(link_zoropool(code_builder)?)
}
