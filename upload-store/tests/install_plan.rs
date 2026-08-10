//! The install journal's record format and its recovery planner.
//!
//! These are the parts that decide what happens after a power cut, so they
//! are exercised on their own, without a card underneath: every state the
//! card can present is enumerated here, including the ones that should be
//! unreachable.

use heapless::String;
use upload_store::install::{plan, InstallIntent, Located, Presence, Step, RECORD_BYTES};

fn short(text: &str) -> String<12> {
    let mut name = String::new();
    name.push_str(text).expect("short name fits");
    name
}

fn long(text: &str) -> String<64> {
    let mut name = String::new();
    name.push_str(text).expect("long name fits");
    name
}

fn replacement() -> InstallIntent {
    InstallIntent {
        // The chains a real record writes down alongside each name.
        stage: Located {
            alias: short("TXN00001.TMP"),
            chain: 21,
        },
        long_name: long("A Book With A Long Name.epub"),
        old: Some(Located {
            alias: short("BOOK0001.EPU"),
            chain: 9,
        }),
        rollback: short("TXN00001.OLD"),
    }
}

fn fresh() -> InstallIntent {
    InstallIntent {
        old: None,
        ..replacement()
    }
}

fn at(old: bool, rollback: bool, stage: bool, dest: bool) -> Presence {
    Presence {
        old,
        rollback,
        stage,
        dest,
        foreign: false,
    }
}

