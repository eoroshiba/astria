use std::collections::BTreeMap;

use anyhow::{
    bail,
    Context,
    Result,
};
use astria_core::{
    crypto::VerificationKey,
    primitive::v1::{
        Address,
        ADDRESS_LEN,
    },
    protocol::transaction::v1alpha1::action::ValidatorUpdate,
};
use async_trait::async_trait;
use borsh::{
    BorshDeserialize,
    BorshSerialize,
};
use cnidarium::{
    StateRead,
    StateWrite,
};
use serde::{
    Deserialize,
    Serialize,
};
use tracing::instrument;

/// Newtype wrapper to read and write an address from rocksdb.
#[derive(BorshSerialize, BorshDeserialize, Debug)]
struct SudoAddress([u8; ADDRESS_LEN]);

/// Newtype wrapper to read and write a validator set or set of updates from rocksdb.
///
/// Contains a map of hex-encoded public keys to validator updates.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ValidatorSet(BTreeMap<ValidatorSetKey, ValidatorUpdate>);

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Ord, PartialOrd)]
pub(crate) struct ValidatorSetKey(#[serde(with = "::hex::serde")] [u8; ADDRESS_LEN]);

impl From<[u8; ADDRESS_LEN]> for ValidatorSetKey {
    fn from(value: [u8; ADDRESS_LEN]) -> Self {
        Self(value)
    }
}

impl From<VerificationKey> for ValidatorSetKey {
    fn from(value: VerificationKey) -> Self {
        Self(value.address_bytes())
    }
}

impl ValidatorSet {
    pub(crate) fn new_from_updates(updates: Vec<ValidatorUpdate>) -> Self {
        Self(
            updates
                .into_iter()
                .map(|update| (update.verification_key.into(), update))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn get<T: Into<ValidatorSetKey>>(&self, address: T) -> Option<&ValidatorUpdate> {
        self.0.get(&address.into())
    }

    pub(crate) fn push_update(&mut self, update: ValidatorUpdate) {
        self.0.insert(update.verification_key.into(), update);
    }

    pub(crate) fn remove<T: Into<ValidatorSetKey>>(&mut self, address: T) {
        self.0.remove(&address.into());
    }

    /// Apply updates to the validator set.
    ///
    /// If the power of a validator is set to 0, remove it from the set.
    /// Otherwise, update the validator's power.
    pub(crate) fn apply_updates(&mut self, validator_updates: ValidatorSet) {
        for (address, update) in validator_updates.0 {
            match update.power {
                0 => self.0.remove(&address),
                _ => self.0.insert(address, update),
            };
        }
    }

    pub(crate) fn try_into_cometbft(self) -> anyhow::Result<Vec<tendermint::validator::Update>> {
        self.0
            .into_values()
            .map(crate::utils::sequencer_to_cometbft_validator)
            .collect::<Result<Vec<_>, _>>()
            .context("failed to map one or more astria validators to cometbft validators")
    }
}

const SUDO_STORAGE_KEY: &str = "sudo";
const VALIDATOR_SET_STORAGE_KEY: &str = "valset";
const VALIDATOR_UPDATES_KEY: &[u8] = b"valupdates";

#[async_trait]
pub(crate) trait StateReadExt: StateRead {
    #[instrument(skip(self))]
    async fn get_sudo_address(&self) -> Result<Address> {
        let Some(bytes) = self
            .get_raw(SUDO_STORAGE_KEY)
            .await
            .context("failed reading raw sudo key from state")?
        else {
            // return error because sudo key must be set
            bail!("sudo key not found");
        };
        let SudoAddress(address_bytes) =
            SudoAddress::try_from_slice(&bytes).context("invalid sudo key bytes")?;
        Ok(crate::address::base_prefixed(address_bytes))
    }

    #[instrument(skip(self))]
    async fn get_validator_set(&self) -> Result<ValidatorSet> {
        let Some(bytes) = self
            .get_raw(VALIDATOR_SET_STORAGE_KEY)
            .await
            .context("failed reading raw validator set from state")?
        else {
            // return error because validator set must be set
            bail!("validator set not found")
        };

        let ValidatorSet(validator_set) =
            serde_json::from_slice(&bytes).context("invalid validator set bytes")?;
        Ok(ValidatorSet(validator_set))
    }

    #[instrument(skip(self))]
    async fn get_validator_updates(&self) -> Result<ValidatorSet> {
        let Some(bytes) = self
            .nonverifiable_get_raw(VALIDATOR_UPDATES_KEY)
            .await
            .context("failed reading raw validator updates from state")?
        else {
            // return empty set because validator updates are optional
            return Ok(ValidatorSet(BTreeMap::new()));
        };

        let validator_updates: ValidatorSet =
            serde_json::from_slice(&bytes).context("invalid validator updates bytes")?;
        Ok(validator_updates)
    }
}

impl<T: StateRead> StateReadExt for T {}

#[async_trait]
pub(crate) trait StateWriteExt: StateWrite {
    #[instrument(skip(self))]
    fn put_sudo_address(&mut self, address: Address) -> Result<()> {
        self.put_raw(
            SUDO_STORAGE_KEY.to_string(),
            borsh::to_vec(&SudoAddress(address.bytes()))
                .context("failed to convert sudo address to vec")?,
        );
        Ok(())
    }

    #[instrument(skip(self))]
    fn put_validator_set(&mut self, validator_set: ValidatorSet) -> Result<()> {
        self.put_raw(
            VALIDATOR_SET_STORAGE_KEY.to_string(),
            serde_json::to_vec(&validator_set).context("failed to serialize validator set")?,
        );
        Ok(())
    }

    #[instrument(skip(self))]
    fn put_validator_updates(&mut self, validator_updates: ValidatorSet) -> Result<()> {
        self.nonverifiable_put_raw(
            VALIDATOR_UPDATES_KEY.to_vec(),
            serde_json::to_vec(&validator_updates)
                .context("failed to serialize validator updates")?,
        );
        Ok(())
    }

    #[instrument(skip(self))]
    fn clear_validator_updates(&mut self) {
        self.nonverifiable_delete(VALIDATOR_UPDATES_KEY.to_vec());
    }
}

impl<T: StateWrite> StateWriteExt for T {}

#[cfg(test)]
mod test {
    use astria_core::protocol::transaction::v1alpha1::action::ValidatorUpdate;
    use cnidarium::StateDelta;

    use super::{
        StateReadExt as _,
        StateWriteExt as _,
        ValidatorSet,
    };
    use crate::test_utils::verification_key;

    fn empty_validator_set() -> ValidatorSet {
        ValidatorSet::new_from_updates(vec![])
    }

    #[tokio::test]
    async fn sudo_address() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state = StateDelta::new(snapshot);

        // doesn't exist at first
        state
            .get_sudo_address()
            .await
            .expect_err("no sudo address should exist at first");

        // can write new
        let mut address_expected = crate::address::base_prefixed([42u8; 20]);
        state
            .put_sudo_address(address_expected)
            .expect("writing sudo address should not fail");
        assert_eq!(
            state
                .get_sudo_address()
                .await
                .expect("a sudo address was written and must exist inside the database"),
            address_expected,
            "stored sudo address was not what was expected"
        );

        // can rewrite with new value
        address_expected = crate::address::base_prefixed([41u8; 20]);
        state
            .put_sudo_address(address_expected)
            .expect("writing sudo address should not fail");
        assert_eq!(
            state
                .get_sudo_address()
                .await
                .expect("a new sudo address was written and must exist inside the database"),
            address_expected,
            "updated sudo address was not what was expected"
        );
    }

    #[tokio::test]
    async fn validator_set_uninitialized_fails() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let state = StateDelta::new(snapshot);

        // doesn't exist at first
        state
            .get_validator_set()
            .await
            .expect_err("no validator set should exist at first");
    }

    #[tokio::test]
    async fn put_validator_set() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state = StateDelta::new(snapshot);

        let initial = vec![ValidatorUpdate {
            power: 10,
            verification_key: verification_key(1),
        }];
        let initial_validator_set = ValidatorSet::new_from_updates(initial);

        // can write new
        state
            .put_validator_set(initial_validator_set.clone())
            .expect("writing initial validator set should not fail");
        assert_eq!(
            state
                .get_validator_set()
                .await
                .expect("a validator set was written and must exist inside the database"),
            initial_validator_set,
            "stored validator set was not what was expected"
        );

        // can update
        let updates = vec![ValidatorUpdate {
            power: 20,
            verification_key: verification_key(2),
        }];
        let updated_validator_set = ValidatorSet::new_from_updates(updates);
        state
            .put_validator_set(updated_validator_set.clone())
            .expect("writing update validator set should not fail");
        assert_eq!(
            state
                .get_validator_set()
                .await
                .expect("a validator set was written and must exist inside the database"),
            updated_validator_set,
            "stored validator set was not what was expected"
        );
    }

    #[tokio::test]
    async fn get_validator_updates_empty() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let state = StateDelta::new(snapshot);

        // querying for empty validator set is ok
        assert_eq!(
            state
                .get_validator_updates()
                .await
                .expect("if no updates have been written return empty set"),
            empty_validator_set(),
            "returned empty validator set different than expected"
        );
    }

    #[tokio::test]
    async fn put_validator_updates() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state = StateDelta::new(snapshot);

        // create update validator set
        let mut updates = vec![
            ValidatorUpdate {
                power: 10,
                verification_key: verification_key(1),
            },
            ValidatorUpdate {
                power: 0,
                verification_key: verification_key(2),
            },
        ];
        let mut validator_set_updates = ValidatorSet::new_from_updates(updates);

        // put validator updates
        state
            .put_validator_updates(validator_set_updates.clone())
            .expect("writing update validator set should not fail");
        assert_eq!(
            state
                .get_validator_updates()
                .await
                .expect("an update validator set was written and must exist inside the database"),
            validator_set_updates,
            "stored update validator set was not what was expected"
        );

        // create different updates
        updates = vec![
            ValidatorUpdate {
                power: 22,
                verification_key: verification_key(1),
            },
            ValidatorUpdate {
                power: 10,
                verification_key: verification_key(3),
            },
        ];

        validator_set_updates = ValidatorSet::new_from_updates(updates);

        // write different updates
        state
            .put_validator_updates(validator_set_updates.clone())
            .expect("writing update validator set should not fail");
        assert_eq!(
            state
                .get_validator_updates()
                .await
                .expect("an update validator set was written and must exist inside the database"),
            validator_set_updates,
            "stored update validator set was not what was expected"
        );
    }

    #[tokio::test]
    async fn clear_validator_updates() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state = StateDelta::new(snapshot);

        // create update validator set
        let updates = vec![ValidatorUpdate {
            power: 10,
            verification_key: verification_key(1),
        }];
        let validator_set_updates = ValidatorSet::new_from_updates(updates);

        // put validator updates
        state
            .put_validator_updates(validator_set_updates.clone())
            .expect("writing update validator set should not fail");
        assert_eq!(
            state
                .get_validator_updates()
                .await
                .expect("an update validator set was written and must exist inside the database"),
            validator_set_updates,
            "stored update validator set was not what was expected"
        );

        // clear updates
        state.clear_validator_updates();

        // check that clear worked
        assert_eq!(
            state
                .get_validator_updates()
                .await
                .expect("if no updates have been written return empty set"),
            empty_validator_set(),
            "returned validator set different than expected"
        );
    }

    #[tokio::test]
    async fn clear_validator_updates_empty_ok() {
        let storage = cnidarium::TempStorage::new().await.unwrap();
        let snapshot = storage.latest_snapshot();
        let mut state = StateDelta::new(snapshot);

        // able to clear non-existent updates with no error
        state.clear_validator_updates();
    }

    #[tokio::test]
    async fn execute_validator_updates() {
        // create initial validator set
        let initial = vec![
            ValidatorUpdate {
                power: 1,
                verification_key: verification_key(0),
            },
            ValidatorUpdate {
                power: 2,
                verification_key: verification_key(1),
            },
            ValidatorUpdate {
                power: 3,
                verification_key: verification_key(2),
            },
        ];
        let mut initial_validator_set = ValidatorSet::new_from_updates(initial);

        // create set of updates (update key_0, remove key_1)
        let updates = vec![
            ValidatorUpdate {
                power: 5,
                verification_key: verification_key(0),
            },
            ValidatorUpdate {
                power: 0,
                verification_key: verification_key(1),
            },
        ];

        let validator_set_updates = ValidatorSet::new_from_updates(updates);

        // apply updates
        initial_validator_set.apply_updates(validator_set_updates);

        // create end state
        let updates = vec![
            ValidatorUpdate {
                power: 5,
                verification_key: verification_key(0),
            },
            ValidatorUpdate {
                power: 3,
                verification_key: verification_key(2),
            },
        ];
        let validator_set_endstate = ValidatorSet::new_from_updates(updates);

        // check updates applied correctly
        assert_eq!(
            initial_validator_set, validator_set_endstate,
            "validator set apply updates did not behave as expected"
        );
    }
}
