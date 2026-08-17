//! Freeing a file's clusters in a way that survives losing power part way.
//!
//! # Why this is not just a delete
//!
//! FAT keeps a file's data in a chain, and each cluster's FAT entry is the
//! pointer to the next one. Freeing a cluster is precisely overwriting that
//! pointer. So freeing a chain by following it — which is what the driver's
//! `truncate_cluster_chain` does, and what `remove_file_reclaiming_clusters`
//! did before this module — cannot be resumed after a reset: the entry naming
//! what came next is the one that was just cleared. Knowing the file's first
//! cluster does not help. That is why this records the cluster numbers before
//! freeing any of them.
//!
//! # The ordering, and what each step buys
//!
//! ```text
//! write the intent, naming the entry and its first batch   <- durable
//! unlink the directory entry                               <- commit point
//! free the clusters this batch names
//! write the next batch to the other slot                   <- durable
//! free those, and so on
//! write the clear record                                   <- durable
//! ```
//!
//! The unlink is the commit point on purpose. Before it, the book is whole
//! and listed; after it, the book is gone and only its space is outstanding.
//! There is no reachable state in which something is *listed and unreadable*,
//! which is what the previous ordering — truncate, then unlink — left behind
//! when it was interrupted, and what made a deleted book come back as a
//! corrupt one.
//!
//! # Why batches, and why two slots
//!
//! A chain can be thousands of clusters; a record naming all of them would
//! not fit in a sector, and a record written across two sectors is a record
//! that can be torn. So a batch is what fits in one sector, and the record
//! also carries the cluster the batch stops before, which is where the next
//! batch starts. Progress costs one write per batch rather than one per
//! cluster.
//!
//! The two slots alternate, each with its own sequence number and checksum,
//! and a batch is only freed once the slot describing it is durable. So a
//! torn slot write leaves the *previous* slot intact and still describing
//! work that is safe to redo — the clusters it names are either allocated or
//! already free, and freeing is idempotent either way.
//!
//! # What this survives, and what it does not
//!
//! Power loss, while this device is the only thing writing to the card. That
//! is the same contract the install journal states, and it is not modesty:
//! FAT counts no references, so once another writer frees a chain its
//! cluster numbers stop identifying anything. A number this journal wrote
//! down may by then belong to a file that writer created.
//!
//! Freeing is therefore only safe because of the order, not because the
//! numbers are checked: nothing is freed until the entry naming it is gone,
//! and under the contract nothing else is allocating in between. The
//! primitive underneath does not establish ownership — it validates that a
//! number addresses a real cluster and clears it. That is what makes it
//! replayable, and it is also why a stale number is dangerous rather than
//! merely useless.
//!
//! Where an outside edit is *recognisable*, this module leans toward not
//! making it worse; see [`Entry::Stranger`] for the one case it can see and
//! what it currently does about it.
//!
//! # Clear is a state, not an absence
//!
//! Finishing does not delete or truncate the journal: doing that would need a
//! crash-safe delete, which is the problem this module exists to solve. Once
//! written, the file stays for the life of the card and "nothing in flight"
//! is a record that says so.
//!
//! It reaches its full size in two steps rather than one, and that is load
//! bearing. The first record is written into a file one slot long; the
//! second slot comes into existence when a second record is first needed.
//! So a file shorter than one slot can only be a bootstrap that never
//! completed -- which, by the ordering above, is a transaction that never
//! touched anything. Creating the file at full size instead would make an
//! interrupted first write indistinguishable from a journal whose records
//! have both become unreadable, and the safe answer to that second case is
//! to refuse.

use embedded_sdmmc::{ClusterId, Mode, TimeSource};
use heapless::Vec;

use crate::install::{fnv1a, Dir, ShortName};
use crate::open_or_make_dir;
use proto::cache::CACHE_ROOT_DIR;

/// The journal, under the cache root beside `INSTALL.JNL`.
pub const JOURNAL_FILE: &str = "RECLAIM.JNL";

/// One slot, sized to a sector so a write cannot be torn across two.
pub const SLOT_BYTES: usize = 512;
/// Two of them, alternating.
pub const SLOT_COUNT: usize = 2;
pub const JOURNAL_BYTES: usize = SLOT_BYTES * SLOT_COUNT;

/// The envelope: magic, version, sequence number and checksum, at fixed
/// offsets, covering the whole slot.
///
/// These four may never move or change meaning, whatever else a later
/// version does with the rest of the slot. They are what lets this build
/// recognise a record it cannot *act on* as nonetheless whole and later than
/// its own — which is the difference between waiting for a build that
/// understands it and overwriting a reclaim that is still outstanding. A
/// version that needed a different envelope would need a different file.
const MAGIC: &[u8; 4] = b"CRJ1";
const VERSION: u16 = 1;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_FLAGS: usize = 6;
const OFF_COUNT: usize = 7;
const OFF_SEQ: usize = 8;
const OFF_CONTINUATION: usize = 12;
const OFF_PLACE: usize = 16;
const OFF_NAME: usize = 17;
const OFF_ENTRY_CLUSTER: usize = 32;
const OFF_CLUSTERS: usize = 36;
const OFF_CRC: usize = SLOT_BYTES - 4;

/// How many cluster numbers one slot carries: 118, which is one progress
/// write per 118 clusters freed rather than one per cluster.
pub const MAX_BATCH: usize = (OFF_CRC - OFF_CLUSTERS) / 4;
// Stated as a number as well as a formula, so a layout change that quietly
// shrinks the batch has to be noticed and re-argued rather than absorbed.
const _: () = assert!(MAX_BATCH == 118);

// The fields may not run into each other, and the batch may not run into the
// checksum.
const _: () = assert!(OFF_NAME + 12 <= OFF_ENTRY_CLUSTER);
const _: () = assert!(OFF_ENTRY_CLUSTER + 4 <= OFF_CLUSTERS);
const _: () = assert!(OFF_CLUSTERS + MAX_BATCH * 4 <= OFF_CRC);

