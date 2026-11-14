//! Append-only key/value store over an in-memory log.

use std::collections::BTreeMap;

use wire::{decode_frame, encode_frame, Record, WireError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The log is corrupt at the given byte offset.
    Corrupt { offset: usize, source: WireError },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Corrupt { offset, source } => {
                write!(f, "corrupt log at byte {offset}: {source}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Debug, Default)]
pub struct Store {
    log: Vec<u8>,
    index: BTreeMap<Vec<u8>, Vec<u8>>,
    records: usize,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a store by replaying an existing log.
    pub fn open(log: Vec<u8>) -> Result<Self, StoreError> {
        let mut index = BTreeMap::new();
        let mut offset = 0;
        let mut records = 0;
        while offset < log.len() {
            let (record, used) = decode_frame(&log[offset..])
                .map_err(|source| StoreError::Corrupt { offset, source })?;
            apply(&mut index, record);
            offset += used;
            records += 1;
        }
        Ok(Store {
            log,
            index,
            records,
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.append(Record::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        });
    }

    pub fn delete(&mut self, key: &[u8]) -> bool {
        if !self.index.contains_key(key) {
            return false;
        }
        self.append(Record::Delete { key: key.to_vec() });
        true
    }

    fn append(&mut self, record: Record) {
        encode_frame(&record, &mut self.log);
        apply(&mut self.index, record);
        self.records += 1;
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.index.get(key).map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Keys in ascending byte order.
    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.index.keys().map(Vec::as_slice)
    }

    /// Number of records in the log (including overwritten and deleted ones).
    pub fn record_count(&self) -> usize {
        self.records
    }

    pub fn log(&self) -> &[u8] {
        &self.log
    }

    pub fn into_log(self) -> Vec<u8> {
        self.log
    }

    /// Rewrite the log so it holds exactly one `Put` per live key.
    pub fn compact(&mut self) {
        let mut log = Vec::new();
        for (key, value) in &self.index {
            encode_frame(
                &Record::Put {
                    key: key.clone(),
                    value: value.clone(),
                },
                &mut log,
            );
        }
        self.records = self.index.len();
        self.log = log;
    }
}

fn apply(index: &mut BTreeMap<Vec<u8>, Vec<u8>>, record: Record) {
    match record {
        Record::Put { key, value } => {
            index.insert(key, value);
        }
        Record::Delete { key } => {
            index.remove(&key);
        }
    }
}
