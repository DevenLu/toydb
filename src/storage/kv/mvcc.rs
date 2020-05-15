use super::Store;
use crate::Error;

use serde::{Deserialize, Serialize};
use serde_derive::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ops::{Bound, RangeBounds};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// MVCC status
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub txns: u64,
    pub txns_active: u64,
}

/// An MVCC-based transactional key-value store.
pub struct MVCC<S: Store> {
    /// The underlying KV store. It is protected by a mutex so it can be shared between multiple
    /// transactions.
    store: Arc<RwLock<S>>,
}

// FIXME Implement Clone manually due to https://github.com/rust-lang/rust/issues/26925
impl<S: Store> Clone for MVCC<S> {
    fn clone(&self) -> Self {
        MVCC { store: self.store.clone() }
    }
}

impl<S: Store> MVCC<S> {
    /// Creates a new MVCC key-value store with the given key-value store for storage.
    pub fn new(store: S) -> Self {
        Self { store: Arc::new(RwLock::new(store)) }
    }

    /// Begins a new transaction in read-write mode.
    #[allow(dead_code)]
    pub fn begin(&self) -> Result<Transaction<S>, Error> {
        Transaction::begin(self.store.clone(), Mode::ReadWrite)
    }

    /// Begins a new transaction in the given mode.
    pub fn begin_with_mode(&self, mode: Mode) -> Result<Transaction<S>, Error> {
        Transaction::begin(self.store.clone(), mode)
    }

    /// Resumes a transaction with the given ID.
    pub fn resume(&self, id: u64) -> Result<Transaction<S>, Error> {
        Transaction::resume(self.store.clone(), id)
    }

    /// Fetches an unversioned metadata value
    pub fn get_metadata(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let session = self.store.read()?;
        session.get(key)
    }

    /// Sets an unversioned metadata value
    pub fn set_metadata(&self, key: &[u8], value: Vec<u8>) -> Result<(), Error> {
        let mut session = self.store.write()?;
        session.set(key, value)
    }

    /// Returns engine status
    //
    // Bizarrely, the return statement is in fact necessary - see:
    // https://github.com/rust-lang/reference/issues/452
    #[allow(clippy::needless_return)]
    pub fn status(&self) -> Result<Status, Error> {
        let session = self.store.read()?;
        return Ok(Status {
            txns: match session.get(&Key::TxnNext.encode())? {
                Some(ref v) => deserialize(v)?,
                None => 1,
            } - 1,
            txns_active: session
                .scan(&Key::TxnActive(0).encode()..&Key::TxnActive(std::u64::MAX).encode())
                .try_fold(0, |count, r| r.map(|_| count + 1))?,
        });
    }
}

/// Serializes MVCC metadata.
fn serialize<V: Serialize>(value: &V) -> Result<Vec<u8>, Error> {
    Ok(bincode::serialize(value)?)
}

/// Deserializes MVCC metadata.
fn deserialize<'a, V: Deserialize<'a>>(bytes: &'a [u8]) -> Result<V, Error> {
    Ok(bincode::deserialize(bytes)?)
}

/// An MVCC transaction.
pub struct Transaction<S: Store> {
    /// The underlying store for the transaction. Shared between transactions using a mutex.
    store: Arc<RwLock<S>>,
    /// The unique transaction ID.
    id: u64,
    /// The transaction mode.
    mode: Mode,
    /// The snapshot that the transaction is running in.
    snapshot: Snapshot,
}

impl<S: Store> Transaction<S> {
    /// Begins a new transaction in the given mode.
    fn begin(store: Arc<RwLock<S>>, mode: Mode) -> Result<Self, Error> {
        let mut session = store.write()?;

        let id = match session.get(&Key::TxnNext.encode())? {
            Some(ref v) => deserialize(v)?,
            None => 1,
        };
        session.set(&Key::TxnNext.encode(), serialize(&(id + 1))?)?;
        session.set(&Key::TxnActive(id).encode(), serialize(&mode)?)?;

        // We always take a new snapshot, even for snapshot transactions, because all transactions
        // increment the transaction ID and we need to properly record currently active transactions
        // for any future snapshot transactions looking at this one.
        let mut snapshot = Snapshot::take(&mut session, id)?;
        std::mem::drop(session);
        if let Mode::Snapshot { version } = &mode {
            snapshot = Snapshot::restore(&store.read()?, *version)?
        }

        Ok(Self { store, id, mode, snapshot })
    }

    /// Resumes an active transaction with the given ID. Errors if the transaction is not active.
    fn resume(store: Arc<RwLock<S>>, id: u64) -> Result<Self, Error> {
        let session = store.read()?;
        let mode = match session.get(&Key::TxnActive(id).encode())? {
            Some(v) => deserialize(&v)?,
            None => return Err(Error::Value(format!("No active transaction {}", id))),
        };
        let snapshot = match &mode {
            Mode::Snapshot { version } => Snapshot::restore(&session, *version)?,
            _ => Snapshot::restore(&session, id)?,
        };
        std::mem::drop(session);
        Ok(Self { store, id, mode, snapshot })
    }

