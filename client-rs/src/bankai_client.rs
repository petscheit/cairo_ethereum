use crate::{
    contract_init::ContractInitializationData,
    epoch_update::EpochUpdate,
    state::Error,
    sync_committee::SyncCommitteeUpdate,
    utils::{
        atlantic_client::AtlanticClient, rpc::BeaconRpcClient, starknet_client::StarknetClient,
    },
    BankaiConfig,
};
use dotenv::from_filename;
use std::env;
use tracing::info;

#[derive(Debug)]
pub struct BankaiClient {
    pub client: BeaconRpcClient,
    pub starknet_client: StarknetClient,
    pub config: BankaiConfig,
    pub atlantic_client: AtlanticClient,
}

impl BankaiClient {
    pub async fn new() -> Self {
        from_filename(".env.sepolia").ok();
        let config = BankaiConfig::default();
        Self {
            client: BeaconRpcClient::new(env::var("BEACON_RPC_URL").unwrap()),
            starknet_client: StarknetClient::new(
                env::var("STARKNET_RPC_URL").unwrap().as_str(),
                env::var("STARKNET_ADDRESS").unwrap().as_str(),
                env::var("STARKNET_PRIVATE_KEY").unwrap().as_str(),
            )
            .await
            .unwrap(),
            atlantic_client: AtlanticClient::new(
                config.atlantic_endpoint.clone(),
                env::var("ATLANTIC_API_KEY").unwrap(),
            ),
            config,
        }
    }

    pub async fn get_sync_committee_update(
        &self,
        mut slot: u64,
    ) -> Result<SyncCommitteeUpdate, Error> {
        let mut attempts = 0;
        const MAX_ATTEMPTS: u8 = 3;

        // Before we start generating the proof, we ensure the slot was not missed
        let _header = loop {
            match self.client.get_header(slot).await {
                Ok(header) => break header,
                Err(Error::EmptySlotDetected(_)) => {
                    attempts += 1;
                    if attempts >= MAX_ATTEMPTS {
                        return Err(Error::EmptySlotDetected(slot));
                    }
                    slot += 1;
                    info!(
                        "Empty slot detected! Attempt {}/{}. Fetching slot: {}",
                        attempts, MAX_ATTEMPTS, slot
                    );
                }
                Err(e) => return Err(e), // Propagate other errors immediately
            }
        };

        let proof: SyncCommitteeUpdate = SyncCommitteeUpdate::new(&self.client, slot).await?;

        Ok(proof)
    }

    pub async fn get_epoch_proof(&self, slot: u64) -> Result<EpochUpdate, Error> {
        let epoch_proof = EpochUpdate::new(&self.client, slot).await?;
        Ok(epoch_proof)
    }

    #[cfg(feature = "cli")]
    pub async fn get_contract_initialization_data(
        &self,
        slot: u64,
        config: &BankaiConfig,
    ) -> Result<ContractInitializationData, Error> {
        let contract_init = ContractInitializationData::new(&self.client, slot, config).await?;
        Ok(contract_init)
    }
}