/// Nothing is in flight. The transaction that wrote it is finished.
const FLAG_CLEAR: u8 = 1 << 0;

/// Which directory the entry being reclaimed stood in.
///
/// A number rather than a path: the record has to survive a reset and be
/// acted on by a build that may have been upgraded in between, and these are
/// the only three places this transaction is used from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `/BOOKS` — a book the reader deleted.
    Books = 0,
    /// The card root — the OTA trigger.
    Root = 1,
    /// `/READER/ROLLBACK` — a predecessor an install has finished with.
    Rollback = 2,
}

impl Place {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Books),
            1 => Some(Self::Root),
            2 => Some(Self::Rollback),
            _ => None,
        }
    }
}

/// One batch of a reclaim, as it sits in a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Higher is later. The live slot is whichever holds the larger one.
    pub seq: u32,
    /// Where the entry stood.
    pub place: Place,
    /// The 8.3 name of the entry to unlink, empty once it is known gone.
    pub name: ShortName,
    /// The first cluster that entry pointed at when the record was written.
    ///
    /// Checked before unlinking on a replay: a name is not proof. If some
    /// other file has taken this name since, its entry points somewhere else
    /// and must be left alone.
    pub entry_cluster: u32,
    /// The clusters this batch frees.
    pub clusters: Vec<u32, MAX_BATCH>,
    /// Where the next batch starts, or 0 when this is the last one.
    pub continuation: u32,
}