    /// Returns the transaction ID.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the transaction mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Commits the transaction, by removing the txn from the active set.
    pub fn commit(self) -> Result<(), Error> {
        let mut session = self.store.write()?;
        session.delete(&Key::TxnActive(self.id).encode())?;
        session.flush()
    }

    /// Rolls back the transaction, by removing all updated entries.
    pub fn rollback(self) -> Result<(), Error> {
        let mut session = self.store.write()?;
        if self.mode.mutable() {
            let mut rollback = Vec::new();
            let mut scan = session.scan(
                &Key::TxnUpdate(self.id, vec![]).encode()
                    ..&Key::TxnUpdate(self.id + 1, vec![]).encode(),
            );
            while let Some((key, _)) = scan.next().transpose()? {
                match Key::decode(&key)? {
                    Key::TxnUpdate(_, updated_key) => rollback.push(updated_key),
                    k => return Err(Error::Internal(format!("Expected TxnUpdate, got {:?}", k))),
                };
                rollback.push(key);
            }
            std::mem::drop(scan);
            for key in rollback.into_iter() {
                session.delete(&key)?;
            }
        }
        session.delete(&Key::TxnActive(self.id).encode())
    }

    /// Deletes a key.
    pub fn delete(&mut self, key: &[u8]) -> Result<(), Error> {
        self.write(key, None)
    }

    /// Fetches a key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let session = self.store.read()?;
        let mut scan = session
            .scan(
                Key::Record(key.to_vec(), 0).encode()..=Key::Record(key.to_vec(), self.id).encode(),
            )
            .rev();
        while let Some((k, v)) = scan.next().transpose()? {
            match Key::decode(&k)? {
                Key::Record(_, version) => {
                    if self.snapshot.is_visible(version) {
                        return deserialize(&v);
                    }
                }
                k => return Err(Error::Internal(format!("Expected Txn::Record, got {:?}", k))),
            };
        }
        Ok(None)
    }

    /// Scans a key range.
    pub fn scan(&self, range: impl RangeBounds<Vec<u8>>) -> Result<super::Scan, Error> {
        Ok(Box::new(Scan::new(self.store.clone(), self.snapshot.clone(), range)?))
    }

    /// Scans keys under a given prefix.
    pub fn scan_prefix(&self, prefix: &[u8]) -> Result<super::Scan, Error> {
        if prefix.is_empty() {
            return Err(Error::Internal("Scan prefix cannot be empty".into()));
        }
        let start = prefix.to_vec();
        let mut end = start.clone();
        for i in (0..end.len()).rev() {
            match end[i] {
                // If all 0xff we could in principle use Range::Unbounded, but it's won't happen
                0xff if i == 0 => return Err(Error::Internal("Invalid prefix scan range".into())),
                0xff => {
                    end[i] = 0x00;
                    continue;
                }
                v => {
                    end[i] = v + 1;
                    break;
                }
            }
        }
        Ok(Box::new(Scan::new(self.store.clone(), self.snapshot.clone(), start..end)?))
    }

    /// Sets a key.
    pub fn set(&mut self, key: &[u8], value: Vec<u8>) -> Result<(), Error> {
        self.write(key, Some(value))
    }

    /// Writes a value for a key. None is used for deletion.
    fn write(&self, key: &[u8], value: Option<Vec<u8>>) -> Result<(), Error> {
        if !self.mode.mutable() {
            return Err(Error::ReadOnly);
        }
        let mut session = self.store.write()?;

        // Check if the key is dirty, i.e. if it has any uncommitted changes, by scanning for any
        // versions that aren't visible to us.
        let min = self.snapshot.invisible.iter().min().cloned().unwrap_or(self.id + 1);
        let mut scan = session
            .scan(
                Key::Record(key.to_vec(), min).encode()
                    ..=Key::Record(key.to_vec(), std::u64::MAX).encode(),
            )
            .rev();
        while let Some((k, _)) = scan.next().transpose()? {
            match Key::decode(&k)? {
                Key::Record(_, version) => {
                    if !self.snapshot.is_visible(version) {
                        return Err(Error::Serialization);
                    }
                }
                k => return Err(Error::Internal(format!("Expected Txn::Record, got {:?}", k))),
            };
        }
        std::mem::drop(scan);

        // Write the key and its update record.
        let key = Key::Record(key.to_vec(), self.id).encode();
        let update = Key::TxnUpdate(self.id, key.clone()).encode();
        session.set(&update, vec![])?;
        session.set(&key, serialize(&value)?)
    }
}

/// An MVCC transaction mode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Mode {
    /// A read-write transaction.
    ReadWrite,
    /// A read-only transaction.
    ReadOnly,
    /// A read-only transaction running in a snapshot of a given version.
    ///
    /// The version must refer to a committed transaction ID. Any changes visible to the original
    /// transaction will be visible in the snapshot (i.e. transactions that had not committed before
    /// the snapshot transaction started will not be visible, even though they have a lower version).
    Snapshot { version: u64 },
}

