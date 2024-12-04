use reqwest::Client;
use alloy_rpc_types_beacon::header::HeaderResponse;
use alloy_rpc_types_beacon::events::light_client_finality::SyncAggregate;
use serde_json::Value;
use crate::Error;
use itertools::Itertools;
use crate::types::SyncCommitteeValidatorPubs;

/// A client for interacting with the Ethereum Beacon Chain RPC endpoints.
/// Provides methods to fetch headers, sync aggregates, and validator information.
pub(crate) struct BeaconRpcClient {
    provider: Client,
    pub rpc_url: String,
}

impl BeaconRpcClient {
    /// Creates a new BeaconRpcClient instance.
    /// 
    /// # Arguments
    /// * `rpc_url` - The base URL for the Beacon Chain RPC endpoint
    pub fn new(rpc_url: String) -> Self {
        Self { provider: reqwest::Client::new(), rpc_url }
    }

    /// Makes an HTTP GET request and returns the JSON response.
    /// This is a helper method used by all other RPC calls.
    async fn get_json(&self, route: &str) -> Result<Value, Error> {
        let url = format!("{}/{}", self.rpc_url, route);
        self.provider
            .get(url)
            .send()
            .await
            .map_err(Error::RpcError)?
            .json()
            .await
            .map_err(Error::RpcError)
    }

    /// Fetches the beacon chain header for a specific slot.
    /// This provides information about the block at the given slot number.
    /// Returns Error::BlockNotFound if no block exists at the specified slot.
    pub async fn get_header(&self, slot: u64) -> Result<HeaderResponse, Error> {
        let json = self.get_json(&format!("eth/v1/beacon/headers/{}", slot)).await?;
        
        // Check for 404 NOT_FOUND error
        if let Some(code) = json.get("code").and_then(|c| c.as_i64()) {
            if code == 404 {
                return Err(Error::EmptySlotDetected(slot));
            }
        }
        
        serde_json::from_value(json).map_err(Error::DeserializeError)
    }

    /// Fetches the sync aggregate from the block AFTER the specified slot.
    /// Note: This intentionally fetches slot + 1 because sync aggregates reference 
    /// the previous slot's header.
    pub async fn get_sync_aggregate(&self, slot: u64) -> Result<SyncAggregate, Error> {
        let json = self.get_json(&format!("eth/v2/beacon/blocks/{}", slot + 1)).await?;
        serde_json::from_value(json["data"]["message"]["body"]["sync_aggregate"].clone())
            .map_err(Error::DeserializeError)
    }

    /// Retrieves the list of validator indices that are part of the sync committee
    /// for the specified slot.
    /// 
    /// The returned indices can be used to fetch the corresponding public keys
    /// using fetch_validator_pubkeys().
    async fn fetch_sync_committee_indexes(&self, slot: u64) -> Result<Vec<u64>, Error> {
        let json = self.get_json(&format!("eth/v1/beacon/states/{}/sync_committees", slot)).await?;
        
        // Parse the array of validator indices from the JSON response
        json["data"]["validators"]
            .as_array()
            .ok_or(Error::FetchSyncCommitteeError)?
            .iter()
            .map(|v| v.as_str()
                .ok_or(Error::FetchSyncCommitteeError)
                .and_then(|s| s.parse().map_err(|_| Error::FetchSyncCommitteeError)))
            .collect()
    }

    /// Fetches the public keys for a list of validator indices.
    /// 
    /// # Arguments
    /// * `indexes` - Array of validator indices to look up
    /// 
    /// # Returns
    /// A vector of public keys in the same order as the input indices.
    /// If a validator index is not found, returns an error.
    async fn fetch_validator_pubkeys(&self, indexes: &[u64]) -> Result<Vec<String>, Error> {
        // Construct query string with all validator indices
        let query = indexes.iter().map(|i| format!("id={}", i)).join("&");
        let json = self.get_json(&format!("eth/v1/beacon/states/head/validators?{}", query)).await?;
        
        let validators = json["data"].as_array()
            .ok_or(Error::FetchSyncCommitteeError)?;

        // Match returned validators with requested indices and extract public keys
        indexes.iter()
            .map(|index| {
                validators.iter()
                    .find(|v| v["index"].as_str()
                        .and_then(|i| i.parse::<u64>().ok()) == Some(*index))
                    .and_then(|v| v["validator"]["pubkey"].as_str())
                    .map(String::from)
                    .ok_or(Error::FetchSyncCommitteeError)
            })
            .collect()
    }

    /// Fetches the public keys of validators in the sync committee for a given slot.
    /// Note: This actually fetches data for the next slot (slot + 1).
    /// 
    /// # Arguments
    /// * `slot` - The slot number to fetch the sync committee validator public keys for
    /// 
    /// # Returns
    /// Returns a `SyncCommitteeValidatorPubs` containing the public keys of all validators
    /// in the sync committee.
    pub async fn get_sync_committee_validator_pubs(&self, slot: u64) -> Result<SyncCommitteeValidatorPubs, Error> {
        let slot = slot + 1;
        let indexes = self.fetch_sync_committee_indexes(slot).await?;
        let pubkeys = self.fetch_validator_pubkeys(&indexes).await?;
        Ok(pubkeys.into())
    }
}