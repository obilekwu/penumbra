use std::{collections::HashMap, time::Duration};

use anyhow::{anyhow, Context, Result};
use penumbra_keys::Address;
use penumbra_num::Amount;
use rand::rngs::OsRng;
use tokio::sync::mpsc::{self, error::TryRecvError};

use crate::{participant::Participant, storage::Storage};

/// Wait time of 10 minutes
const CONTRIBUTION_TIME_SECS: u64 = 10 * 60;

pub struct Coordinator {
    storage: Storage,
    participants: HashMap<Address, (Participant, Amount)>,
    new_participant_rx: mpsc::Receiver<(Participant, Amount)>,
}

impl Coordinator {
    pub fn new(storage: Storage) -> (Self, mpsc::Sender<(Participant, Amount)>) {
        let (new_participant_tx, new_participant_rx) = mpsc::channel(9001);
        (
            Self {
                storage,
                participants: HashMap::new(),
                new_participant_rx,
            },
            new_participant_tx,
        )
    }

    pub async fn run(mut self) -> Result<()> {
        loop {
            tracing::debug!(
                participant_count = self.participants.len(),
                "top of coordinator loop"
            );
            // 0. Wait for the first participant
            if self.participants.is_empty() {
                self.wait_for_participant().await?;
            }
            // 1. Check for new connections, but don't wait for them.
            self.dequeue_participants()?;
            // 2. Score connections
            self.prune_participants();
            let ranked = self.score_participants();
            // In theory ranked could've become empty for some reason in the meantime
            if ranked.is_empty() {
                continue;
            }
            // 3. Update everyone on status.
            let contributor = ranked[0];
            let contributor_bid = self.participants[&contributor].1;
            self.inform_participants_of_status(&ranked, contributor_bid)
                .await;
            // 5. Get contribution, or error if they don't respond quickly enough
            self.contribute(contributor).await?;
            // 6. Remove from pool regardless of what happened
            self.participants.remove(&contributor);
        }
    }
}

impl Coordinator {
    async fn wait_for_participant(&mut self) -> Result<()> {
        if let Some((participant, bid)) = self.new_participant_rx.recv().await {
            let address = participant.address();
            tracing::info!(?address, "has been added as a participant");
            self.participants.insert(address, (participant, bid));
            Ok(())
        } else {
            Err(anyhow!("Participant queue was closed"))
        }
    }

    fn dequeue_participants(&mut self) -> Result<()> {
        loop {
            match self.new_participant_rx.try_recv() {
                Ok((participant, bid)) => {
                    let address = participant.address();
                    tracing::info!(?address, "has been added as a participant");
                    self.participants.insert(address, (participant, bid));
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(e @ TryRecvError::Disconnected) => {
                    return Err(e).with_context(|| "Channel with incoming connections was closed")
                }
            }
        }
    }

    fn prune_participants(&mut self) {
        self.participants
            .retain(|_, (connection, _)| connection.is_live());
    }

    fn score_participants(&self) -> Vec<Address> {
        let mut out: Vec<Address> = self.participants.keys().cloned().collect();
        out.sort_by_cached_key(|addr| self.participants[addr].1);
        out
    }

    async fn inform_participants_of_status(&mut self, ranked: &[Address], contributor_bid: Amount) {
        for (i, address) in ranked.iter().enumerate() {
            let (connection, bid) = self
                .participants
                .get(address)
                .expect("Ranked participants are chosen from the set of connections");
            if let Err(e) =
                connection.try_notify(i as u32, ranked.len() as u32, contributor_bid, *bid)
            {
                tracing::info!(?e, ?address, "pruning connection that we failed to notify");
                self.participants.remove(address);
            };
        }
    }

    #[tracing::instrument(skip(self))]
    async fn contribute(&mut self, contributor: Address) -> Result<()> {
        match tokio::time::timeout(
            Duration::from_secs(CONTRIBUTION_TIME_SECS),
            self.contribute_inner(contributor),
        )
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Err(_) => {
                tracing::info!("timeout when asking for contribution");
                Ok(())
            }
            Ok(Err(e)) => Err(e),
        }
    }

    #[tracing::instrument(skip(self))]
    async fn contribute_inner(&mut self, contributor: Address) -> Result<()> {
        let parent = self.storage.current_crs().await?;
        let (participant, _) = self
            .participants
            .get_mut(&contributor)
            .expect("We ask for the contributions of participants we're connected to");
        let maybe = participant.contribute(&parent).await?;
        if let Some(unvalidated) = maybe {
            if let Some(contribution) =
                unvalidated.validate(&mut OsRng, &self.storage.root().await?)
            {
                if contribution.is_linked_to(&parent) {
                    self.storage
                        .commit_contribution(contributor, contribution)
                        .await?;
                    participant
                        .confirm(self.storage.current_slot().await?)
                        .await?;
                    return Ok(());
                }
            }
        }
        self.storage.strike(&contributor).await?;
        return Ok(());
    }
}