impl Mode {
    /// Checks whether the transaction mode can mutate data.
    pub fn mutable(&self) -> bool {
        match self {
            Self::ReadWrite => true,
            Self::ReadOnly => false,
            Self::Snapshot { .. } => false,
        }
    }

    /// Checks whether a mode satisfies a mode (i.e. ReadWrite satisfies ReadOnly).
    pub fn satisfies(&self, other: &Mode) -> bool {
        match (self, other) {
            (Mode::ReadWrite, Mode::ReadOnly) => true,
            (Mode::Snapshot { .. }, Mode::ReadOnly) => true,
            (_, _) if self == other => true,
            (_, _) => false,
        }
    }
}

/// A versioned snapshot, containing visibility information about concurrent transactions.
#[derive(Clone)]
struct Snapshot {
    /// The version (i.e. transaction ID) that the snapshot belongs to.
    version: u64,
    /// The set of transaction IDs that were active at the start of the transactions,
    /// and thus should be invisible to the snapshot.
    invisible: HashSet<u64>,
}

impl Snapshot {
    /// Takes a new snapshot, persisting it as `Key::TxnSnapshot(version)`.
    fn take(session: &mut RwLockWriteGuard<impl Store>, version: u64) -> Result<Self, Error> {
        let mut snapshot = Self { version, invisible: HashSet::new() };
        let mut scan = session.scan(&Key::TxnActive(0).encode()..&Key::TxnActive(version).encode());
        while let Some((key, _)) = scan.next().transpose()? {
            match Key::decode(&key)? {
                Key::TxnActive(id) => snapshot.invisible.insert(id),
                k => return Err(Error::Internal(format!("Expected TxnActive, got {:?}", k))),
            };
        }
        std::mem::drop(scan);
        session.set(&Key::TxnSnapshot(version).encode(), serialize(&snapshot.invisible)?)?;
        Ok(snapshot)
    }

    /// Restores an existing snapshot from `Key::TxnSnapshot(version)`, or errors if not found.
    fn restore(session: &RwLockReadGuard<impl Store>, version: u64) -> Result<Self, Error> {
        match session.get(&Key::TxnSnapshot(version).encode())? {
            Some(ref v) => Ok(Self { version, invisible: deserialize(v)? }),
            None => Err(Error::Value(format!("Snapshot not found for version {}", version))),
        }
    }

    /// Checks whether the given version is visible in this snapshot.
    fn is_visible(&self, version: u64) -> bool {
        version <= self.version && self.invisible.get(&version).is_none()
    }
}

/// MVCC keys. The encoding must preserve the grouping and ordering of keys.
///
/// The first byte determines the key type. u64 is encoded in big-endian byte order. For Vec<u8>, we
/// use 0x00 0xff as an escape sequence for 0x00, and 0x00 0x00 as a terminator, to avoid
/// key/version overlaps from messing up the key sequence during scans - see:
/// https://activesphere.com/blog/2018/08/17/order-preserving-serialization
#[derive(Debug)]
enum Key {
    /// The next available txn ID. Used when starting new txns.
    TxnNext,
    /// Marker for active txns, containing the txn mode. Used to detect concurrent txns, and
    /// to resume txns.
    TxnActive(u64),
    /// Txn snapshot, containing concurrent active txns at start of txn.
    TxnSnapshot(u64),
    /// Update marker for a txn ID and key, used for rollback.
    TxnUpdate(u64, Vec<u8>),
    /// A record for a key/version pair.
    Record(Vec<u8>, u64),
    /// Arbitrary unversioned metadata.
    Metadata(Vec<u8>),
}

impl Key {
    /// Decodes a key from a byte representation.
    fn decode(key: &[u8]) -> Result<Self, Error> {
        let mut iter = key.iter();
        match iter.next() {
            Some(0x01) => Ok(Key::TxnNext),
            Some(0x02) => Ok(Key::TxnActive(Self::decode_u64(&mut iter)?)),
            Some(0x03) => Ok(Key::TxnSnapshot(Self::decode_u64(&mut iter)?)),
            Some(0x04) => Ok(Key::TxnUpdate(Self::decode_u64(&mut iter)?, iter.cloned().collect())),
            Some(0x05) => Ok(Key::Metadata(Self::decode_bytes(&mut iter)?)),
            Some(0xff) => {
                Ok(Self::Record(Self::decode_bytes(&mut iter)?, Self::decode_u64(&mut iter)?))
            }
            _ => Err(Error::Value("Unable to parse MVCC key".into())),
        }
    }

