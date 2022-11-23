use std::collections::{BTreeSet, HashMap, HashSet};

use borsh::BorshDeserialize;
use eyre::{eyre, Result};

use super::{ChangedKeys, Tally, Votes};
use crate::ledger::eth_bridge::storage::vote_tallies;
use crate::ledger::storage::traits::StorageHasher;
use crate::ledger::storage::{DBIter, Storage, DB};
use crate::types::address::Address;
use crate::types::storage::BlockHeight;
use crate::types::voting_power::FractionalVotingPower;

/// Wraps all the information about votes needed for updating some existing
/// tally in storage.
pub(in super::super) struct VoteInfo {
    inner: HashMap<Address, (BlockHeight, FractionalVotingPower)>,
}

impl VoteInfo {
    /// Constructs a new [`VoteInfo`]. For all `votes` provided, a corresponding
    /// [`FractionalVotingPower`] must be provided in `voting_powers` also,
    /// otherwise an error will be returned.
    pub fn new(
        votes: Votes,
        voting_powers: &HashMap<(Address, BlockHeight), FractionalVotingPower>,
    ) -> Result<Self> {
        let mut inner = HashMap::default();
        for (address, block_height) in votes {
            let fract_voting_power = match voting_powers
                .get(&(address.clone(), block_height))
            {
                Some(fract_voting_power) => fract_voting_power,
                None => {
                    return Err(eyre!(
                        "No fractional voting power provided for vote by \
                         validator {address} at block height {block_height}"
                    ));
                }
            };
            _ = inner
                .insert(address, (block_height, fract_voting_power.to_owned()));
        }
        Ok(Self { inner })
    }

    pub fn voters(&self) -> BTreeSet<Address> {
        self.inner.keys().cloned().collect()
    }
}

impl IntoIterator for VoteInfo {
    type IntoIter = std::collections::hash_set::IntoIter<Self::Item>;
    type Item = (Address, BlockHeight, FractionalVotingPower);

    fn into_iter(self) -> Self::IntoIter {
        let items: HashSet<_> = self
            .inner
            .into_iter()
            .map(|(address, (block_height, fract_voting_power))| {
                (address, block_height, fract_voting_power)
            })
            .collect();
        items.into_iter()
    }
}

/// Calculate an updated [`Tally`] based on one that is in storage under `keys`,
/// with some new `voters`.
pub(in super::super) fn calculate_updated<D, H, T>(
    store: &mut Storage<D, H>,
    keys: &vote_tallies::Keys<T>,
    vote_info: VoteInfo,
) -> Result<(Tally, ChangedKeys)>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    T: BorshDeserialize,
{
    tracing::info!(
        ?keys.prefix,
        validators = ?vote_info.voters(),
        "Recording validators as having voted for this event"
    );
    let tally_pre = super::storage::read(store, keys)?;
    let tally_post = calculate_tally_post(&tally_pre, vote_info)?;
    let changed_keys = validate_update(keys, &tally_pre, &tally_post)?;

    if tally_post.seen {
        tracing::info!(
            ?keys.prefix,
            "Event has been seen by a quorum of validators",
        );
    } else {
        tracing::debug!(
            ?keys.prefix,
            "Event is not yet seen by a quorum of validators",
        );
    };

    tracing::debug!(
        ?tally_pre,
        ?tally_post,
        "Calculated and validated vote tracking updates",
    );
    Ok((tally_post, changed_keys))
}

