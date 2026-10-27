//! DIMSE message encoding: command + dataset as one message (NET-05/06;
//! docs/07-network.md §5.2, §5.3).
//!
//! A message is a command set followed by an optional dataset. The command
//! set is always Implicit VR Little Endian; the dataset uses the presentation
//! context's accepted transfer syntax.

use crate::error::{NetworkError, Result};
use crate::pdu::{PData, Pdv};
use radicom::io::dataset_builder::parse_dataset_bytes;
use radicom::io::writer::{DicomWriter, WriteOptions};
use radicom::model::dataset::Dataset;
use radicom::model::transfer_syntax::{syntaxes, TransferSyntax};

/// Encodes a command set, keeping its recomputed group length (§5.3).
pub fn encode_command(dataset: &Dataset) -> Result<Vec<u8>> {
    let options = WriteOptions { keep_group_lengths: true, ..Default::default() };
    let mut out = Vec::new();
    DicomWriter::new(options)
        .write_dataset(dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut out)
        .map_err(|_| NetworkError::protocol("command set could not be encoded"))?;
    Ok(out)
}

/// Decodes a command set (§5.2).
pub fn decode_command(bytes: &[u8]) -> Result<Dataset> {
    parse_dataset_bytes(bytes, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN)
        .map_err(|e| NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))
}

/// Encodes a dataset under the negotiated syntax, dropping group lengths (§5.3).
pub fn encode_dataset(dataset: &Dataset, syntax: &TransferSyntax) -> Result<Vec<u8>> {
    let options = WriteOptions { keep_group_lengths: false, ..Default::default() };
    let mut out = Vec::new();
    DicomWriter::new(options)
        .write_dataset(dataset, syntax, &mut out)
        .map_err(|_| NetworkError::protocol("dataset could not be encoded"))?;
    Ok(out)
}

/// Decodes a dataset under the negotiated syntax (§5.2: validation off).
pub fn decode_dataset(bytes: &[u8], syntax: &TransferSyntax) -> Result<Dataset> {
    parse_dataset_bytes(bytes, syntax)
        .map_err(|e| NetworkError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())))
}

/// Splits a command (and optional dataset) into P-DATA-TF PDUs (§5.3).
///
/// The command stream always occupies its own P-DATA-TF, before the dataset.
pub fn split_message(
    context_id: u8,
    command: &[u8],
    dataset: Option<&[u8]>,
    max_pdu: u32,
    max_pdvs_per_pdu: usize,
) -> Result<Vec<PData>> {
    let splitter = crate::assembler::PduSplitter::new(max_pdu, max_pdvs_per_pdu);
    splitter.split_message(context_id, command, dataset)
}

/// True when a PDV sequence carries a command first, as required by §5.3.
pub fn starts_with_command(pdus: &[PData]) -> bool {
    pdus.first()
        .and_then(|p| p.pdvs.first())
        .map(|pdv: &Pdv| pdv.is_command)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dimse::{Command, CommandField};
    use radicom::model::tag::Tag;
    use radicom::model::vr::Vr;

    fn sample_dataset() -> Dataset {
        let mut ds = Dataset::new();
        ds.insert_str(Tag::new(0x0008, 0x0016), Vr::Ui, "1.2.840.10008.5.1.4.1.1.7");
        ds.insert_str(Tag::new(0x0010, 0x0010), Vr::Pn, "Doe^John");
        ds
    }

    #[test]
    fn command_encoding_keeps_the_group_length() {
        let command = Command::request(CommandField::CEchoRq, 1);
        let bytes = encode_command(&command.to_dataset()).unwrap();
        // (0000,0000) is the first element and must be present
        assert_eq!(&bytes[..4], &[0x00, 0x00, 0x00, 0x00]);
        let ds = decode_command(&bytes).unwrap();
        assert!(ds.contains(&Tag::new(0x0000, 0x0000)), "command group length kept");
        assert_eq!(Command::from_dataset(&ds).unwrap().field, CommandField::CEchoRq);
    }

    #[test]
    fn dataset_round_trips_under_each_syntax() {
        let ds = sample_dataset();
        for syntax in [
            syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
            syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone(),
        ] {
            let bytes = encode_dataset(&ds, &syntax).unwrap();
            let back = decode_dataset(&bytes, &syntax).unwrap();
            assert_eq!(back, ds, "round-trip under {}", syntax.uid());
        }
    }

    #[test]
    fn dataset_encoding_drops_group_lengths() {
        let mut ds = sample_dataset();
        ds.insert_value(Tag::new(0x0010, 0x0000), 4u32);
        let bytes = encode_dataset(&ds, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let back = decode_dataset(&bytes, &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        assert!(!back.contains(&Tag::new(0x0010, 0x0000)), "group lengths removed on send (§5.3)");
    }

    #[test]
    fn message_splitting_puts_the_command_first() {
        let command = encode_command(&Command::request(CommandField::CStoreRq, 3).to_dataset()).unwrap();
        let dataset = encode_dataset(&sample_dataset(), &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        let pdus = split_message(1, &command, Some(&dataset), 16384, 16).unwrap();
        assert_eq!(pdus.len(), 2);
        assert!(starts_with_command(&pdus));
        assert!(pdus[0].pdvs.iter().all(|p| p.is_command));
        assert!(pdus[1].pdvs.iter().all(|p| !p.is_command));
    }

    #[test]
    fn command_only_message_has_one_pdu_set() {
        let command = encode_command(&Command::request(CommandField::CEchoRq, 1).to_dataset()).unwrap();
        let pdus = split_message(1, &command, None, 16384, 16).unwrap();
        assert_eq!(pdus.len(), 1);
        assert!(starts_with_command(&pdus));
    }

    #[test]
    fn reassembly_of_a_split_message_is_lossless() {
        let command = encode_command(&Command::request(CommandField::CStoreRq, 9).to_dataset()).unwrap();
        let dataset = encode_dataset(&sample_dataset(), &syntaxes::EXPLICIT_VR_LITTLE_ENDIAN).unwrap();
        // a small maximum forces several PDVs per stream
        let pdus = split_message(1, &command, Some(&dataset), 48, 4).unwrap();
        let mut assembler = crate::assembler::PdvAssembler::new();
        let mut units = Vec::new();
        for pdu in pdus {
            units.extend(assembler.push_pdata(&pdu).unwrap());
        }
        assert_eq!(units.len(), 2, "one command unit and one dataset unit");
        assert_eq!(units[0].bytes, command);
        assert_eq!(units[1].bytes, dataset);
    }
}