    /// Decodes a byte vector from a byte representation. See encode_bytes() for format.
    fn decode_bytes<'a, I: Iterator<Item = &'a u8>>(iter: &mut I) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        loop {
            match iter.next() {
                Some(0x00) => match iter.next() {
                    Some(0x00) => break,            // 0x00 0x00 is terminator
                    Some(0xff) => bytes.push(0x00), // 0x00 0xff is escape sequence for 0x00
                    b => return Err(Error::Value(format!("Unexpected 0x00 encoding {:?}", b))),
                },
                Some(b) => bytes.push(*b),
                None => return Err(Error::Value("Unexpected end of input".into())),
            }
        }
        Ok(bytes)
    }

    /// Decodes a u64 from a byte representation.
    fn decode_u64<'a, I: Iterator<Item = &'a u8>>(iter: &mut I) -> Result<u64, Error> {
        let bytes = iter.take(8).cloned().collect::<Vec<u8>>();
        if bytes.len() < 8 {
            return Err(Error::Value(format!("Unable to decode u64, got {} bytes", bytes.len())));
        }
        let mut buf = [0; 8];
        buf.copy_from_slice(&bytes[..]);
        Ok(u64::from_be_bytes(buf))
    }

    /// Encodes a key into a byte vector.
    fn encode(self) -> Vec<u8> {
        match self {
            Self::TxnNext => vec![0x01],
            Self::TxnActive(id) => [vec![0x02], Self::encode_u64(id)].concat(),
            Self::TxnSnapshot(version) => [vec![0x03], Self::encode_u64(version)].concat(),
            Self::TxnUpdate(id, key) => [vec![0x04], Self::encode_u64(id), key].concat(),
            Self::Metadata(key) => [vec![0x05], Self::encode_bytes(key)].concat(),
            Self::Record(key, version) => {
                [vec![0xff], Self::encode_bytes(key), Self::encode_u64(version)].concat()
            }
        }
    }

    /// Encodes a byte vector.
    fn encode_bytes(bytes: Vec<u8>) -> Vec<u8> {
        let mut escaped = vec![];
        for b in bytes.into_iter() {
            escaped.push(b);
            if b == 0x00 {
                escaped.push(0xff);
            }
        }
        escaped.push(0x00);
        escaped.push(0x00);
        escaped
    }

    /// Encodes a u64.
    fn encode_u64(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }
}

/// A key range scan.
/// FIXME This should really just wrap the underlying iterator via a RwLock guard for the store,
/// but making the lifetimes work out is non-trivial. See also:
/// https://users.rust-lang.org/t/creating-an-iterator-over-mutex-contents-cannot-infer-an-appropriate-lifetime/24458
pub struct Scan<S: Store> {
    /// The KV store used for the scan.
    store: Arc<RwLock<S>>,
    /// The snapshot the scan is running in.
    snapshot: Snapshot,
    /// Keeps track of the remaining range bounds we're iterating over.
    bounds: (Bound<Vec<u8>>, Bound<Vec<u8>>),
    /// Keeps track of next() candidate pair to be returned if no newer versions are found.
    next_candidate: Option<(Vec<u8>, Option<Vec<u8>>)>,
    /// Keeps track of next_back() returned key, whose older versions should be ignored.
    next_back_returned: Option<Vec<u8>>,
}

impl<S: Store> Scan<S> {
    /// Creates a new scan.
    fn new(
        store: Arc<RwLock<S>>,
        snapshot: Snapshot,
        range: impl RangeBounds<Vec<u8>>,
    ) -> Result<Self, Error> {
        let start = match range.start_bound() {
            Bound::Excluded(k) => Bound::Excluded(Key::Record(k.clone(), std::u64::MAX).encode()),
            Bound::Included(k) => Bound::Included(Key::Record(k.clone(), 0).encode()),
            Bound::Unbounded => Bound::Included(Key::Record(vec![], 0).encode()),
        };
        let end = match range.end_bound() {
            Bound::Excluded(k) => Bound::Excluded(Key::Record(k.clone(), 0).encode()),
            Bound::Included(k) => Bound::Included(Key::Record(k.clone(), std::u64::MAX).encode()),
            Bound::Unbounded => Bound::Unbounded,
        };

        Ok(Self {
            store,
            snapshot,
            bounds: (start, end),
            next_candidate: None,
            next_back_returned: None,
        })
    }

    // next() with error handling.
    #[allow(clippy::type_complexity)]
    fn try_next(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        let session = self.store.read()?;
        let mut range = session.scan(self.bounds.clone());
        while let Some((k, v)) = range.next().transpose()? {
            // Keep track of iterator progress
            self.bounds.0 = Bound::Excluded(k.clone());

            let (key, version) = match Key::decode(&k)? {
                Key::Record(key, version) => (key, version),
                k => return Err(Error::Internal(format!("Expected Record, got {:?}", k))),
            };
            if !self.snapshot.is_visible(version) {
                continue;
            }

            // Keep track of return candidate, and return current candidate if key changes.
            let ret = match &self.next_candidate {
                Some((k, Some(v))) if k != &key => Some((k.clone(), v.clone())),
                _ => None,
            };
            self.next_candidate = Some((key, deserialize(&v)?));
            if ret.is_some() {
                return Ok(ret);
            }
        }

        // When iteration ends, return the last candidate if any
        if let Some((k, Some(v))) = self.next_candidate.clone() {
            self.next_candidate = None;
            Ok(Some((k, v)))
        } else {
            Ok(None)
        }
    }