/// Takes an existing [`Tally`] and calculates the new [`Tally`] based on new
/// voters from `vote_info`. Returns an error if any new voters have already
/// voted previously.
fn calculate_tally_post(pre: &Tally, vote_info: VoteInfo) -> Result<Tally> {
    let previous_voters: BTreeSet<_> = pre.seen_by.keys().cloned().collect();
    let new_voters = vote_info.voters();
    let duplicate_voters: BTreeSet<_> =
        previous_voters.intersection(&new_voters).collect();
    if !duplicate_voters.is_empty() {
        // TODO: this is a programmer error and should never happen
        return Err(eyre!("Duplicate voters found - {:?}", duplicate_voters));
    }

    let mut voting_power_post = pre.voting_power.clone();
    let mut seen_by_post = pre.seen_by.clone();
    for (validator, vote_height, voting_power) in vote_info {
        _ = seen_by_post.insert(validator, vote_height);
        voting_power_post += voting_power;
    }

    let seen_post = voting_power_post > FractionalVotingPower::TWO_THIRDS;

    Ok(Tally {
        voting_power: voting_power_post,
        seen_by: seen_by_post,
        seen: seen_post,
    })
}

/// Validates that `post` is an updated version of `pre`, and returns keys which
/// changed. This function serves as a sort of validity predicate for this
/// native transaction, which is otherwise not checked by anything else.
fn validate_update<T>(
    keys: &vote_tallies::Keys<T>,
    pre: &Tally,
    post: &Tally,
) -> Result<ChangedKeys> {
    let mut keys_changed = ChangedKeys::default();

    let mut seen = false;
    if pre.seen != post.seen {
        // the only valid transition for `seen` is from `false` to `true`
        if pre.seen || !post.seen {
            return Err(eyre!(
                "Tally seen changed from {:#?} to {:#?}",
                &pre.seen,
                &post.seen,
            ));
        }
        keys_changed.insert(keys.seen());
        seen = true;
    }
    let pre_seen_by: BTreeSet<_> = pre.seen_by.keys().cloned().collect();
    let post_seen_by: BTreeSet<_> = post.seen_by.keys().cloned().collect();

    if pre_seen_by != post_seen_by {
        // if seen_by changes, it must be a strict superset of the previous
        // seen_by
        if !post_seen_by.is_superset(&pre_seen_by) {
            return Err(eyre!(
                "Tally seen changed from {:#?} to {:#?}",
                &pre_seen_by,
                &post_seen_by,
            ));
        }
        keys_changed.insert(keys.seen_by());
    }

    if pre.voting_power != post.voting_power {
        // if voting_power changes, it must have increased
        if pre.voting_power >= post.voting_power {
            return Err(eyre!(
                "Tally voting_power changed from {:#?} to {:#?}",
                &pre.voting_power,
                &post.voting_power,
            ));
        }
        keys_changed.insert(keys.voting_power());
    }

    if post.voting_power > FractionalVotingPower::TWO_THIRDS
        && !seen
        && pre.voting_power >= post.voting_power
    {
        return Err(eyre!(
            "Tally is not seen even though new voting_power is enough: {:#?}",
            &post.voting_power,
        ));
    }

    Ok(keys_changed)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::ledger::protocol::transactions::votes;
    use crate::ledger::protocol::transactions::votes::update::tests::helpers::{arbitrary_event, setup_tally};
    use crate::ledger::storage::testing::TestStorage;
    use crate::types::address;
    use crate::types::ethereum_events::EthereumEvent;

    mod helpers {
        use super::*;

        /// Returns an arbitrary piece of data that can be tallied, and the keys
        /// for it.
        pub(super) fn arbitrary_event()
        -> (EthereumEvent, vote_tallies::Keys<EthereumEvent>) {
            let event = EthereumEvent::TransfersToNamada {
                nonce: 0.into(),
                transfers: vec![],
            };
            let keys = vote_tallies::Keys::from(&event);
            (event, keys)
        }

        /// Writes an initial [`Tally`] to storage, based on the passed `votes`.
        pub(super) fn setup_tally(
            storage: &mut TestStorage,
            event: &EthereumEvent,
            keys: &vote_tallies::Keys<EthereumEvent>,
            votes: HashSet<(Address, BlockHeight, FractionalVotingPower)>,
        ) -> Result<Tally> {
            let voting_power: FractionalVotingPower =
                votes.iter().cloned().map(|(_, _, v)| v).sum();
            let tally = Tally {
                voting_power: voting_power.to_owned(),
                seen_by: votes.into_iter().map(|(a, h, _)| (a, h)).collect(),
                seen: voting_power > FractionalVotingPower::TWO_THIRDS,
            };
            votes::storage::write(storage, keys, event, &tally)?;
            Ok(tally)
        }
    }

    #[test]
    fn test_vote_info_new_empty() -> Result<()> {
        let voting_powers = HashMap::default();

        let vote_info = VoteInfo::new(Votes::default(), &voting_powers)?;

        assert!(vote_info.voters().is_empty());
        assert_eq!(vote_info.into_iter().count(), 0);
        Ok(())
    }

    #[test]
    fn test_vote_info_new_single_voter() -> Result<()> {
        let validator = address::testing::established_address_1;
        let vote_height = || BlockHeight(100);
        let voting_power = || FractionalVotingPower::new(1, 3).unwrap();
        let vote = || (validator(), vote_height());
        let votes = Votes::from([vote()]);
        let voting_powers = HashMap::from([(vote(), voting_power())]);

        let vote_info = VoteInfo::new(votes, &voting_powers)?;

        assert_eq!(vote_info.voters(), BTreeSet::from([validator()]));
        let votes: BTreeSet<_> = vote_info.into_iter().collect();
        assert_eq!(
            votes,
            BTreeSet::from([(validator(), vote_height(), voting_power())]),
        );
        Ok(())
    }

    #[test]
    fn test_calculate_updated_empty() -> Result<()> {
        let mut storage = TestStorage::default();
        let (event, keys) = arbitrary_event();
        let tally_pre = setup_tally(
            &mut storage,
            &event,
            &keys,
            HashSet::from([(
                address::testing::established_address_1(),
                BlockHeight(100),
                FractionalVotingPower::new(1, 3).unwrap(),
            )]),
        )?;
        votes::storage::write(&mut storage, &keys, &event, &tally_pre)?;
        let vote_info = VoteInfo::new(Votes::default(), &HashMap::default())?;

        let (tally_post, changed_keys) =
            calculate_updated(&mut storage, &keys, vote_info)?;

        assert_eq!(tally_post, tally_pre);
        assert!(changed_keys.is_empty());
        Ok(())
    }

    #[test]
    fn test_calculate_updated_one_vote_not_seen() -> Result<()> {
        let mut storage = TestStorage::default();

        let (event, keys) = arbitrary_event();
        let (event, keys) = arbitrary_event();
        let tally_pre = setup_tally(
            &mut storage,
            &event,
            &keys,
            HashSet::from([(
                address::testing::established_address_1(),
                BlockHeight(100),
                FractionalVotingPower::new(1, 3).unwrap(),
            )]),
        )?;
        votes::storage::write(&mut storage, &keys, &event, &tally_pre)?;

        let validator = address::testing::established_address_2;
        let vote_height = || BlockHeight(100);
        let voting_power = || FractionalVotingPower::new(1, 3).unwrap();
        let vote = || (validator(), vote_height());
        let votes = Votes::from([vote()]);
        let voting_powers = HashMap::from([(vote(), voting_power())]);
        let vote_info = VoteInfo::new(votes, &voting_powers)?;

        let (tally_post, changed_keys) =
            calculate_updated(&mut storage, &keys, vote_info)?;

        assert_eq!(
            tally_post,
            Tally {
                voting_power: FractionalVotingPower::new(2, 3).unwrap(),
                seen_by: BTreeMap::from([
                    (address::testing::established_address_1(), 10.into()),
                    vote(),
                ]),
                seen: false,
            }
        );
        assert_eq!(
            changed_keys,
            BTreeSet::from([keys.voting_power(), keys.seen_by()])
        );
        Ok(())
    }
}
