//! Shared candidate registry, generated from `protocol/read_keys.csv`.
//! A report for another software ID is a candidate, not proof of compatibility.

/// A read-access key and its provenance.
pub struct ReadKeyCandidate {
    /// Numeric key, encoded little-endian by the protocol.
    pub key: u16,
    /// Space-separated software IDs with reported support.
    pub software_ids: &'static str,
    /// Implementation or original device report.
    pub source: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/read_keys.rs"));