impl Batch {
    /// Serialise into one slot. Trailing bytes are zero, so a record reads
    /// the same whichever slot it lands in and whatever was there before.
    pub fn encode(&self) -> [u8; SLOT_BYTES] {
        let mut out = [0u8; SLOT_BYTES];
        out[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(MAGIC);
        out[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&VERSION.to_le_bytes());
        out[OFF_FLAGS] = 0;
        out[OFF_COUNT] = self.clusters.len() as u8;
        out[OFF_SEQ..OFF_SEQ + 4].copy_from_slice(&self.seq.to_le_bytes());
        out[OFF_CONTINUATION..OFF_CONTINUATION + 4]
            .copy_from_slice(&self.continuation.to_le_bytes());
        out[OFF_PLACE] = self.place as u8;
        let name = self.name.as_bytes();
        out[OFF_NAME..OFF_NAME + name.len()].copy_from_slice(name);
        out[OFF_ENTRY_CLUSTER..OFF_ENTRY_CLUSTER + 4]
            .copy_from_slice(&self.entry_cluster.to_le_bytes());
        for (index, cluster) in self.clusters.iter().enumerate() {
            let at = OFF_CLUSTERS + index * 4;
            out[at..at + 4].copy_from_slice(&cluster.to_le_bytes());
        }
        let crc = fnv1a(&out[..OFF_CRC]);
        out[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// The record saying nothing is in flight, which is how a finished
    /// transaction is recorded. Carries a sequence number like any other, so
    /// it can be told from the record it supersedes.
    pub fn clear(seq: u32) -> [u8; SLOT_BYTES] {
        let mut out = [0u8; SLOT_BYTES];
        out[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(MAGIC);
        out[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&VERSION.to_le_bytes());
        out[OFF_FLAGS] = FLAG_CLEAR;
        out[OFF_SEQ..OFF_SEQ + 4].copy_from_slice(&seq.to_le_bytes());
        let crc = fnv1a(&out[..OFF_CRC]);
        out[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }
}

/// What one slot holds.
// A batch is a sector's worth of cluster numbers, so the `Work` variant is
// inherently ~480 bytes larger than the others. There is no allocator here to
// box it into, and the list has to be in memory to be acted on -- reading it
// back per cluster would trade the space for a card read each time.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Not a whole record: never written, or torn part way through one.
    ///
    /// Falling back to the other slot over one of these is safe, and is what
    /// the second slot is for. A batch is freed only once the slot
    /// describing it is durable, so a slot that never became whole describes
    /// work that was never begun.
    Torn,
    /// A whole record this build cannot act on.
    ///
    /// The checksum is good, so these bytes are exactly what some build
    /// meant to write -- it is only the meaning that is out of reach: a
    /// version this one does not know, or a field value it does not
    /// understand. Falling back over one of these is *not* safe. It may be
    /// the later record, written by a build that has already begun freeing
    /// the batch it describes, and the older slot would then send recovery
    /// down a chain that has since been taken apart. The whole journal is
    /// refused instead.
    Unsupported,
    /// A record saying nothing is in flight.
    Clear { seq: u32 },
    /// A record describing work.
    Work(Batch),
}

impl Slot {
    /// The sequence number, for choosing between slots.
    fn seq(&self) -> Option<u32> {
        match self {
            // Neither orders against anything: a torn slot has no record to
            // order, and an unsupported one is never chosen between -- it
            // refuses the journal outright.
            Self::Torn | Self::Unsupported => None,
            Self::Clear { seq } => Some(*seq),
            Self::Work(batch) => Some(batch.seq),
        }
    }

    /// Whether this slot stops the journal being acted on at all.
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }
}

/// Read one slot's bytes.
pub fn decode_slot(bytes: &[u8]) -> Slot {
    // Order matters here, and it is the point of the function.
    //
    // The checksum is asked *before* the version, so that a whole record
    // from a build this one does not know is recognised as whole. Asking the
    // version first makes such a record indistinguishable from a torn one,
    // and the other slot is then fallen back on -- possibly past a durable
    // record whose batch is already part freed.
    if bytes.len() < SLOT_BYTES || &bytes[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
        return Slot::Torn;
    }
    let stored = u32::from_le_bytes([
        bytes[OFF_CRC],
        bytes[OFF_CRC + 1],
        bytes[OFF_CRC + 2],
        bytes[OFF_CRC + 3],
    ]);
    if stored != fnv1a(&bytes[..OFF_CRC]) {
        return Slot::Torn;
    }
    // Whole, from here on. Anything this build cannot make sense of is a
    // record it must wait for rather than one it may step over.
    let version = u16::from_le_bytes([bytes[OFF_VERSION], bytes[OFF_VERSION + 1]]);
    if version != VERSION {
        return Slot::Unsupported;
    }
    let seq = u32::from_le_bytes([
        bytes[OFF_SEQ],
        bytes[OFF_SEQ + 1],
        bytes[OFF_SEQ + 2],
        bytes[OFF_SEQ + 3],
    ]);
    if bytes[OFF_FLAGS] & FLAG_CLEAR != 0 {
        return Slot::Clear { seq };
    }
    let count = bytes[OFF_COUNT] as usize;
    if count > MAX_BATCH {
        // A whole record whose batch does not fit this build's slot layout:
        // the same version number over a different shape.
        return Slot::Unsupported;
    }
    let Some(place) = Place::from_byte(bytes[OFF_PLACE]) else {
        // A place a later build reclaims from and this one does not know.
        return Slot::Unsupported;
    };
    let mut name = ShortName::new();
    for byte in &bytes[OFF_NAME..OFF_NAME + 12] {
        if *byte == 0 {
            break;
        }
        if name.push(*byte as char).is_err() {
            return Slot::Unsupported;
        }
    }
    let mut clusters = Vec::new();
    for index in 0..count {
        let at = OFF_CLUSTERS + index * 4;
        let cluster = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        if clusters.push(cluster).is_err() {
            return Slot::Unsupported;
        }
    }
    Slot::Work(Batch {
        seq,
        place,
        name,
        entry_cluster: u32::from_le_bytes([
            bytes[OFF_ENTRY_CLUSTER],
            bytes[OFF_ENTRY_CLUSTER + 1],
            bytes[OFF_ENTRY_CLUSTER + 2],
            bytes[OFF_ENTRY_CLUSTER + 3],
        ]),
        clusters,
        continuation: u32::from_le_bytes([
            bytes[OFF_CONTINUATION],
            bytes[OFF_CONTINUATION + 1],
            bytes[OFF_CONTINUATION + 2],
            bytes[OFF_CONTINUATION + 3],
        ]),
    })
}

/// Whether these slots can be acted on at all.
///
/// One whole record this build cannot read refuses the journal, even when
/// the other slot decodes perfectly. The readable one may be the *older*
/// record, and the unreadable one a later build's, whose batch is already
/// part freed — acting on the older record would then walk a chain that has
/// been taken apart. This is the case a fallback must not cover, and the
/// reason a torn slot and an unsupported one are different things.
pub fn slots_are_usable(slots: &[Slot; SLOT_COUNT]) -> bool {
    !slots.iter().any(Slot::is_unsupported)
}

/// What the card has to say about reclamation.
// Carries a `Live`, and so a `Slot`: same sector-sized batch, same reason as
// there for not boxing it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Journal {
    /// Nothing to finish, and nothing to be careful of.
    ///
    /// No journal file at all, or one that never got a whole first record
    /// written. Those are the same answer because of the ordering rule: the
    /// first record is durable before anything is unlinked or freed, so a
    /// journal without one describes a transaction that never began. The
    /// bootstrap is simply started again.
    ///
    /// This is why the first record matters more than it looks. Refusing
    /// here instead -- the safe-seeming answer -- would strand a card on the
    /// one transaction that provably did nothing.
    Absent,
    /// There is a journal, and this build cannot safely act on it.
    ///
    /// Two ways to arrive here, and the first one turns on the file's
    /// length. A journal that has reached its permanent two-slot size and
    /// holds no whole record in either -- it carried records once, and what
    /// they said is now unreadable. A shorter journal with nothing readable
    /// is [`Journal::Absent`] instead, because the only record it could ever
    /// have held is a first one, and a first record that did not survive
    /// describes a transaction that never began.
    ///
    /// Or one slot *is* whole and this build cannot read it
    /// ([`Slot::Unsupported`]), in which case the other slot is refused too,
    /// however well it decodes, because the one out of reach may be the
    /// later of the two. That answer does not depend on length: a whole
    /// record is a whole record even when it is the only one.
    ///
    /// Not the same as nothing in flight, and the difference is the reason
    /// the format is versioned. Another build may have unlinked a book and
    /// got part way through freeing its chain; the numbers it wrote down are
    /// the only thing that can still find the rest, and this build cannot
    /// read them. Treating that as idle would abandon the reclaim and leak
    /// the chain for the life of the card. The caller refuses to mutate
    /// instead -- the same answer `INSTALL.JNL` gives to a record it cannot
    /// read, for the same reason.
    Unrecognized,
    /// A record to act on.
    Found(Live),
}

/// Which of the two slots is the live one, and which is therefore next to be
/// written.
///
/// The live slot is the readable one with the later sequence number. A torn
/// write leaves one slot unreadable and the other holding the record it was
/// superseding, which is exactly the record that is still safe to act on.
///
/// With neither readable this names no slot, and what that means is not
/// decided here: it depends on how long the journal is, which these bytes
/// do not say. [`read_journal`] settles it — a bootstrap that never
/// completed, or a journal whose history has become unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    /// The record to act on.
    pub slot: Slot,
    /// Which slot index it came from, or `None` when neither is readable.
    pub index: Option<usize>,
}

impl Live {
    /// The slot the next record goes in: the other one, so a torn write
    /// cannot take the record it supersedes with it.
    pub fn next_index(&self) -> usize {
        match self.index {
            Some(index) => (index + 1) % SLOT_COUNT,
            None => 0,
        }
    }

