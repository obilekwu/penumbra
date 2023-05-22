use anyhow::{Context, Result};
use async_trait::async_trait;
use penumbra_chain::component::StateReadExt as _;
use penumbra_storage::{StateRead, StateWrite};

use std::sync::Arc;

use penumbra_proto::DomainType;

use crate::{
    action_handler::ActionHandler, component::StakingImpl as _, rate::RateData, validator,
    StateReadExt as _,
};

#[async_trait]
impl ActionHandler for validator::Definition {
    type CheckStatelessContext = ();
    async fn check_stateless(&self, _context: ()) -> Result<()> {
        // First, check the signature:
        let definition_bytes = self.validator.encode_to_vec();
        self.validator
            .identity_key
            .0
            .verify(&definition_bytes, &self.auth_sig)
            .context("validator definition signature failed to verify")?;

        // TODO(hdevalence) -- is this duplicated by the check during parsing?
        // Check that the funding streams do not exceed 100% commission (10000bps)
        let total_funding_bps = self
            .validator
            .funding_streams
            .iter()
            .map(|fs| fs.rate_bps() as u64)
            .sum::<u64>();

        if total_funding_bps > 10000 {
            return Err(anyhow::anyhow!(
                "validator defined {} bps of funding streams, greater than 10000bps = 100%",
                total_funding_bps
            ));
        }

        Ok(())
    }

    async fn check_stateful<S: StateRead + 'static>(&self, state: Arc<S>) -> Result<()> {
        let v = self;

        // Check that the sequence numbers of the updated validators is correct...
        // Check whether we are redefining an existing validator.
        if let Some(existing_v) = state.validator(&v.validator.identity_key).await? {
            // Ensure that the highest existing sequence number is less than
            // the new sequence number.
            let current_seq = existing_v.sequence_number;
            if v.validator.sequence_number <= current_seq {
                return Err(anyhow::anyhow!(
                    "expected sequence numbers to be increasing: current sequence number is {}",
                    current_seq
                ));
            }
        }

        // Check whether the consensus key has already been used by another validator.
        if let Some(existing_v) = state
            .validator_by_consensus_key(&v.validator.consensus_key)
            .await?
        {
            if v.validator.identity_key != existing_v.identity_key {
                // This is a new validator definition, but the consensus
                // key it declares is already in use by another validator.
                //
                // Rejecting this is important for two reasons:
                //
                // 1. It prevents someone from declaring an (app-level)
                // validator that "piggybacks" on the actual behavior of someone
                // else's validator.
                //
                // 2. If we submit a validator update to Tendermint that
                // includes duplicate consensus keys, Tendermint gets confused
                // and hangs.
                return Err(anyhow::anyhow!(
                    "consensus key {:?} is already in use by validator {}",
                    v.validator.consensus_key,
                    existing_v.identity_key,
                ));
            }
        }

        // the validator definition has now passed all verification checks
        Ok(())
    }

    async fn execute<S: StateWrite>(&self, mut state: S) -> Result<()> {
        let v = self;

        let cur_epoch = state.get_current_epoch().await.unwrap();

        if state
            .validator(&v.validator.identity_key)
            .await
            .unwrap()
            .is_some()
        {
            // This is an existing validator definition.
            state.update_validator(v.validator.clone()).await.unwrap();
        } else {
            // This is a new validator definition.
            // Set the default rates and state.
            let validator_key = v.validator.identity_key;

            // Delegations require knowing the rates for the
            // next epoch, so pre-populate with 0 reward => exchange rate 1 for
            // the current and next epochs.
            let cur_rate_data = RateData {
                identity_key: validator_key,
                epoch_index: cur_epoch.index,
                validator_reward_rate: 0,
                validator_exchange_rate: 1_0000_0000, // 1 represented as 1e8
            };
            let next_rate_data = RateData {
                identity_key: validator_key,
                epoch_index: cur_epoch.index + 1,
                validator_reward_rate: 0,
                validator_exchange_rate: 1_0000_0000, // 1 represented as 1e8
            };

            state
                .add_validator(v.validator.clone(), cur_rate_data, next_rate_data)
                .await
                .unwrap();
        }

        Ok(())
    }
}