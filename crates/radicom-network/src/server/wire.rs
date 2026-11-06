//! Command-set codecs (Implicit VR LE), shared by the SCP and SCU.

use super::*;

/// Encodes a command set.
///
/// PS3.5 §6.3.1/§7.7: a command set is always Implicit VR Little Endian, no
/// matter what transfer syntax the presentation context negotiated. The writer
/// recomputes and keeps (0000,0000) (§11.1).
pub fn command_bytes(command: &Command) -> Result<Vec<u8>> {
    crate::message::encode_command(&command.to_dataset())
}

/// Parses a command set (§11.2), matching [`command_bytes`].
pub fn command_from_bytes(bytes: &[u8]) -> Result<Command> {
    let dataset = parse_command_dataset(bytes)?;
    Command::from_dataset(&dataset)
}

/// Parses command bytes, tolerating trailing padding.
pub fn parse_command_dataset(bytes: &[u8]) -> Result<Dataset> {
    radicom::io::dataset_builder::parse_dataset_bytes(bytes, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
        .map_err(|e| {
            NetworkError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })
}