    /// The sequence number the next record carries.
    pub fn next_seq(&self) -> u32 {
        self.slot.seq().map_or(1, |seq| seq.wrapping_add(1))
    }
}

/// Choose between the slots.
pub fn live_slot(slots: [Slot; SLOT_COUNT]) -> Live {
    let [first, second] = slots;
    match (first.seq(), second.seq()) {
        (Some(a), Some(b)) => {
            // Wrapping comparison, so a journal that has been written four
            // billion times does not suddenly prefer the older record.
            if b.wrapping_sub(a) < a.wrapping_sub(b) {
                Live {
                    slot: second,
                    index: Some(1),
                }
            } else {
                Live {
                    slot: first,
                    index: Some(0),
                }
            }
        }
        (Some(_), None) => Live {
            slot: first,
            index: Some(0),
        },
        (None, Some(_)) => Live {
            slot: second,
            index: Some(1),
        },
        (None, None) => Live {
            slot: Slot::Torn,
            index: None,
        },
    }
}

/// Read both slots off the card.
///
/// Three answers, and keeping them apart is what makes replay safe. Two of
/// them are ways of not answering, and they are not interchangeable.
///
/// A slot that reads and fails magic or checksum is [`Slot::Torn`]: it never
/// became a whole record. That is the case the second slot exists for, and
/// falling back to the other one is correct — a batch is freed only once the
/// slot describing it is durable, so a slot that never became whole
/// describes work that was never begun.
///
/// A slot that reads and checksums but cannot be understood is
/// [`Slot::Unsupported`], and falling back over it is *not* allowed. It is a
/// whole record, so some build meant to write it, and it may be the later of
/// the two with its batch already part freed. The journal is refused
/// entirely; see [`slots_are_usable`].
///
/// A slot that will not read *at all* is a third thing again. The card has
/// stopped answering, so nothing is known about that slot — including
/// whether it was durable. Falling back to the older record could hand
/// recovery a continuation whose chain has since been taken apart, which is
/// the lost-topology problem this journal exists to prevent. So a seek or
/// read failure stops the pass and is retried at the next mount, where the
/// card may answer.
pub fn read_journal<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
) -> Result<Journal, ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    // Opened, never created. Reading to find out whether a reclaim is
    // outstanding must not write to the card -- and a card that will not
    // answer an open is not a card to respond to with a metadata write.
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(Journal::Absent),
        Err(_) => return Err(ReclaimError::Card),
    };
    let file = match cache_root.open_file_in_dir(JOURNAL_FILE, Mode::ReadOnly) {
        Ok(file) => file,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(Journal::Absent),
        Err(_) => return Err(ReclaimError::Card),
    };
    // Not even one whole slot. The first record never became durable, and
    // nothing is mutated until it does, so there is nothing here to finish
    // and nothing to be careful of -- see `Journal::Absent`.
    let length = file.length();
    if length < SLOT_BYTES as u32 {
        let _ = file.close();
        return Ok(Journal::Absent);
    }
    // The second slot exists only once a second record has been needed. Its
    // absence is not a torn write, but it reads the same way here, and the
    // difference is settled below by the file's length.
    let has_second_slot = length >= JOURNAL_BYTES as u32;
    let mut slots = [Slot::Torn, Slot::Torn];
    let mut buffer = [0u8; SLOT_BYTES];
    let mut unreadable_card = false;
    for (index, slot) in slots.iter_mut().enumerate() {
        if index == 1 && !has_second_slot {
            break;
        }
        let read = file
            .seek_from_start((index * SLOT_BYTES) as u32)
            .map_err(|_| ())
            .and_then(|()| read_exact(&file, &mut buffer));
        if read.is_err() {
            unreadable_card = true;
            break;
        }
        *slot = decode_slot(&buffer);
    }
    if file.close().is_err() || unreadable_card {
        return Err(ReclaimError::Card);
    }
    // A whole record this build cannot act on refuses the journal even when
    // the other slot decodes, because the one it cannot read may be the
    // later of the two.
    if !slots_are_usable(&slots) {
        return Ok(Journal::Unrecognized);
    }
    let live = live_slot(slots);
    match live.index {
        Some(_) => Ok(Journal::Found(live)),
        // Nothing readable. Which answer that is depends on whether this
        // journal ever got as far as two slots.
        //
        // If it did not, the only record it could hold is a first one, and a
        // first record that did not survive is a bootstrap that never
        // reached the point where anything is mutated. Starting again is
        // safe, and refusing instead would strand a card on a transaction
        // that never began.
        //
        // If it did, the file is never shortened again, so two unreadable
        // slots mean records were written and are now unreadable -- which
        // may be a reclaim part way through, and is refused.
        None if !has_second_slot => Ok(Journal::Absent),
        None => Ok(Journal::Unrecognized),
    }
}

fn read_exact<D, T, const MD: usize, const MF: usize, const MV: usize>(
    file: &embedded_sdmmc::File<'_, D, T, MD, MF, MV>,
    out: &mut [u8],
) -> Result<(), ()>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut at = 0;
    while at < out.len() {
        match file.read(&mut out[at..]) {
            Ok(0) => return Err(()),
            Ok(read) => at += read,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

/// What the card shows of the entry a record names.
///
/// Only two things are asked about it, and both are needed. Whether an entry
/// stands under that name at all, and whether it is the same file -- the
/// chain it points at. A name on its own is not proof: a freed 8.3 alias
/// goes to whatever is written next, so a name that has come back may belong
/// to a stranger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    /// No entry under that name. Either this reclaim already unlinked it, or
    /// something else did.
    Gone,
    /// An entry under that name, pointing where the record says.
    Ours,
    /// An entry under that name, pointing somewhere else.
    ///
    /// Not this file, and not this transaction's to remove. Under the
    /// module's sole-writer contract this is barely reachable — a name is
    /// only reused after its entry is gone, and nothing else is writing —
    /// so it means what it says: the reclaim already unlinked, and a later
    /// upload has been handed the freed alias.
    ///
    /// Outside that contract it is the one sign of interference this
    /// transaction can actually see, and then it is ambiguous. Another
    /// writer may have deleted the original entry — freeing its chain — and
    /// allocated some of those clusters to files of its own before taking
    /// the name. Freeing this record's numbers would then clear a live
    /// file's data.
    ///
    /// The reclaim continues anyway today, which is correct under the
    /// contract and a hazard outside it. Stopping the pass instead was
    /// considered and is not obviously better: a stop that is not itself
    /// recorded is undone by the next boot, where the stranger may be gone
    /// and the same numbers freed against a card that has moved on again;
    /// and a stop that *is* recorded needs a way for the reader to resolve
    /// it, or an outside edit wedges every future upload and delete. That
    /// is a decision about what the device does to a person's card, not a
    /// detail of this transaction, and it is left open deliberately rather
    /// than settled here by default.
    Stranger,
}

/// What to do next, given a record and what the card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Take the name away. The commit point: before it the file is whole and
    /// listed, after it the file is gone and only its space is outstanding.
    /// Never touches clusters.
    Unlink,
    /// Free the clusters this record names. Idempotent, so a replay that
    /// cannot tell how far the last attempt got simply runs the batch again.
    FreeBatch,
    /// Read the next batch off the chain and record it, in the other slot.
    /// Only after that record is durable may those clusters be freed.
    RecordNextBatch { from: u32 },
    /// Nothing outstanding; write the clear record.
    Finish,
}

