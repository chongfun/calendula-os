//! Managed replacement: keeping a copy's [`BookId`] when an upload lands over
//! it.
//!
//! An upload that lands under a name the shelf already holds is two
//! transactions. The filesystem one, `INSTALL.JNL`, swaps the bytes and
//! clears itself. The library one is what this module keeps: that the copy
//! at that place is the same copy, now made of other bytes, and that its
//! record in the ledger should say so. Nothing bridged them before, so a
//! power cut after the install had cleared its journal and before the
//! ledger had been told left a record saying one thing and a file holding
//! another, which the next scan could only read as a stranger at a known
//! path, and mint a fresh id for. Reading position, once it hangs from the
//! id, would have gone with the old one.
//!
//! So the installer publishes an intent here before `INSTALL.JNL` is
//! written: which id, at which place, what stood there, and which bytes are
//! meant to land. It stands after `INSTALL.JNL` clears and is cleared only
//! once the ledger record has been rewritten. Recovery runs it after the
//! filesystem journals have settled, and asks the card rather than the
//! record which side won: the destination's bytes are hashed and compared
//! to the intent. The new digest is decisive. A predecessor that had a
//! digest is recognised by it. One that had none is recognised as any file
//! that is not the new bytes, which holds only because a live intent is a
//! storage transaction for the sole-writer contract: nothing else was
//! permitted to write while it stood. Where nothing stood, nothing standing
//! is the old landing. Anything else keeps the intent and refuses, and
//! while it stands no other replacement may begin and no scan may adopt.
//!
//! The intent is kept in `/READER/REPLACE.JNL` the way the ledger journal
//! is: two sector-sized slots written alternately with a sequence number, so
//! a torn publication leaves the entry before it. A torn publication is an
//! install that has not begun, since the installer waits for it; a torn
//! clear is an intent that still stands, which recovery resolves again, and
//! resolving is idempotent.
//!
//! No `BookId` or digest enters `INSTALL.JNL` or `RECLAIM.JNL`. The
//! filesystem transaction decides what the card holds, and this one only
//! records what that means for identity.

use core::fmt::Write as _;

use embedded_sdmmc::{Directory, TimeSource};
use heapless::String;
use proto::cache::CACHE_ROOT_DIR;
use proto::identity::{
    classify_replace_journal, encode_replace_journal, landing, BookId, Landing, LedgerRecord,
    Predecessor, ReplaceIntent, ReplaceJournal, ReplaceJournalReading, REPLACE_JOURNAL_BYTES,
    REPLACE_JOURNAL_SLOTS, REPLACE_JOURNAL_SLOT_BYTES,
};
use proto::library_path::{BookRoot, LibraryPath, MAX_PATH_BYTES};
use proto::source::{CachedSourceDigest, SourceDigest};

use crate::ledger::{self, LedgerFault, SlotVerdict};

/// The intent, beside the ledger it will be published to.
pub const REPLACE_JOURNAL: &str = "REPLACE.JNL";

const _: () = assert!(REPLACE_JOURNAL_SLOT_BYTES == ledger::SLOT_BYTES);
const _: () = assert!(REPLACE_JOURNAL_SLOTS == ledger::SLOT_COUNT);

/// A replacement in flight, as read back off the card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub id: BookId,
    pub root: BookRoot,
    pub locator: String<MAX_PATH_BYTES>,
    pub predecessor: Predecessor,
    pub new: CachedSourceDigest,
}

impl Standing {
    fn from_intent(intent: &ReplaceIntent<'_>) -> Result<Self, LedgerFault> {
        let mut locator = String::new();
        locator
            .push_str(intent.locator)
            .map_err(|_| LedgerFault::Record)?;
        Ok(Self {
            id: intent.id,
            root: intent.root,
            locator,
            predecessor: intent.predecessor,
            new: intent.new,
        })
    }
}

/// The replacement in flight, or `None` when none is.
pub fn read<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Standing>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(None),
        Err(_) => return Err(LedgerFault::Device),
    };
    standing(&cache_root)
}

/// Publish the intent for an install about to begin.
///
/// `locator` is where the install lands, under `at`; `new` is the identity
/// of the bytes staged for it; `predecessor_size` is the size of the file
/// holding that place now, or `None` for no file. The copy keeps the id of
/// the ledger record naming that place and size, if there is one, along
/// with whatever that record knew of the predecessor's bytes; a place no
/// record names is a copy the library has not adopted, and is minted an id
/// here so that the landing can be published under it.
///
/// Refuses with [`LedgerFault::Busy`] while another intent stands. The
/// caller must not touch the destination until this returns: the intent is
/// durable then, and not before.
pub fn begin<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    at: BookRoot,
    locator: &str,
    new: SourceDigest,
    predecessor_size: Option<u32>,
    random: &mut impl FnMut() -> u32,
) -> Result<Standing, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = ledger::open_or_make_cache_root(root)?;
    if standing(&cache_root)?.is_some() {
        return Err(LedgerFault::Busy);
    }
    let live = ledger::open(root)?;
    let (id, predecessor) = match predecessor_size {
        None => (BookId::mint(random), Predecessor::None),
        Some(size) => {
            let known = match &live {
                Some(live) => ledger::find_record(root, live, at, locator, size)?,
                None => None,
            };
            match known {
                Some((id, Some(old))) => (id, Predecessor::Known(old)),
                Some((id, None)) => (id, Predecessor::Unknown),
                None => (BookId::mint(random), Predecessor::Unknown),
            }
        }
    };
    let intent = ReplaceIntent {
        id,
        root: at,
        locator,
        predecessor,
        new: CachedSourceDigest::new(new),
    };
    publish(&cache_root, &ReplaceJournal::Standing(intent))?;
    Standing::from_intent(&intent)
}

