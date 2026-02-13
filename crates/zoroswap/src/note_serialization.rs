use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use miden_client::note::Note;
use miden_protocol::utils::{Deserializable, Serializable};

pub fn serialize_note(note: &Note) -> Result<String> {
    // Use the proper Miden serialization method
    let note_bytes: Vec<u8> = note.to_bytes();
    let encoded = general_purpose::STANDARD.encode(note_bytes);
    Ok(encoded)
}

pub fn deserialize_note(encoded: &str) -> Result<Note> {
    // Decode from base64
    let note_bytes = general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| anyhow!("Failed to decode base64: {}", e))?;

    // Deserialize the note from bytes using the proper method
    let note = Note::read_from_bytes(&note_bytes)
        .map_err(|e| anyhow!("Failed to deserialize note: {}", e))?;

    Ok(note)
}