    /// next_back() with error handling.
    #[allow(clippy::type_complexity)]
    fn try_next_back(&mut self) -> Result<Option<(Vec<u8>, Vec<u8>)>, Error> {
        let session = self.store.read()?;
        let mut range = session.scan(self.bounds.clone());
        while let Some((k, v)) = range.next_back().transpose()? {
            // Keep track of iterator progress
            self.bounds.1 = Bound::Excluded(k.clone());

            let (key, version) = match Key::decode(&k)? {
                Key::Record(key, version) => (key, version),
                k => return Err(Error::Internal(format!("Expected Record, got {:?}", k))),
            };
            if !self.snapshot.is_visible(version) {
                continue;
            }

            // Keep track of keys already been seen and returned (i.e. skip older versions)
            if self.next_back_returned.as_ref() == Some(&key) {
                continue;
            }
            self.next_back_returned = Some(key.clone());

            if let Some(value) = deserialize(&v)? {
                return Ok(Some((key, value)));
            }
        }
        Ok(None)
    }
}

impl<S: Store> Iterator for Scan<S> {
    type Item = Result<(Vec<u8>, Vec<u8>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.try_next().transpose()
    }
}

impl<S: Store> DoubleEndedIterator for Scan<S> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.try_next_back().transpose()
    }
}

#[cfg(test)]
pub mod tests {
    use super::super::Test;
    use super::*;

    fn setup() -> MVCC<Test> {
        MVCC::new(Test::new())
    }