/// The next step towards finishing the reclaim `batch` describes.
///
/// Total by construction, like the install planner and for the same reason:
/// a recovery that meets a state it has no rule for stalls on exactly the
/// crash it exists to handle.
///
/// Unlike that planner, one of the inputs is not readable off the card.
/// Every install step changes something observable — a name appears or goes
/// — so the planner can ask the card where it got to. Freeing a batch
/// changes nothing a cheap read can see: the clusters are detached either
/// way, and asking the FAT about each in turn is the work itself. So
/// `batch_freed` is carried by the pass rather than observed, and starts
/// false after a reset. That costs a batch re-freed on the first pass after
/// a cut, which is exactly the idempotence the primitive was built for.
pub fn plan(batch: &Batch, entry: Entry, batch_freed: bool) -> Step {
    // The name goes first, always. A name outliving its data is the failure
    // this transaction exists to prevent, and it is the only part of any of
    // this that a reader can see. A name already gone is a step already
    // taken; a name now worn by a stranger is not this transaction's to
    // remove -- and freeing continues past it only under the module's
    // sole-writer contract, which is where that name can only have been
    // reused after this record's chain was detached. See [`Entry::Stranger`]
    // for what is and is not true outside it.
    if entry == Entry::Ours {
        return Step::Unlink;
    }
    if !batch_freed && !batch.clusters.is_empty() {
        return Step::FreeBatch;
    }
    if batch.continuation != 0 {
        return Step::RecordNextBatch {
            from: batch.continuation,
        };
    }
    Step::Finish
}

/// The directory a record's [`Place`] names.
///
/// Opened from the root every time rather than carried, so recovery needs
/// only the handle it already has at mount.
fn open_place<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'a, D, T, MD, MF, MV>,
    books: Option<&Dir<'a, D, T, MD, MF, MV>>,
    place: Place,
) -> Result<Dir<'a, D, T, MD, MF, MV>, ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match place {
        // Both handed in rather than opened by name: the shelf's directory
        // name belongs to the firmware, which already holds both handles
        // wherever recovery runs. `.` reopens a handle onto the same
        // directory, since these are consumed by value.
        Place::Root => root.open_dir(".").map_err(|_| ReclaimError::Card),
        // No shelf and a record naming one is a refusal, never an
        // assumption that the work is done. The record's clusters are
        // detached and its numbers carry no ownership, so proceeding as if
        // it had finished would leave them free for the next allocation
        // while the record still says to free them.
        Place::Books => books
            .ok_or(ReclaimError::ShelfMissing)?
            .open_dir(".")
            .map_err(|_| ReclaimError::Card),
        Place::Rollback => {
            let cache_root = root
                .open_dir(CACHE_ROOT_DIR)
                .map_err(|_| ReclaimError::Card)?;
            cache_root
                .open_dir(crate::install::ROLLBACK_DIR)
                .map_err(|_| ReclaimError::Card)
        }
    }
}

/// What the card shows of the entry a record names.
fn classify<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Dir<'_, D, T, MD, MF, MV>,
    name: &str,
    entry_cluster: u32,
) -> Result<Entry, ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    match dir.find_directory_entry(name) {
        Ok(entry) => Ok(if entry.cluster.value() == entry_cluster {
            Entry::Ours
        } else {
            Entry::Stranger
        }),
        Err(embedded_sdmmc::Error::NotFound) => Ok(Entry::Gone),
        Err(_) => Err(ReclaimError::Card),
    }
}

/// Read up to one slot's worth of a chain, starting at `from`.
///
/// Read-only, and that is the point of doing it before anything is written:
/// losing this to a reset costs nothing but the walk, whereas a record that
/// named topology nobody had confirmed would be a licence to free clusters
/// on trust.
fn walk_batch<D, T, const MD: usize, const MF: usize, const MV: usize>(
    dir: &Dir<'_, D, T, MD, MF, MV>,
    from: u32,
) -> Result<(Vec<u32, MAX_BATCH>, u32), ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let mut clusters: Vec<u32, MAX_BATCH> = Vec::new();
    let mut at = ClusterId::new(from);
    loop {
        if clusters.push(at.value()).is_err() {
            // Full. `at` is the cluster this batch stops before, and where
            // the next one starts.
            return Ok((clusters, at.value()));
        }
        match dir.next_cluster_in_chain(at) {
            Ok(Some(next)) => at = next,
            Ok(None) => return Ok((clusters, 0)),
            // A chain that has already been taken apart, or a number that
            // is not a cluster on this volume. Either way this walk cannot
            // be completed, and a partial one must not be published.
            Err(_) => return Err(ReclaimError::Card),
        }
    }
}

