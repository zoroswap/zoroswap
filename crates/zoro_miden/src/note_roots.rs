use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use miden_client::{Word, assembly::CodeBuilder};
use miden_standards::note::StandardNote;
use tracing::info;

use crate::note::{NoteKind, TrustedNote};

pub static NOTE_ROOTS: OnceLock<NoteRoots> = OnceLock::new();

pub fn get_note_roots() -> &'static NoteRoots {
    NOTE_ROOTS.get_or_init(|| {
        info!("Initializing Note roots ...");
        NoteRoots::generate_from_notes().expect("Generating Note Roots failed.")
    })
}

pub struct NoteRoots {
    deposit: Word,
    withdraw: Word,
    swap: Word,
    p2id: Word,
}

impl NoteRoots {
    pub fn generate_from_notes() -> Result<Self> {
        Ok(Self {
            deposit: get_script_root_for_local_script("DEPOSIT.masm")?,
            withdraw: get_script_root_for_local_script("WITHDRAW.masm")?,
            swap: get_script_root_for_local_script("ZOROSWAP.masm")?,
            p2id: StandardNote::P2ID.script_root(),
        })
    }

    pub fn get_order_type(&self, root: &Word) -> Result<NoteKind> {
        if root.eq(&self.deposit) {
            Ok(NoteKind::Deposit)
        } else if root.eq(&self.withdraw) {
            Ok(NoteKind::Withdraw)
        } else if root.eq(&self.swap) {
            Ok(NoteKind::Swap)
        } else if root.eq(&self.p2id) {
            Ok(NoteKind::P2ID)
        } else {
            Err(anyhow!(
                "Passed note root does not belong to a known note kind."
            ))
        }
    }
}

pub fn get_script_root_for_local_script(masm_name: &str) -> Result<Word> {
    let code_builder = CodeBuilder::new();
    let note_script = TrustedNote::get_note_script(code_builder, masm_name)?;
    Ok(note_script.root())
}

#[cfg(test)]
mod tests {
    use crate::note_roots::NoteRoots;

    #[test]
    pub fn create_note_roots() {
        NoteRoots::generate_from_notes().unwrap();
    }
}