    #[test]
    fn test_begin() -> Result<(), Error> {
        let mvcc = setup();

        let txn = mvcc.begin()?;
        assert_eq!(1, txn.id());
        assert_eq!(Mode::ReadWrite, txn.mode());
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(2, txn.id());
        txn.rollback()?;

        let txn = mvcc.begin()?;
        assert_eq!(3, txn.id());
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_begin_with_mode_readonly() -> Result<(), Error> {
        let mvcc = setup();
        let txn = mvcc.begin_with_mode(Mode::ReadOnly)?;
        assert_eq!(1, txn.id());
        assert_eq!(Mode::ReadOnly, txn.mode());
        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_begin_with_mode_readwrite() -> Result<(), Error> {
        let mvcc = setup();
        let txn = mvcc.begin_with_mode(Mode::ReadWrite)?;
        assert_eq!(1, txn.id());
        assert_eq!(Mode::ReadWrite, txn.mode());
        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_begin_with_mode_snapshot() -> Result<(), Error> {
        let mvcc = setup();

        // Write a couple of versions for a key
        let mut txn = mvcc.begin_with_mode(Mode::ReadWrite)?;
        txn.set(b"key", vec![0x01])?;
        txn.commit()?;
        let mut txn = mvcc.begin_with_mode(Mode::ReadWrite)?;
        txn.set(b"key", vec![0x02])?;
        txn.commit()?;

        // Check that we can start a snapshot in version 1
        let txn = mvcc.begin_with_mode(Mode::Snapshot { version: 1 })?;
        assert_eq!(3, txn.id());
        assert_eq!(Mode::Snapshot { version: 1 }, txn.mode());
        assert_eq!(Some(vec![0x01]), txn.get(b"key")?);
        txn.commit()?;

        // Check that we can start a snapshot in a past snapshot transaction
        let txn = mvcc.begin_with_mode(Mode::Snapshot { version: 3 })?;
        assert_eq!(4, txn.id());
        assert_eq!(Mode::Snapshot { version: 3 }, txn.mode());
        assert_eq!(Some(vec![0x02]), txn.get(b"key")?);
        txn.commit()?;

        // Check that the current transaction ID is valid as a snapshot version
        let txn = mvcc.begin_with_mode(Mode::Snapshot { version: 5 })?;
        assert_eq!(5, txn.id());
        assert_eq!(Mode::Snapshot { version: 5 }, txn.mode());
        txn.commit()?;

        // Check that any future transaction IDs are invalid
        assert_eq!(
            mvcc.begin_with_mode(Mode::Snapshot { version: 9 }).err(),
            Some(Error::Value("Snapshot not found for version 9".into()))
        );

        // Check that concurrent transactions are hidden from snapshots of snapshot transactions.
        // This is because any transaction, including a snapshot transaction, allocates a new
        // transaction ID, and we need to make sure concurrent transaction at the time the
        // transaction began are hidden from future snapshot transactions.
        let mut txn_active = mvcc.begin()?;
        let txn_snapshot = mvcc.begin_with_mode(Mode::Snapshot { version: 1 })?;
        assert_eq!(7, txn_active.id());
        assert_eq!(8, txn_snapshot.id());
        txn_active.set(b"key", vec![0x07])?;
        assert_eq!(Some(vec![0x01]), txn_snapshot.get(b"key")?);
        txn_active.commit()?;
        txn_snapshot.commit()?;

        let txn = mvcc.begin_with_mode(Mode::Snapshot { version: 8 })?;
        assert_eq!(9, txn.id());
        assert_eq!(Some(vec![0x02]), txn.get(b"key")?);
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_resume() -> Result<(), Error> {
        let mvcc = setup();

        // We first write a set of values that should be visible
        let mut t1 = mvcc.begin()?;
        t1.set(b"a", b"t1".to_vec())?;
        t1.set(b"b", b"t1".to_vec())?;
        t1.commit()?;

        // We then start three transactions, of which we will resume t3.
        // We commit t2 and t4's changes, which should not be visible,
        // and write a change for t3 which should be visible.
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;
        let mut t4 = mvcc.begin()?;

        t2.set(b"a", b"t2".to_vec())?;
        t3.set(b"b", b"t3".to_vec())?;
        t4.set(b"c", b"t4".to_vec())?;

        t2.commit()?;
        t4.commit()?;

        // We now resume t3, who should see it's own changes but none
        // of the others'
        let id = t3.id();
        std::mem::drop(t3);
        let tr = mvcc.resume(id)?;
        assert_eq!(3, tr.id());
        assert_eq!(Mode::ReadWrite, tr.mode());

        assert_eq!(Some(b"t1".to_vec()), tr.get(b"a")?);
        assert_eq!(Some(b"t3".to_vec()), tr.get(b"b")?);
        assert_eq!(None, tr.get(b"c")?);

        // A separate transaction should not see t3's changes, but should see the others
        let t = mvcc.begin()?;
        assert_eq!(Some(b"t2".to_vec()), t.get(b"a")?);
        assert_eq!(Some(b"t1".to_vec()), t.get(b"b")?);
        assert_eq!(Some(b"t4".to_vec()), t.get(b"c")?);
        t.rollback()?;

        // Once tr commits, a separate transaction should see t3's changes
        tr.commit()?;

        let t = mvcc.begin()?;
        assert_eq!(Some(b"t2".to_vec()), t.get(b"a")?);
        assert_eq!(Some(b"t3".to_vec()), t.get(b"b")?);
        assert_eq!(Some(b"t4".to_vec()), t.get(b"c")?);
        t.rollback()?;

        // It should also be possible to start a snapshot transaction and resume it.
        let ts = mvcc.begin_with_mode(Mode::Snapshot { version: 1 })?;
        assert_eq!(7, ts.id());
        assert_eq!(Some(b"t1".to_vec()), ts.get(b"a")?);

        let id = ts.id();
        std::mem::drop(ts);
        let ts = mvcc.resume(id)?;
        assert_eq!(7, ts.id());
        assert_eq!(Mode::Snapshot { version: 1 }, ts.mode());
        assert_eq!(Some(b"t1".to_vec()), ts.get(b"a")?);
        ts.commit()?;

        // Resuming an inactive transaction should error.
        assert_eq!(mvcc.resume(7).err(), Some(Error::Value("No active transaction 7".into())));

        Ok(())
    }

    #[test]
    fn test_txn_delete_conflict() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"key", vec![0x00])?;
        txn.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.delete(b"key")?;
        assert_eq!(Err(Error::Serialization), t1.delete(b"key"));
        assert_eq!(Err(Error::Serialization), t3.delete(b"key"));
        t2.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_delete_idempotent() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.delete(b"key")?;
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        assert_eq!(None, txn.get(b"a")?);
        txn.set(b"a", vec![0x01])?;
        assert_eq!(Some(vec![0x01]), txn.get(b"a")?);
        txn.set(b"a", vec![0x02])?;
        assert_eq!(Some(vec![0x02]), txn.get(b"a")?);
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get_deleted() -> Result<(), Error> {
        let mvcc = setup();
        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.delete(b"a")?;
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(None, txn.get(b"a")?);
        txn.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_get_hides_newer() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t1.set(b"a", vec![0x01])?;
        t1.commit()?;
        t3.set(b"c", vec![0x03])?;
        t3.commit()?;

        assert_eq!(None, t2.get(b"a")?);
        assert_eq!(None, t2.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_hides_uncommitted() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        t1.set(b"a", vec![0x01])?;
        let t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;
        t3.set(b"c", vec![0x03])?;

        assert_eq!(None, t2.get(b"a")?);
        assert_eq!(None, t2.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_readonly_historical() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"b", vec![0x02])?;
        txn.commit()?;

        let mut txn = mvcc.begin()?;
        txn.set(b"c", vec![0x03])?;
        txn.commit()?;

        let tr = mvcc.begin_with_mode(Mode::Snapshot { version: 2 })?;
        assert_eq!(Some(vec![0x01]), tr.get(b"a")?);
        assert_eq!(Some(vec![0x02]), tr.get(b"b")?);
        assert_eq!(None, tr.get(b"c")?);

        Ok(())
    }

    #[test]
    fn test_txn_get_serial() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"a", vec![0x01])?;
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(Some(vec![0x01]), txn.get(b"a")?);

        Ok(())
    }

    #[test]
    fn test_txn_scan() -> Result<(), Error> {
        let mvcc = setup();
        let mut txn = mvcc.begin()?;

        txn.set(b"a", vec![0x01])?;

        txn.delete(b"b")?;

        txn.set(b"c", vec![0x01])?;
        txn.set(b"c", vec![0x02])?;
        txn.delete(b"c")?;
        txn.set(b"c", vec![0x03])?;

        txn.set(b"d", vec![0x01])?;
        txn.set(b"d", vec![0x02])?;
        txn.set(b"d", vec![0x03])?;
        txn.set(b"d", vec![0x04])?;
        txn.delete(b"d")?;

        txn.set(b"e", vec![0x01])?;
        txn.set(b"e", vec![0x02])?;
        txn.set(b"e", vec![0x03])?;
        txn.delete(b"e")?;
        txn.set(b"e", vec![0x04])?;
        txn.set(b"e", vec![0x05])?;
        txn.commit()?;

        // Forward scan
        let txn = mvcc.begin()?;
        assert_eq!(
            vec![
                (b"a".to_vec(), vec![0x01]),
                (b"c".to_vec(), vec![0x03]),
                (b"e".to_vec(), vec![0x05]),
            ],
            txn.scan(..)?.collect::<Result<Vec<_>, _>>()?
        );

        // Reverse scan
        assert_eq!(
            vec![
                (b"e".to_vec(), vec![0x05]),
                (b"c".to_vec(), vec![0x03]),
                (b"a".to_vec(), vec![0x01]),
            ],
            txn.scan(..)?.rev().collect::<Result<Vec<_>, _>>()?
        );

        // Alternate forward/backward scan
        let mut scan = txn.scan(..)?;
        assert_eq!(Some((b"a".to_vec(), vec![0x01])), scan.next().transpose()?);
        assert_eq!(Some((b"e".to_vec(), vec![0x05])), scan.next_back().transpose()?);
        assert_eq!(Some((b"c".to_vec(), vec![0x03])), scan.next_back().transpose()?);
        assert_eq!(None, scan.next().transpose()?);
        std::mem::drop(scan);

        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_txn_scan_key_version_overlap() -> Result<(), Error> {
        // The idea here is that with a naive key/version concatenation
        // we get overlapping entries that mess up scans. For example:
        //
        // 00|00 00 00 00 00 00 00 01
        // 00 00 00 00 00 00 00 00 02|00 00 00 00 00 00 00 02
        // 00|00 00 00 00 00 00 00 03
        //
        // The key encoding should be resistant to this.
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(&[0], vec![0])?; // v0
        txn.set(&[0], vec![1])?; // v1
        txn.set(&[0, 0, 0, 0, 0, 0, 0, 0, 2], vec![2])?; // v2
        txn.set(&[0], vec![3])?; // v3
        txn.commit()?;

        let txn = mvcc.begin()?;
        assert_eq!(
            vec![(vec![0].to_vec(), vec![3]), (vec![0, 0, 0, 0, 0, 0, 0, 0, 2].to_vec(), vec![2]),],
            txn.scan(..)?.collect::<Result<Vec<_>, _>>()?
        );
        Ok(())
    }

    #[test]
    fn test_txn_scan_prefix() -> Result<(), Error> {
        let mvcc = setup();
        let mut txn = mvcc.begin()?;

        txn.set(b"a", vec![0x01])?;
        txn.set(b"az", vec![0x01, 0x1a])?;
        txn.set(b"b", vec![0x02])?;
        txn.set(b"ba", vec![0x02, 0x01])?;
        txn.set(b"bb", vec![0x02, 0x02])?;
        txn.set(b"bc", vec![0x02, 0x03])?;
        txn.set(b"c", vec![0x03])?;
        txn.commit()?;

        // Forward scan
        let txn = mvcc.begin()?;
        assert_eq!(
            vec![
                (b"b".to_vec(), vec![0x02]),
                (b"ba".to_vec(), vec![0x02, 0x01]),
                (b"bb".to_vec(), vec![0x02, 0x02]),
                (b"bc".to_vec(), vec![0x02, 0x03]),
            ],
            txn.scan_prefix(b"b")?.collect::<Result<Vec<_>, _>>()?
        );

        // Reverse scan
        assert_eq!(
            vec![
                (b"bc".to_vec(), vec![0x02, 0x03]),
                (b"bb".to_vec(), vec![0x02, 0x02]),
                (b"ba".to_vec(), vec![0x02, 0x01]),
                (b"b".to_vec(), vec![0x02]),
            ],
            txn.scan_prefix(b"b")?.rev().collect::<Result<Vec<_>, _>>()?
        );

        // Alternate forward/backward scan
        let mut scan = txn.scan_prefix(b"b")?;
        assert_eq!(Some((b"b".to_vec(), vec![0x02])), scan.next().transpose()?);
        assert_eq!(Some((b"bc".to_vec(), vec![0x02, 0x03])), scan.next_back().transpose()?);
        assert_eq!(Some((b"bb".to_vec(), vec![0x02, 0x02])), scan.next_back().transpose()?);
        assert_eq!(Some((b"ba".to_vec(), vec![0x02, 0x01])), scan.next().transpose()?);
        assert_eq!(None, scan.next_back().transpose()?);
        std::mem::drop(scan);

        txn.commit()?;
        Ok(())
    }

    #[test]
    fn test_txn_set_conflict() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        assert_eq!(Err(Error::Serialization), t1.set(b"key", vec![0x01]));
        assert_eq!(Err(Error::Serialization), t3.set(b"key", vec![0x03]));
        t2.commit()?;

        Ok(())
    }

    #[test]
    fn test_txn_set_conflict_committed() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        t2.commit()?;
        assert_eq!(Err(Error::Serialization), t1.set(b"key", vec![0x01]));
        assert_eq!(Err(Error::Serialization), t3.set(b"key", vec![0x03]));

        Ok(())
    }

    #[test]
    fn test_txn_set_rollback() -> Result<(), Error> {
        let mvcc = setup();

        let mut txn = mvcc.begin()?;
        txn.set(b"key", vec![0x00])?;
        txn.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;
        let mut t3 = mvcc.begin()?;

        t2.set(b"key", vec![0x02])?;
        t2.rollback()?;
        assert_eq!(Some(vec![0x00]), t1.get(b"key")?);
        t1.commit()?;
        t3.set(b"key", vec![0x03])?;
        t3.commit()?;

        Ok(())
    }

    #[test]
    // A dirty write is when t2 overwrites an uncommitted value written by t1.
    fn test_txn_anomaly_dirty_write() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(t2.set(b"key", b"t2".to_vec()), Err(Error::Serialization));

        Ok(())
    }