/// The same state, with the long name held by a file this transaction did not
/// put there.
fn intruded(at: Presence) -> Presence {
    Presence {
        foreign: true,
        ..at
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

#[test]
fn a_record_survives_the_round_trip() {
    for intent in [replacement(), fresh()] {
        let bytes = intent.encode();
        assert_eq!(bytes.len(), RECORD_BYTES);
        assert_eq!(
            InstallIntent::decode(&bytes),
            Some(intent),
            "a record must read back as what was written"
        );
    }
}

#[test]
fn a_record_that_did_not_survive_its_write_does_not_decode() {
    let good = replacement().encode();

    // Every single-byte corruption must fail to decode, so no damaged record
    // is acted on as though it described work.
    //
    // Failing to decode is not "no transaction": a whole record that will not
    // decode is kept and blocks the card, since it was written and so its
    // transaction had started. What a rejection means is `IntentState`'s to
    // say -- see `a_record_from_another_build_is_kept_and_blocks_the_card`.
    for index in 0..RECORD_BYTES {
        let mut torn = good;
        torn[index] ^= 0xFF;
        assert!(
            InstallIntent::decode(&torn).is_none(),
            "byte {index} was allowed to change without invalidating the record"
        );
    }

    // The commonest tear of all: the write stopped part way.
    for kept in 0..RECORD_BYTES {
        let mut torn = [0u8; RECORD_BYTES];
        torn[..kept].copy_from_slice(&good[..kept]);
        assert!(
            InstallIntent::decode(&torn).is_none(),
            "a record cut off at {kept} bytes must not decode"
        );
    }

    assert!(InstallIntent::decode(&[]).is_none());
    assert!(InstallIntent::decode(&good[..RECORD_BYTES - 1]).is_none());
}

// ---------------------------------------------------------------------------
// The planner
// ---------------------------------------------------------------------------

#[test]
fn the_happy_path_walks_forward_one_step_at_a_time() {
    let intent = replacement();
    let steps = [
        (at(true, false, true, false), Step::RetireOldHolder),
        (at(true, true, true, false), Step::UnlinkOldHolder),
        (at(false, true, true, false), Step::InstallStage),
        (at(false, true, true, true), Step::UnlinkStage),
        (at(false, true, false, true), Step::ReclaimRollback),
        (at(false, false, false, true), Step::Done),
    ];
    for (presence, expected) in steps {
        assert_eq!(plan(&intent, presence), expected, "at {presence:?}");
    }
}

#[test]
fn a_first_upload_skips_the_predecessor_steps() {
    let intent = fresh();
    assert_eq!(
        plan(&intent, at(false, false, true, false)),
        Step::InstallStage
    );
    assert_eq!(
        plan(&intent, at(false, false, true, true)),
        Step::UnlinkStage
    );
    assert_eq!(plan(&intent, at(false, false, false, true)), Step::Done);
}

#[test]
fn a_lost_upload_puts_the_predecessor_back() {
    let intent = replacement();
    // Parked, and its own name is free: restore it.
    assert_eq!(
        plan(&intent, at(false, true, false, false)),
        Step::RestoreOldHolder
    );
    // Restored, with the parked name still on the same chain: unlink it.
    assert_eq!(
        plan(&intent, at(true, true, false, false)),
        Step::UnlinkRollbackCopy
    );
    assert_eq!(plan(&intent, at(true, false, false, false)), Step::Done);
}

/// A parked copy belonging to no recorded predecessor must be left alone.
#[test]
fn a_stray_rollback_copy_is_not_this_transactions_to_delete() {
    assert_eq!(plan(&fresh(), at(false, true, false, false)), Step::Done);
}

/// Somebody put their own file on the shelf under the name this upload was
/// going to take. It cannot be moved aside, so the install can never happen
/// and retrying it would refuse every later upload for as long as that file
/// stays there.
#[test]
fn an_upload_gives_up_when_something_else_holds_its_name() {
    for intent in [replacement(), fresh()] {
        for old in [false, true] {
            assert_eq!(
                plan(&intent, intruded(at(old, false, true, false))),
                Step::Done,
                "nothing of this transaction's is off the shelf; the sweep takes the upload"
            );
        }
    }
    // Not while the predecessor is parked: it cannot go back under a name
    // someone else holds, and the record is the only thing keeping the sweep
    // off it. The install it keeps asking for is one the card refuses, so
    // recovery reports itself unfinished — which is the honest answer.
    assert_eq!(
        plan(&replacement(), intruded(at(false, true, true, false))),
        Step::InstallStage
    );
}

/// Only one step frees clusters, and it is never reachable while two names
/// share a chain. This is the invariant that keeps a half-done move from
/// destroying the file it was saving.
#[test]
fn no_step_that_frees_clusters_is_offered_while_a_chain_is_shared() {
    for intent in [replacement(), fresh()] {
        for bits in 0..32u8 {
            let mut presence = at(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            presence.foreign = bits & 16 != 0;
            if (presence.old && presence.rollback) || (presence.stage && presence.dest) {
                assert_ne!(
                    plan(&intent, presence),
                    Step::ReclaimRollback,
                    "reclaiming at {presence:?} would free a chain another name still holds"
                );
            }
        }
    }
}

/// Recovery must finish. Applying the planner's steps from any starting state
/// has to reach `Done` rather than cycle between two repairs.
#[test]
fn recovery_terminates_from_every_state_it_can_observe() {
    for intent in [replacement(), fresh()] {
        for bits in 0..32u8 {
            let mut start = at(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
            start.foreign = bits & 16 != 0;
            // The one state with no way out, and deliberately so: the name is
            // taken and the predecessor is parked, so neither the install nor
            // the restore can happen. Recovery stops at its own step bound and
            // reports itself unfinished rather than clearing a record that is
            // all that keeps the sweep off the parked book. Asserting
            // termination here would be asserting that it gives up.
            if start.foreign && start.stage && !start.dest && start.rollback {
                assert_eq!(plan(&intent, start), Step::InstallStage);
                continue;
            }
            let mut presence = start;
            let mut taken = 0;
            loop {
                let step = plan(&intent, presence);
                if step == Step::Done {
                    break;
                }
                presence = apply(step, presence);
                taken += 1;
                assert!(
                    taken <= 8,
                    "starting from {start:?}, recovery kept finding work to do"
                );
            }
        }
    }
}

/// Which steps need a predecessor, and so are the ones a record without one
/// cannot carry out. `plan` reaches them only through `at.old`, which for such
/// a record needs a shelf entry sharing the parked copy's chain — a state this
/// transaction never builds. `apply_step` refuses them anyway, and recovery
/// settles rather than asking forever: see
/// `a_step_the_record_cannot_describe_settles_instead_of_wedging`.
#[test]
fn the_predecessor_steps_are_the_ones_a_record_without_one_cannot_do() {
    for bits in 0..32u8 {
        let mut at = at(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
        at.foreign = bits & 16 != 0;
        let step = plan(&fresh(), at);
        let needs_predecessor = matches!(
            step,
            Step::RetireOldHolder | Step::UnlinkOldHolder | Step::RestoreOldHolder
        );
        assert!(
            !needs_predecessor || at.old,
            "at {at:?} a record with no predecessor was planned {step:?}, \
             which it has nothing to carry out"
        );
    }
    // And the one that walks backwards is refused outright, since putting a
    // book back is meaningless when the record names none to put.
    assert_ne!(
        plan(&fresh(), at(false, true, false, false)),
        Step::RestoreOldHolder
    );
}

/// What each step does to the card, so a plan can be walked without one.
fn apply(step: Step, at: Presence) -> Presence {
    match step {
        Step::RetireOldHolder => Presence {
            rollback: true,
            ..at
        },
        Step::UnlinkOldHolder => Presence { old: false, ..at },
        // A name somebody else holds is one the driver will not link a second
        // entry onto, so this step changes nothing at all. Modelling it as a
        // success would hide the one state the planner cannot walk out of.
        Step::InstallStage if at.foreign => at,
        // A move, so the scratch name goes as the shelf name arrives.
        Step::InstallStage => Presence {
            stage: false,
            dest: true,
            ..at
        },
        Step::Done => at,
        Step::UnlinkStage => Presence { stage: false, ..at },
        Step::ReclaimRollback => Presence {
            rollback: false,
            ..at
        },
        // The predecessor is back on the shelf. What the card shows is an
        // entry under a driver-derived alias, which `observe` has to read as
        // the predecessor by its cluster chain rather than its name -- see
        // `a_restore_cut_half_way_through_is_finished_not_repeated`.
        Step::RestoreOldHolder => Presence { old: true, ..at },
        Step::UnlinkRollbackCopy => Presence {
            rollback: false,
            ..at
        },
    }
}

/// The end state is always exactly one book: never none, never a duplicate.
///
/// This transaction's copies, which is what `old` and `dest` count. A shelf
/// with a stranger's file on it is not one of these states — see
/// `an_upload_gives_up_when_something_else_holds_its_name`.
#[test]
fn every_recovery_ends_with_one_book_on_the_shelf() {
    // Only the replacement intent. A fresh install has no predecessor, so a
    // parked copy in one of these states is a stray belonging to no recorded
    // transaction, and the planner deliberately leaves it alone rather than
    // guessing -- which ends with no book on the shelf and is the right answer
    // for a transaction that never had one.
    let intent = replacement();
    for bits in 0..16u8 {
        let start = at(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0);
        // Only states reachable from a real transaction are meaningful: the
        // upload cannot be missing before it is installed unless something
        // was parked or the predecessor still stands.
        if !start.stage && !start.dest && !start.old && !start.rollback {
            continue;
        }
        // The predecessor cannot still hold its name once the destination is
        // installed: it is retired first. Reaching this state means something
        // outside the transaction wrote to the destination alias, and the
        // planner deliberately leaves both files alone rather than guessing
        // which one the user wants.
        if start.old && start.dest {
            continue;
        }
        let mut presence = start;
        loop {
            let step = plan(&intent, presence);
            if step == Step::Done {
                break;
            }
            presence = apply(step, presence);
        }
        let on_shelf = usize::from(presence.old) + usize::from(presence.dest);
        assert_eq!(
            on_shelf, 1,
            "starting from {start:?}, the shelf ended with {on_shelf} copies"
        );
    }
}