/// Write one slot and make it durable.
///
/// Opened append-or-create and seeked, never truncated: an existing journal
/// keeps the record this write is superseding until this one lands.
fn write_slot<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'_, D, T, MD, MF, MV>,
    index: usize,
    bytes: &[u8; SLOT_BYTES],
) -> Result<(), ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = open_or_make_dir(root, CACHE_ROOT_DIR).map_err(|()| ReclaimError::Card)?;
    let file = cache_root
        .open_file_in_dir(JOURNAL_FILE, Mode::ReadWriteCreateOrAppend)
        .map_err(|_| ReclaimError::Card)?;
    let wrote = file
        .seek_from_start((index * SLOT_BYTES) as u32)
        .map_err(|_| ReclaimError::Card)
        .and_then(|()| file.write(bytes).map_err(|_| ReclaimError::Card));
    // The close is what makes it durable, so its failure is the write's.
    let closed = file.close();
    wrote?;
    closed.map_err(|_| ReclaimError::Card)
}

/// How many steps one pass may take before it is called stuck.
///
/// Each step either unlinks, frees the current batch, or advances to the
/// next one, so a pass terminates on any chain the walk can complete. The
/// bound is for a record that does not describe one -- a continuation that
/// leads back into itself, say -- so a wedged card is a refused pass rather
/// than a device that never finishes mounting.
const MAX_STEPS: usize = 4096;

/// Carry whatever the journal describes to completion.
///
/// Safe to call when there is nothing outstanding, which is what makes it a
/// mount-time pass: an absent or cleared journal touches nothing.
///
/// `Ok(true)` means a live record was found and replayed -- a reclaim really
/// was interrupted, and this pass finished it. That is the difference
/// between a transaction that was cut and one whose reply merely went
/// missing, and a caller proving the journal works needs to tell them
/// apart.
pub fn recover<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'a, D, T, MD, MF, MV>,
    books: Option<&Dir<'a, D, T, MD, MF, MV>>,
) -> Result<bool, ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let live = match read_journal(root)? {
        Journal::Absent => return Ok(false),
        Journal::Unrecognized => return Err(ReclaimError::Unrecognized),
        Journal::Found(live) => live,
    };
    // Taken before the record is moved out of it.
    let mut next_index = live.next_index();
    let mut next_seq = live.next_seq();
    let mut batch = match live.slot {
        Slot::Clear { .. } => return Ok(false),
        Slot::Work(batch) => batch,
        // `read_journal` hands back neither of these.
        Slot::Torn | Slot::Unsupported => return Err(ReclaimError::Unrecognized),
    };
    // False after a reset, so the first pass re-frees the batch it finds.
    // That is what the idempotent free is for.
    let mut batch_freed = false;

    for _ in 0..MAX_STEPS {
        let dir = open_place(root, books, batch.place)?;
        let entry = classify(&dir, batch.name.as_str(), batch.entry_cluster)?;
        match plan(&batch, entry, batch_freed) {
            Step::Unlink => {
                dir.delete_entry_in_dir(batch.name.as_str())
                    .map_err(|_| ReclaimError::Card)?;
            }
            Step::FreeBatch => {
                for cluster in &batch.clusters {
                    dir.free_cluster(ClusterId::new(*cluster))
                        .map_err(|_| ReclaimError::Card)?;
                }
                batch_freed = true;
            }
            Step::RecordNextBatch { from } => {
                // Walked in full before it is written, and written in full
                // before any of it is freed.
                let (clusters, continuation) = walk_batch(&dir, from)?;
                let next = Batch {
                    seq: next_seq,
                    place: batch.place,
                    name: batch.name.clone(),
                    entry_cluster: batch.entry_cluster,
                    clusters,
                    continuation,
                };
                write_slot(root, next_index, &next.encode())?;
                next_index = (next_index + 1) % SLOT_COUNT;
                next_seq = next_seq.wrapping_add(1);
                batch = next;
                batch_freed = false;
            }
            Step::Finish => {
                write_slot(root, next_index, &Batch::clear(next_seq))?;
                return Ok(true);
            }
        }
    }
    Err(ReclaimError::Card)
}