    #[test]
    // A dirty read is when t2 can read an uncommitted value set by t1.
    fn test_txn_anomaly_dirty_read() -> Result<(), Error> {
        let mvcc = setup();

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(None, t2.get(b"key")?);

        Ok(())
    }

    #[test]
    // A lost update is when t1 and t2 both read a value and update it, where t2's update replaces t1.
    fn test_txn_anomaly_lost_update() -> Result<(), Error> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"key", b"t0".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        t1.get(b"key")?;
        t2.get(b"key")?;

        t1.set(b"key", b"t1".to_vec())?;
        assert_eq!(t2.set(b"key", b"t2".to_vec()), Err(Error::Serialization));

        Ok(())
    }

    #[test]
    // A fuzzy (or unrepeatable) read is when t2 sees a value change after t1 updates it.
    fn test_txn_anomaly_fuzzy_read() -> Result<(), Error> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"key", b"t0".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let t2 = mvcc.begin()?;

        assert_eq!(Some(b"t0".to_vec()), t2.get(b"key")?);
        t1.set(b"key", b"t1".to_vec())?;
        t1.commit()?;
        assert_eq!(Some(b"t0".to_vec()), t2.get(b"key")?);

        Ok(())
    }

    #[test]
    // Read skew is when t1 reads a and b, but t2 modifies b in between the reads.
    fn test_txn_anomaly_read_skew() -> Result<(), Error> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"t0".to_vec())?;
        t0.set(b"b", b"t0".to_vec())?;
        t0.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"t0".to_vec()), t1.get(b"a")?);
        t2.set(b"a", b"t2".to_vec())?;
        t2.set(b"b", b"t2".to_vec())?;
        t2.commit()?;
        assert_eq!(Some(b"t0".to_vec()), t1.get(b"b")?);

        Ok(())
    }

    #[test]
    // A phantom read is when t1 reads entries matching some predicate, but a modification by
    // t2 changes the entries that match the predicate such that a later read by t1 returns them.
    fn test_txn_anomaly_phantom_read() -> Result<(), Error> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"true".to_vec())?;
        t0.set(b"b", b"false".to_vec())?;
        t0.commit()?;

        let t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"true".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"false".to_vec()), t1.get(b"b")?);

        t2.set(b"b", b"true".to_vec())?;
        t2.commit()?;

        assert_eq!(Some(b"true".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"false".to_vec()), t1.get(b"b")?);

        Ok(())
    }

    /* FIXME To avoid write skew we need to implement serializable snapshot isolation.
    #[test]
    // Write skew is when t1 reads b and writes it to a while t2 reads a and writes it to b.¨
    fn test_txn_anomaly_write_skew() -> Result<(), Error> {
        let mvcc = setup();

        let mut t0 = mvcc.begin()?;
        t0.set(b"a", b"1".to_vec())?;
        t0.set(b"b", b"2".to_vec())?;
        t0.commit()?;

        let mut t1 = mvcc.begin()?;
        let mut t2 = mvcc.begin()?;

        assert_eq!(Some(b"1".to_vec()), t1.get(b"a")?);
        assert_eq!(Some(b"2".to_vec()), t2.get(b"b")?);

        // FIXME Some of the following operations should error
        t1.set(b"a", b"2".to_vec())?;
        t2.set(b"b", b"1".to_vec())?;

        t1.commit()?;
        t2.commit()?;

        Ok(())
    }*/

    #[test]
    fn test_metadata() -> Result<(), Error> {
        let mvcc = setup();

        mvcc.set_metadata(b"foo", b"bar".to_vec())?;
        assert_eq!(Some(b"bar".to_vec()), mvcc.get_metadata(b"foo")?);

        assert_eq!(None, mvcc.get_metadata(b"x")?);

        mvcc.set_metadata(b"foo", b"baz".to_vec())?;
        assert_eq!(Some(b"baz".to_vec()), mvcc.get_metadata(b"foo")?);
        Ok(())
    }
}