/// Settle the intent in flight, the landing being known.
///
/// A new landing rewrites the copy's ledger record with the new size and
/// digest under the same id; an old landing leaves the ledger as it was.
/// Either way the intent is then cleared. Nothing to settle is not an
/// error: a clear that was cut leaves an intent recovery resolves again,
/// and a caller may settle what recovery already did.
pub fn settle<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
    landed: Landing,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = ledger::open_or_make_cache_root(root)?;
    let Some(intent) = standing(&cache_root)? else {
        return Ok(());
    };
    if landed == Landing::New {
        let live = ledger::open(root)?;
        let byte_size = u32::try_from(intent.new.byte_len()).map_err(|_| LedgerFault::Record)?;
        ledger::publish_record(
            root,
            live,
            &LedgerRecord {
                id: intent.id,
                root: intent.root,
                locator: intent.locator.as_str(),
                byte_size,
                misses: 0,
                source: Some(intent.new),
            },
        )?;
    }
    publish(&cache_root, &ReplaceJournal::Cleared)
}

/// What recovery found and did about a replacement in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// No intent stands.
    Nothing,
    /// An intent stood, the destination said which side it held, and the
    /// ledger and the intent now say so too.
    Settled(Landing),
    /// An intent stands and the destination holds neither legal landing.
    /// Nothing was changed, and nothing may adopt or replace until it is
    /// looked at.
    Refused,
}

/// Resolve a standing intent against what the destination holds.
///
/// To be run after `RECLAIM.JNL` and `INSTALL.JNL` have settled and not
/// before: the filesystem transaction decides what the card holds, and this
/// one only records what that means. The destination is hashed whole, since
/// which side landed is a question about its bytes and a persisted digest
/// is evidence rather than an answer.
pub fn recover<D, T, const MD: usize, const MF: usize, const MV: usize>(
    root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Recovery, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let cache_root = match root.open_dir(CACHE_ROOT_DIR) {
        Ok(dir) => dir,
        Err(embedded_sdmmc::Error::NotFound) => return Ok(Recovery::Nothing),
        Err(_) => return Err(LedgerFault::Device),
    };
    let Some(intent) = standing(&cache_root)? else {
        return Ok(Recovery::Nothing);
    };
    let path = LibraryPath::parse(intent.locator.as_str()).map_err(|_| LedgerFault::Record)?;
    let destination = crate::library::with_book_at(root, intent.root, &path, |dir, alias| {
        let mut name = String::<12>::new();
        if write!(name, "{}", alias).is_err() {
            return Err(crate::install::InstallError::Card);
        }
        crate::digest_of_file(dir, name.as_str())
    })
    .map_err(|_| LedgerFault::Device)?;
    let destination = match destination {
        // No shelf, or no file at the place.
        None => None,
        Some(Ok(found)) => found,
        Some(Err(_)) => return Err(LedgerFault::Device),
    };
    match landing(&intent.predecessor, &intent.new, destination.as_ref()) {
        Some(landed) => {
            settle(root, landed)?;
            Ok(Recovery::Settled(landed))
        }
        None => Ok(Recovery::Refused),
    }
}

fn verdict(bytes: &[u8; REPLACE_JOURNAL_BYTES]) -> SlotVerdict {
    match classify_replace_journal(bytes) {
        ReplaceJournalReading::Blank => SlotVerdict::Blank,
        ReplaceJournalReading::Entry { sequence, .. } => SlotVerdict::Entry(sequence),
        ReplaceJournalReading::UnknownVersion(_) => SlotVerdict::UnknownVersion,
        ReplaceJournalReading::Damaged => SlotVerdict::Damaged,
    }
}

/// The intent in flight, read from the newest entry, or `None` when the
/// newest entry is a clear or there is no journal.
fn standing<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
) -> Result<Option<Standing>, LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let Some((bytes, _, _)) = ledger::newest_slot(cache_root, REPLACE_JOURNAL, verdict)? else {
        return Ok(None);
    };
    match classify_replace_journal(&bytes) {
        ReplaceJournalReading::Entry {
            entry: ReplaceJournal::Standing(intent),
            ..
        } => Ok(Some(Standing::from_intent(&intent)?)),
        ReplaceJournalReading::Entry {
            entry: ReplaceJournal::Cleared,
            ..
        } => Ok(None),
        // `newest_slot` hands back only a slot it classified as an entry.
        _ => Err(LedgerFault::Damaged),
    }
}

fn publish<D, T, const MD: usize, const MF: usize, const MV: usize>(
    cache_root: &Directory<'_, D, T, MD, MF, MV>,
    entry: &ReplaceJournal<'_>,
) -> Result<(), LedgerFault>
where
    D: embedded_sdmmc::BlockDevice,
    T: TimeSource,
{
    let current = ledger::newest_slot(cache_root, REPLACE_JOURNAL, verdict)?
        .map(|(_, slot, sequence)| (slot, sequence));
    ledger::publish_slot(cache_root, REPLACE_JOURNAL, current, |sequence, out| {
        encode_replace_journal(entry, sequence, out).ok_or(LedgerFault::Record)
    })
}