/// Take a file's name away and free its clusters, so that a reset leaves it
/// either wholly there or wholly gone.
///
/// The name is not removed until a record naming its chain is durable, and
/// no cluster is freed until the name is gone. See the module docs for what
/// each of those buys.
pub fn reclaim_entry<'a, D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Dir<'a, D, T, MD, MF, MV>,
    books: Option<&Dir<'a, D, T, MD, MF, MV>>,
    place: Place,
    name: &str,
) -> Result<(), ReclaimError>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let dir = open_place(root, books, place)?;
    let entry = match dir.find_directory_entry(name) {
        Ok(entry) => entry,
        // Nothing to do, and not an error: a delete of something already
        // gone is the state the caller wanted.
        Err(embedded_sdmmc::Error::NotFound) => return Ok(()),
        Err(_) => return Err(ReclaimError::Card),
    };
    let first = entry.cluster.value();
    let mut stored = ShortName::new();
    if stored.push_str(name).is_err() {
        return Err(ReclaimError::Card);
    }

    // The journal's state decides everything, and is asked before anything
    // is touched -- including for an entry with no clusters, which still
    // waits its turn behind an outstanding reclaim and still refuses to
    // proceed past a journal this build cannot read.
    let (index, seq) = match read_journal(root)? {
        Journal::Absent => {
            // First use on this card, and the one place the journal can
            // still grow. Bring it to its permanent two slots *now*, while
            // the chain about to be reclaimed is still allocated.
            //
            // Otherwise the second slot comes into existence later, after
            // that chain has been freed -- and the allocator would be
            // choosing from clusters this transaction just released, with
            // its own free-cluster hint pointing straight at them. A cut
            // before that slot became a whole record would then leave the
            // reader correctly falling back to slot 0, whose batch is
            // replayed, freeing a cluster the journal itself is now living
            // in.
            //
            // A `Clear` first, because the file has to reach full size
            // through a record that means "nothing in flight": a cut while
            // writing it leaves a journal that never began a transaction,
            // and the target untouched.
            write_slot(root, 0, &Batch::clear(1))?;
            (1, 2)
        }
        Journal::Unrecognized => return Err(ReclaimError::Unrecognized),
        Journal::Found(live) => match live.slot {
            Slot::Clear { .. } => (live.next_index(), live.next_seq()),
            // Another reclaim is outstanding. One at a time: the caller
            // finishes that one first.
            _ => return Err(ReclaimError::Busy),
        },
    };

    // Resolve first, write second. A walk lost to a reset costs only the
    // walk; a durable record is something recovery is entitled to act on.
    // An entry pointing at no chain has nothing to walk and still gets a
    // record, so its unlink is serialised like every other.
    let (clusters, continuation) = if first == 0 {
        (Vec::new(), 0)
    } else {
        walk_batch(&dir, first)?
    };
    let record = Batch {
        seq,
        place,
        name: stored,
        entry_cluster: first,
        clusters,
        continuation,
    };
    write_slot(root, index, &record.encode())?;
    recover(root, books).map(|_| ())
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimError {
    /// The card would not answer. Retryable, and retried at the next mount.
    Card,
    /// A record names the shelf and the caller has no shelf to give. Not
    /// retryable by repetition; the caller opens `/BOOKS` and tries again.
    ShelfMissing,
    /// The journal holds something this build cannot act on. Not retryable
    /// by repetition, and the caller refuses to mutate the shelf until it
    /// is resolved -- see [`Journal::Unrecognized`].
    Unrecognized,
    /// A reclaim is already outstanding. One at a time, so the journal never
    /// has to describe two.
    Busy,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(seq: u32, clusters: &[u32], continuation: u32) -> Batch {
        let mut name = ShortName::new();
        name.push_str("BOOK~1.EPU").unwrap();
        Batch {
            seq,
            place: Place::Books,
            name,
            entry_cluster: clusters.first().copied().unwrap_or(0),
            clusters: Vec::from_slice(clusters).unwrap(),
            continuation,
        }
    }

    #[test]
    fn the_name_goes_before_anything_else() {
        // The ordering the whole transaction turns on. Whatever else is
        // outstanding, an entry still pointing at this file is unlinked
        // first, so there is no reachable moment where a listed book has had
        // its clusters taken.
        let batch = batch(1, &[10, 11], 12);
        assert_eq!(plan(&batch, Entry::Ours, false), Step::Unlink);
        assert_eq!(plan(&batch, Entry::Ours, true), Step::Unlink);
    }

    #[test]
    fn a_stranger_wearing_the_name_is_left_alone() {
        // A freed 8.3 alias goes to whatever is written next. Under the
        // module's sole-writer contract that can only have happened after
        // this record's chain was detached, so freeing carries on -- but the
        // entry itself is never this transaction's to remove.
        let batch = batch(1, &[10, 11], 0);
        assert_eq!(plan(&batch, Entry::Stranger, false), Step::FreeBatch);
        assert_eq!(plan(&batch, Entry::Stranger, true), Step::Finish);
    }

    #[test]
    fn the_batch_is_freed_then_the_next_one_recorded() {
        let batch = batch(1, &[10, 11], 12);
        assert_eq!(plan(&batch, Entry::Gone, false), Step::FreeBatch);
        assert_eq!(
            plan(&batch, Entry::Gone, true),
            Step::RecordNextBatch { from: 12 }
        );
    }

    #[test]
    fn a_last_batch_finishes_rather_than_recording_another() {
        let batch = batch(1, &[10, 11], 0);
        assert_eq!(plan(&batch, Entry::Gone, true), Step::Finish);
    }

    #[test]
    fn an_empty_batch_still_follows_its_continuation() {
        // Reachable when a chain's last batch lands exactly on the boundary.
        let batch = batch(1, &[], 40);
        assert_eq!(
            plan(&batch, Entry::Gone, false),
            Step::RecordNextBatch { from: 40 }
        );
    }

    #[test]
    fn a_record_with_nothing_left_is_finished() {
        assert_eq!(plan(&batch(1, &[], 0), Entry::Gone, false), Step::Finish);
    }

    #[test]
    fn a_batch_survives_the_round_trip() {
        let original = batch(7, &[3, 4, 5, 900], 901);
        assert_eq!(decode_slot(&original.encode()), Slot::Work(original));
    }

    #[test]
    fn a_full_batch_fits_its_slot() {
        let clusters: heapless::Vec<u32, MAX_BATCH> = (2..2 + MAX_BATCH as u32).collect();
        let full = Batch {
            clusters,
            ..batch(1, &[], 0)
        };
        assert_eq!(decode_slot(&full.encode()), Slot::Work(full));
    }

    #[test]
    fn a_clear_record_is_a_record() {
        // Finishing writes a record rather than removing the file, because
        // removing it would need the very thing this module provides.
        assert_eq!(decode_slot(&Batch::clear(9)), Slot::Clear { seq: 9 });
    }

    #[test]
    fn a_torn_slot_does_not_read_as_work() {
        let mut bytes = batch(3, &[10, 11], 12).encode();
        bytes[40] ^= 0xFF;
        assert_eq!(decode_slot(&bytes), Slot::Torn);
    }

    #[test]
    fn a_whole_slot_from_another_version_is_unsupported_not_torn() {
        // Whole: the checksum is recomputed, so these bytes are exactly what
        // that build meant to write. This build cannot act on them, but it
        // must not mistake them for a slot that was never finished.
        let mut bytes = batch(3, &[10, 11], 12).encode();
        bytes[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&2u16.to_le_bytes());
        let crc = fnv1a(&bytes[..OFF_CRC]);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(decode_slot(&bytes), Slot::Unsupported);
    }

    #[test]
    fn a_version_bump_with_a_broken_checksum_is_just_torn() {
        // The other half of the rule: an unfamiliar version is only worth
        // waiting for when the record around it is whole.
        let mut bytes = batch(3, &[10, 11], 12).encode();
        bytes[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(decode_slot(&bytes), Slot::Torn);
    }

    #[test]
    fn zeroed_bytes_are_not_a_record() {
        // What a freshly allocated journal file holds.
        assert_eq!(decode_slot(&[0u8; SLOT_BYTES]), Slot::Torn);
    }

    #[test]
    fn a_count_past_the_slot_is_refused() {
        let mut bytes = batch(3, &[10, 11], 12).encode();
        bytes[OFF_COUNT] = (MAX_BATCH + 1) as u8;
        let crc = fnv1a(&bytes[..OFF_CRC]);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        // Whole, and shaped for a slot layout this build does not have.
        assert_eq!(decode_slot(&bytes), Slot::Unsupported);
    }

    #[test]
    fn the_later_record_is_the_live_one() {
        let older = decode_slot(&batch(4, &[10], 0).encode());
        let newer = decode_slot(&batch(5, &[11], 0).encode());
        assert_eq!(live_slot([older.clone(), newer.clone()]).index, Some(1));
        assert_eq!(live_slot([newer, older]).index, Some(0));
    }

    #[test]
    fn a_torn_write_leaves_the_record_it_superseded() {
        // The reason for two slots. A batch is only freed once the slot
        // describing it is durable, so the surviving record always describes
        // work that is safe to redo.
        let standing = decode_slot(&batch(4, &[10, 11], 12).encode());
        let live = live_slot([standing.clone(), Slot::Torn]);
        assert_eq!(live.slot, standing);
        assert_eq!(live.index, Some(0));
        // And the next write goes to the other slot, not over the survivor.
        assert_eq!(live.next_index(), 1);
        assert_eq!(live.next_seq(), 5);
    }

    #[test]
    fn neither_slot_readable_names_no_live_slot() {
        // What the caller does about it is decided at the I/O boundary,
        // where an absent journal and an unreadable one are told apart.
        let live = live_slot([Slot::Torn, Slot::Torn]);
        assert_eq!(live.slot, Slot::Torn);
        assert_eq!(live.index, None);
        // A first record starts at slot zero, sequence one.
        assert_eq!(live.next_index(), 0);
        assert_eq!(live.next_seq(), 1);
    }

    #[test]
    fn a_batch_is_a_hundred_and_eighteen_clusters() {
        // One progress write per this many clusters freed. Worth stating
        // outright: it is the whole reason the journal is affordable.
        // That the batch fits its slot is asserted at compile time beside
        // the layout; this is the number itself, which is what a reader
        // wants when weighing how often progress is written.
        assert_eq!(MAX_BATCH, 118);
    }

    /// A whole record from a build this one does not know, at `seq`.
    fn from_a_later_build(seq: u32) -> [u8; SLOT_BYTES] {
        let mut bytes = batch(seq, &[20, 21], 22).encode();
        bytes[OFF_VERSION..OFF_VERSION + 2].copy_from_slice(&(VERSION + 1).to_le_bytes());
        let crc = fnv1a(&bytes[..OFF_CRC]);
        bytes[OFF_CRC..OFF_CRC + 4].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    #[test]
    fn one_slot_this_build_cannot_read_refuses_the_whole_journal() {
        // The downgrade case. A later build wrote slot B durably and began
        // freeing the batch it describes; the card then booted this build,
        // which cannot read B. Choosing A -- readable, older, and possibly
        // describing a chain that has since been taken apart -- is the
        // lost-topology failure again. Worse, if A says Clear this build
        // would call the journal idle and let unrelated work proceed over an
        // unfinished reclaim.
        let usable = [
            decode_slot(&batch(10, &[10], 0).encode()),
            decode_slot(&from_a_later_build(11)),
        ];
        assert!(
            !slots_are_usable(&usable),
            "a later build's record must stop this one"
        );

        // Including when the readable slot is the reassuring one.
        let clear_and_later = [
            decode_slot(&Batch::clear(10)),
            decode_slot(&from_a_later_build(11)),
        ];
        assert!(!slots_are_usable(&clear_and_later));

        // And whichever slot it lands in.
        let other_way = [
            decode_slot(&from_a_later_build(11)),
            decode_slot(&batch(10, &[10], 0).encode()),
        ];
        assert!(!slots_are_usable(&other_way));
    }

    #[test]
    fn a_torn_slot_beside_a_readable_one_is_still_usable() {
        // The distinction earning its keep: torn is the case the second slot
        // exists for, and must not be swept up with unsupported.
        let slots = [decode_slot(&batch(10, &[10], 0).encode()), Slot::Torn];
        assert!(slots_are_usable(&slots));
    }

    #[test]
    fn falling_back_is_only_ever_to_a_record_that_is_safe_to_redo() {
        // The rule finding the older slot depends on: a batch is freed only
        // after the slot describing it is durable. So if the newer slot did
        // not survive decoding, its batch was never freed, and the older
        // record still describes the world. This pins the contract the
        // fallback rests on -- and read_journal is what keeps a card that
        // *would not answer* from being mistaken for this case, because
        // there the newer slot may well have been durable.
        let older = decode_slot(&batch(4, &[10, 11], 12).encode());
        let mut torn = batch(5, &[12, 13], 14).encode();
        torn[100] ^= 0xFF;
        let live = live_slot([older.clone(), decode_slot(&torn)]);
        assert_eq!(live.slot, older);
        assert_eq!(live.next_index(), 1, "the torn slot is the one to reuse");
    }

    #[test]
    fn the_sequence_comparison_survives_wrapping() {
        // A journal written u32::MAX times must not suddenly prefer the
        // record it just superseded.
        let old = decode_slot(&batch(u32::MAX, &[10], 0).encode());
        let new = decode_slot(&batch(0, &[11], 0).encode());
        assert_eq!(live_slot([old.clone(), new.clone()]).index, Some(1));
        assert_eq!(live_slot([new, old]).index, Some(0));
    }
}
