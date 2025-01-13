mod config;
mod constants;
mod contract_init;
pub mod epoch_batch;
mod epoch_update;
mod execution_header;
mod helpers;
mod sync_committee;
mod traits;
mod utils;

//use alloy_primitives::TxHash;
use alloy_primitives::FixedBytes;
use alloy_rpc_types_beacon::events::HeadEvent;
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    //http::{header, StatusCode},
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use config::BankaiConfig;
use constants::SLOTS_PER_EPOCH;
use contract_init::ContractInitializationData;
use dotenv::from_filename;
use epoch_update::{EpochProof, EpochUpdate};
use num_traits::cast::ToPrimitive;
use postgres_types::{FromSql, ToSql};
use reqwest;
use serde_json::json;
use starknet::core::types::Felt;
use std::env;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task;
use tokio_postgres::{Client, NoTls};
use tokio_stream::StreamExt;
use tracing::{error, info, trace, warn, Level};
use tracing_subscriber::FmtSubscriber;
use traits::Provable;
use utils::{atlantic_client::AtlanticClient, cairo_runner::CairoRunner};
use utils::{
    rpc::BeaconRpcClient,
    //  bankai_client::BankaiClient,
    starknet_client::{StarknetClient, StarknetError},
};
//use std::error::Error as StdError;
use epoch_batch::EpochUpdateBatch;
use std::fmt;
use std::net::SocketAddr;
use sync_committee::SyncCommitteeUpdate;
use tokio::time::Duration;
use uuid::Uuid;

impl std::fmt::Display for StarknetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StarknetError::ProviderError(err) => write!(f, "Provider error: {}", err),
            StarknetError::AccountError(msg) => write!(f, "Account error: {}", msg),
        }
    }
}

impl std::error::Error for StarknetError {}

#[derive(Debug, FromSql, ToSql, Clone)]
#[postgres(name = "job_status")]
enum JobStatus {
    #[postgres(name = "CREATED")]
    Created,
    #[postgres(name = "FETCHED_PROOF")]
    FetchedProof,
    #[postgres(name = "PIE_GENERATED")]
    PieGenerated,
    #[postgres(name = "OFFCHAIN_PROOF_REQUESTED")]
    OffchainProofRequested,
    #[postgres(name = "OFFCHAIN_PROOF_RETRIEVED")]
    OffchainProofRetrieved,
    #[postgres(name = "WRAP_PROOF_REQUESTED")]
    WrapProofRequested,
    #[postgres(name = "WRAPPED_PROOF_DONE")]
    WrappedProofDone,
    #[postgres(name = "READY_TO_BROADCAST")]
    ReadyToBroadcast,
    #[postgres(name = "PROOF_VERIFY_CALLED_ONCHAIN")]
    ProofVerifyCalledOnchain,
    #[postgres(name = "VERIFIED_FACT_REGISTERED")]
    VerifiedFactRegistered,
    #[postgres(name = "ERROR")]
    Error,
    #[postgres(name = "CANCELLED")]
    Cancelled,
}

impl ToString for JobStatus {
    fn to_string(&self) -> String {
        match self {
            JobStatus::Created => "CREATED".to_string(),
            JobStatus::FetchedProof => "FETCHED_PROOF".to_string(),
            JobStatus::PieGenerated => "PIE_GENERATED".to_string(),
            JobStatus::OffchainProofRequested => "OFFCHAIN_PROOF_REQUESTED".to_string(),
            JobStatus::OffchainProofRetrieved => "OFFCHAIN_PROOF_RETRIEVED".to_string(),
            JobStatus::WrapProofRequested => "WRAP_PROOF_REQUESTED".to_string(),
            JobStatus::WrappedProofDone => "WRAPPED_PROOF_DONE".to_string(),
            JobStatus::ReadyToBroadcast => "READY_TO_BROADCAST".to_string(),
            JobStatus::ProofVerifyCalledOnchain => "PROOF_VERIFY_CALLED_ONCHAIN".to_string(),
            JobStatus::VerifiedFactRegistered => "VERIFIED_FACT_REGISTERED".to_string(),
            JobStatus::Cancelled => "CANCELLED".to_string(),
            JobStatus::Error => "ERROR".to_string(),
        }
    }
}

#[derive(Debug, FromSql, ToSql, Clone)]
enum JobType {
    EpochUpdate,
    EpochBatchUpdate,
    SyncComiteeUpdate,
}

#[derive(Debug, FromSql, ToSql)]
enum AtlanticJobType {
    ProofGeneration,
    ProofWrapping,
}

#[derive(Debug)]
pub enum Error {
    InvalidProof,
    RpcError(reqwest::Error),
    DeserializeError(String),
    IoError(std::io::Error),
    StarknetError(StarknetError),
    BlockNotFound,
    FetchSyncCommitteeError,
    FailedFetchingBeaconState,
    InvalidBLSPoint,
    MissingRpcUrl,
    EmptySlotDetected(u64),
    RequiresNewerEpoch(Felt),
    CairoRunError(String),
    AtlanticError(reqwest::Error),
    InvalidResponse(String),
    PoolingTimeout(String),
    InvalidMerkleTree,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidProof => write!(f, "Invalid proof provided"),
            Error::RpcError(err) => write!(f, "RPC error: {}", err),
            Error::DeserializeError(msg) => write!(f, "Deserialization error: {}", msg),
            Error::IoError(err) => write!(f, "I/O error: {}", err),
            Error::StarknetError(err) => write!(f, "Starknet error: {}", err),
            Error::BlockNotFound => write!(f, "Block not found"),
            Error::FetchSyncCommitteeError => write!(f, "Failed to fetch sync committee"),
            Error::FailedFetchingBeaconState => write!(f, "Failed to fetch beacon state"),
            Error::InvalidBLSPoint => write!(f, "Invalid BLS point"),
            Error::MissingRpcUrl => write!(f, "Missing RPC URL"),
            Error::EmptySlotDetected(slot) => write!(f, "Empty slot detected: {}", slot),
            Error::RequiresNewerEpoch(felt) => write!(f, "Requires newer epoch: {}", felt),
            Error::CairoRunError(msg) => write!(f, "Cairo run error: {}", msg),
            Error::AtlanticError(err) => write!(f, "Atlantic RPC error: {}", err),
            Error::InvalidResponse(msg) => write!(f, "Invalid response: {}", msg),
            Error::PoolingTimeout(msg) => write!(f, "Pooling timeout: {}", msg),
            Error::InvalidMerkleTree => write!(f, "Invalid Merkle Tree"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::RpcError(err) => Some(err),
            Error::IoError(err) => Some(err),
            Error::StarknetError(err) => Some(err),
            Error::AtlanticError(err) => Some(err),
            _ => None, // No underlying source for other variants
        }
    }
}

impl From<StarknetError> for Error {
    fn from(e: StarknetError) -> Self {
        Error::StarknetError(e)
    }
}

#[derive(Clone, Debug)]
struct Job {
    job_id: Uuid,
    job_type: JobType,
    job_status: JobStatus,
    slot: u64,
}

#[derive(Clone, Debug)]
struct AppState {
    db_client: Arc<Client>,
    tx: mpsc::Sender<Job>,
    bankai: Arc<BankaiClient>,
}

#[derive(Debug)]
struct BankaiClient {
    client: BeaconRpcClient,
    starknet_client: StarknetClient,
    config: BankaiConfig,
    atlantic_client: AtlanticClient,
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

    pub async fn get_contract_initialization_data(
        &self,
        slot: u64,
        config: &BankaiConfig,
    ) -> Result<ContractInitializationData, Error> {
        let contract_init = ContractInitializationData::new(&self.client, slot, config).await?;
        Ok(contract_init)
    }
}

fn check_env_vars() -> Result<(), String> {
    let required_vars = [
        "BEACON_RPC_URL",
        "STARKNET_RPC_URL",
        "STARKNET_ADDRESS",
        "STARKNET_PRIVATE_KEY",
        "ATLANTIC_API_KEY",
        "PROOF_REGISTRY",
        "POSTGRESQL_HOST",
        "POSTGRESQL_USER",
        "POSTGRESQL_PASSWORD",
        "POSTGRESQL_DB_NAME",
        "RPC_LISTEN_HOST",
        "RPC_LISTEN_PORT",
    ];

    for &var in &required_vars {
        if env::var(var).is_err() {
            return Err(format!("Environment variable `{}` is not set", var));
        }
    }

    Ok(())
}

// Since beacon chain RPCs have different response structure (quicknode responds different than nidereal) we use this event extraction logic
fn extract_json(event_text: &str) -> Option<String> {
    for line in event_text.lines() {
        if line.starts_with("data:") {
            // Extract the JSON after "data:"
            return Some(line.trim_start_matches("data:").trim().to_string());
        }
    }
    None
}

#[tokio::main]
//async fn main() {
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load .env.sepolia file
    from_filename(".env.sepolia").ok();

    let slot_listener_toggle = true;

    let subscriber = FmtSubscriber::builder()
        //.with_max_level(Level::DEBUG)
        .with_max_level(Level::INFO)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Validate environment variables
    check_env_vars().map_err(|e| {
        error!("Error: {}", e);
        std::process::exit(1); // Exit if validation fails
    });

    info!("Starting Bankai light-client daemon...");

    //let database_host = env::var("DATABASE_HOST").expect("DATABASE_HOST must be set");
    let (tx, mut rx): (mpsc::Sender<Job>, mpsc::Receiver<Job>) = mpsc::channel(32);

    //let (tx, mut rx) = mpsc::channel(32);

    let connection_string = "host=localhost user=meow password=meow dbname=bankai";
    // let connection_string = format!(
    //     "host={} user={} password={} dbname={}",
    //     env::var("POSTGRESQL_HOST").unwrap().as_str(),
    //     env::var("POSTGRESQL_USER").unwrap().as_str(),
    //     env::var("POSTGRESQL_PASSWORD").unwrap().as_str(),
    //     env::var("POSTGRESQL_DB_NAME").unwrap().as_str()
    // );
    let _connection_result: Result<
        (
            Client,
            tokio_postgres::Connection<tokio_postgres::Socket, tokio_postgres::tls::NoTlsStream>,
        ),
        tokio_postgres::Error,
    > = tokio_postgres::connect(connection_string, NoTls).await;

    let db_client = match tokio_postgres::connect(connection_string, NoTls).await {
        Ok((client, connection)) => {
            // Spawn a task to manage the connection
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    eprintln!("Connection error: {}", e);
                }
            });

            info!("Connected to the database successfully!");

            // Wrap the client in an Arc for shared ownership
            Arc::new(client)
        }
        Err(err) => {
            error!("Failed to connect to the database: {}", err);
            std::process::exit(1); // Exit with a non-zero status code
        }
    };

    //let db_client_for_task = Arc::new(db_client);

    let bankai = Arc::new(BankaiClient::new().await);
    // Clone the Arc for use in async task
    //let bankai_for_task = Arc::clone(&bankai);

    // Beacon node endpoint construction for ervents
    let events_endpoint = format!(
        "{}/eth/v1/events?topics=head",
        env::var("BEACON_RPC_URL").unwrap().as_str()
    );
    //let events_endpoint = format!("{}/eth/v1/events?topics=head", beacon_node_url)
    let db_client_for_state = db_client.clone();
    let db_client_for_listener = db_client.clone();
    let bankai_for_state = bankai.clone();
    let bankai_for_listener = bankai.clone();

    //Spawn a background task to process jobs
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let job_id = job.job_id;
            let db_clone = Arc::clone(&db_client);
            let bankai_clone = Arc::clone(&bankai);

            // Spawn a *new task* for each job — now they can run in parallel
            tokio::spawn(async move {
                match process_job(job, db_clone.clone(), bankai_clone.clone()).await {
                    Ok(_) => {
                        info!("Job {} completed successfully", job_id);
                    }
                    Err(e) => {
                        update_job_status(&db_clone, job_id, JobStatus::Error).await;
                        error!("Error processing job {}: {}", job_id, e);
                    }
                }
            });
        }
    });

    // let db_client_for_task =db_client.clone();

    let tx_for_task = tx.clone();

    let app_state: AppState = AppState {
        db_client: db_client_for_state,
        tx,
        bankai: bankai_for_state,
    };

    let app = Router::new()
        .route("/status", get(handle_get_status))
        //.route("/get-epoch-proof/:slot", get(handle_get_epoch_proof))
        //.route("/get-committee-hash/:committee_id", get(handle_get_committee_hash))
        .route(
            "/get_merkle_paths_for_epoch/:epoch_id",
            get(handle_get_merkle_paths_for_epoch),
        )
        .route(
            "/debug/get-epoch-update/:slot",
            get(handle_get_epoch_update),
        )
        .route(
            "/debug/get-latest-verified-slot",
            get(handle_get_latest_verified_slot),
        )
        // .route("/debug/get-job-status", get(handle_get_job_status))
        // .route("/get-merkle-inclusion-proof", get(handle_get_merkle_inclusion_proof))
        .layer(DefaultBodyLimit::disable())
        .with_state(app_state);

    let addr = "0.0.0.0:3000".parse::<SocketAddr>()?;
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    info!("Bankai RPC HTTP server is listening on http://{}", addr);

    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Listen for the new slots on BeaconChain
    // Create an HTTP client
    let http_stream_client = reqwest::Client::new();

    // Send the request to the Beacon node
    let response = http_stream_client
        .get(&events_endpoint)
        .send()
        .await
        .unwrap();

    //let db_client = Arc::new(&db_client);
    if slot_listener_toggle {
        task::spawn({
            async move {
                // Check if response is successful; if not, bail out early
                // TODO: need to implement resilience and potentialy use multiple providers (implement something like fallbackprovider functionality in ethers), handle reconnection if connection is lost for various reasons
                if !response.status().is_success() {
                    error!("Failed to connect: HTTP {}", response.status());
                    return;
                }

                info!("Listening for new slots, epochs and sync committee updates...");
                let mut stream = response.bytes_stream();

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            if let Ok(event_text) = String::from_utf8(bytes.to_vec()) {
                                // Preprocess the event text
                                if let Some(json_data) = extract_json(&event_text) {
                                    match serde_json::from_str::<HeadEvent>(&json_data) {
                                        Ok(parsed_event) => {
                                            //let is_node_in_sync = false;
                                            let bankai = bankai_for_listener.clone();

                                            let epoch_id =
                                                helpers::slot_to_epoch_id(parsed_event.slot);
                                            let sync_committee_id =
                                                helpers::slot_to_sync_committee_id(
                                                    parsed_event.slot,
                                                );

                                            info!(
                                                "New slot event detected: {} |  Block: {} | Epoch: {} | Sync committee: {} | Is epoch transition: {}",
                                                parsed_event.slot, parsed_event.block, epoch_id, sync_committee_id, parsed_event.epoch_transition
                                            );

                                            let latest_epoch_slot = bankai
                                                .starknet_client
                                                .get_latest_epoch_slot(&bankai.config)
                                                .await
                                                .unwrap()
                                                .to_u64()
                                                .unwrap();

                                            let latest_verified_epoch_id =
                                                helpers::slot_to_epoch_id(latest_epoch_slot);
                                            let epochs_behind = epoch_id - latest_verified_epoch_id;

                                            // We getting the last slot in progress to determine next slots to prove
                                            let mut last_slot_in_progress: u64 = 1000000;
                                            match get_latest_slot_id_in_progress(
                                                &db_client_for_listener.clone(),
                                            )
                                            .await
                                            {
                                                Ok(Some(slot)) => {
                                                    last_slot_in_progress = slot.to_u64().unwrap();
                                                    info!(
                                                        "Latest in progress slot: {}  Epoch: {}",
                                                        last_slot_in_progress,
                                                        helpers::slot_to_epoch_id(
                                                            last_slot_in_progress
                                                        )
                                                    );
                                                }
                                                Ok(None) => {
                                                    warn!("No any in progress slot");
                                                }
                                                Err(e) => {
                                                    error!(
                                                        "Error while getting latest in progress slot ID: {}",
                                                        e
                                                    );
                                                }
                                            }

                                            if epochs_behind > constants::TARGET_BATCH_SIZE {
                                                // is_node_in_sync = true;

                                                warn!(
                                                    "Bankai is out of sync now. Node is {} epochs behind network. Current Beacon Chain epoch: {} Latest verified epoch: {} Sync in progress...",
                                                    epochs_behind, epoch_id, latest_verified_epoch_id
                                                );

                                                match run_batch_update_job(
                                                    db_client_for_listener.clone(),
                                                    last_slot_in_progress
                                                        + (constants::SLOTS_PER_EPOCH
                                                            * constants::TARGET_BATCH_SIZE),
                                                    tx_for_task.clone(),
                                                )
                                                .await
                                                {
                                                    // Insert new job record to DB
                                                    Ok(()) => {}
                                                    Err(e) => {}
                                                };

                                                // let epoch_update = EpochUpdateBatch::new_by_slot(
                                                //     &bankai,
                                                //     &db_client_for_listener.clone(),
                                                //     last_slot_in_progress
                                                //         + constants::SLOTS_PER_EPOCH,
                                                // )
                                                // .await?;
                                            }

                                            // Check if sync committee update is needed
                                            //sync_committee_id

                                            if latest_epoch_slot
                                                % constants::SLOTS_PER_SYNC_COMMITTEE
                                                == 0
                                            {}

                                            //return;

                                            // When we doing EpochBatchUpdate the slot is latest_batch_output
                                            // So for each batch update we takin into account effectiviely the latest slot from given batch

                                            let db_client = db_client_for_listener.clone();

                                            // evaluete_jobs_statuses();
                                            // broadcast_ready_jobs();

                                            // We can do all circuit computations up to latest slot in advance, but the onchain broadcasts must be send in correct order
                                            // By correct order mean that within the same sync committe the epochs are not needed to be broadcasted in order
                                            // but the order of sync_commite_update->epoch_update must be correct, we firstly need to have correct sync committe veryfied
                                            // before we verify epoch "belonging" to this sync committee

                                            if parsed_event.epoch_transition {
                                                info!("Beacon Chain epoch transition detected. New epoch: {} | Starting processing epoch proving...", epoch_id);

                                                // Check also now if slot is the moment of switch to new sync committee set
                                                if parsed_event.slot
                                                    % constants::SLOTS_PER_SYNC_COMMITTEE
                                                    == 0
                                                {
                                                    info!("Beacon Chain sync committee rotation occured. Slot {} | Sync committee id: {}", parsed_event.slot, sync_committee_id);
                                                }

                                                let job_id = Uuid::new_v4();
                                                let job = Job {
                                                    job_id: job_id.clone(),
                                                    job_type: JobType::EpochBatchUpdate,
                                                    job_status: JobStatus::Created,
                                                    slot: parsed_event.slot, // It is the last slot for given batch
                                                };

                                                let db_client = db_client_for_listener.clone();
                                                match create_job(db_client, job.clone()).await {
                                                    // Insert new job record to DB
                                                    Ok(()) => {
                                                        // Handle success
                                                        info!(
                                                            "Job created successfully with ID: {}",
                                                            job_id
                                                        );
                                                        if tx_for_task.send(job).await.is_err() {
                                                            error!("Failed to send job.");
                                                        }
                                                        // If starting committee update job, first ensule that the corresponding slot is registered in contract
                                                    }
                                                    Err(e) => {
                                                        // Handle the error
                                                        error!("Error creating job: {}", e);
                                                    }
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            warn!("Failed to parse JSON data: {}", err);
                                        }
                                    }
                                } else {
                                    warn!("No valid JSON data found in event: {}", event_text);
                                }
                            }
                        }
                        Err(err) => {
                            warn!("Error reading event stream: {}", err);
                        }
                    }
                }
            }
        });
    }

    // Wait for the server task to finish
    server_task.await?;

    Ok(())
}

async fn run_batch_update_job(
    db_client: Arc<Client>,
    slot: u64,
    tx: mpsc::Sender<Job>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let job_id = Uuid::new_v4();
    let job = Job {
        job_id: job_id.clone(),
        job_type: JobType::EpochBatchUpdate,
        job_status: JobStatus::Created,
        slot,
    };

    match create_job(db_client, job.clone()).await {
        // Insert new job record to DB
        Ok(()) => {
            // Handle success
            info!("Job created successfully with ID: {}", job_id);
            if tx.send(job).await.is_err() {
                return Err("Failed to send job".into());
            }
            // If starting committee update job, first ensule that the corresponding slot is registered in contract
            Ok(())
        }
        Err(e) => {
            // Handle the error
            return Err(e.into());
        }
    }
}

async fn set_atlantic_job_queryid(
    client: &Client,
    job_id: Uuid,
    batch_id: String,
    atlantic_job_type: AtlanticJobType,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match atlantic_job_type {
        AtlanticJobType::ProofGeneration => {
            client
            .execute(
                "UPDATE jobs SET atlantic_batch_id_proof_generation = $1, updated_at = NOW() WHERE job_uuid = $2",
                &[&batch_id.to_string(), &job_id],
            )
            .await?;
        }
        AtlanticJobType::ProofWrapping => {
            client
            .execute(
                "UPDATE jobs SET atlantic_batch_id_proof_wrapping = $1, updated_at = NOW() WHERE job_uuid = $2",
                &[&batch_id.to_string(), &job_id],
            )
            .await?;
        } // _ => {
          //     println!("Unk", status);
          // }
    }

    Ok(())
}

async fn insert_verified_epoch(
    client: &Client,
    epoch_id: u64,
    epoch_proof: EpochProof,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "INSERT INTO verified_epoch (epoch_id, header_root, state_root, n_signers) VALUES ($1)",
            &[
                &epoch_id.to_string(),
                &epoch_proof.header_root.to_string(),
                &epoch_proof.state_root.to_string(),
                &epoch_proof.n_signers.to_string(),
            ],
        )
        .await?;

    Ok(())
}

async fn insert_verified_sync_committee(
    client: &Client,
    sync_committee_id: u64,
    sync_committee_hash: FixedBytes<32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "INSERT INTO verified_sync_committee (sync_committee_id, sync_committee_hash) VALUES ($1)",
            &[&sync_committee_id.to_string(), &sync_committee_hash.to_string()],
        )
        .await?;

    Ok(())
}

async fn create_job(
    client: Arc<Client>,
    job: Job,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "INSERT INTO jobs (job_uuid, job_status, slot, type) VALUES ($1, $2, $3, $4)",
            &[
                &job.job_id,
                &job.job_status.to_string(),
                &(job.slot as i64),
                &"EPOCH_UPDATE",
            ],
        )
        .await?;

    Ok(())
}

async fn fetch_job_status(
    client: &Client,
    job_id: Uuid,
) -> Result<Option<JobStatus>, Box<dyn std::error::Error + Send + Sync>> {
    let row_opt = client
        .query_opt("SELECT status FROM jobs WHERE job_id = $1", &[&job_id])
        .await?;

    Ok(row_opt.map(|row| row.get("status")))
}

pub async fn get_latest_slot_id_in_progress(
    client: &Client,
) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
    // Query the latest slot with job_status in ('in_progress', 'initialized')
    let row_opt = client
        .query_opt(
            "SELECT slot FROM jobs
             WHERE job_status IN ($1, $2)
             ORDER BY slot DESC
             LIMIT 1",
            &[&"CREATED", &"PIE_GENERATED"],
        )
        .await?;

    // Extract and return the slot ID
    if let Some(row) = row_opt {
        Ok(Some(row.get::<_, i64>("slot")))
    } else {
        Ok(None)
    }
}

pub async fn get_merkle_paths_for_epoch(
    client: &Client,
    epoch_id: i32,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    // Query all merkle paths for the given epoch_id
    let rows = client
        .query(
            "SELECT merkle_path FROM epoch_merkle_paths
             WHERE epoch_id = $1
             ORDER BY path_index ASC",
            &[&epoch_id],
        )
        .await?;

    let paths: Vec<String> = rows
        .iter()
        .map(|row| row.get::<_, String>("merkle_path"))
        .collect();

    Ok(paths)
}

async fn update_job_status(
    client: &Client,
    job_id: Uuid,
    new_status: JobStatus,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "UPDATE jobs SET job_status = $1, updated_at = NOW() WHERE job_uuid = $2",
            &[&new_status.to_string(), &job_id],
        )
        .await?;
    Ok(())
}

async fn set_job_txhash(
    client: &Client,
    job_id: Uuid,
    txhash: Felt,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "UPDATE jobs SET tx_hash = $1, updated_at = NOW() WHERE job_uuid = $2",
            &[&txhash.to_string(), &job_id],
        )
        .await?;
    Ok(())
}

async fn cancell_all_unfinished_jobs(
    client: &Client,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    client
        .execute(
            "UPDATE jobs SET status = $1, updated_at = NOW() WHERE status = 'FETCHING'",
            &[&JobStatus::Cancelled.to_string()],
        )
        .await?;
    Ok(())
}

// async fn fetch_job_by_status(
//     client: &Client,
//     status: JobStatus,
// ) -> Result<Option<Job>, Box<dyn std::error::Error + Send + Sync>> {
//     let tx = client.transaction().await?;

//     let row_opt = tx
//         .query_opt(
//             r#"
//             SELECT job_id, status
//             FROM jobs
//             WHERE status = $1
//             ORDER BY updated_at ASC
//             LIMIT 1
//             FOR UPDATE SKIP LOCKED
//             "#,
//             &[&status],
//         )
//         .await?;

//     let job = if let Some(row) = row_opt {
//         Some(Job {
//             job_id: row.get("job_id"),
//             job_type: row.get("type"),
//             job_status: row.get("status"),
//             slot: row.get("slot"),
//         })
//     } else {
//         None
//     };

//     tx.commit().await?;
//     Ok(job)
// }

// async fn add_verified_epoch(
//     client: Arc<Client>,
//     slot: u64,
// ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//     client
//         .execute(
//             "INSERT INTO verified_epochs (slot, job_status, slot, type) VALUES ($1, $2, $3, $4)",
//             &[&slot, &status.to_string(), &(slot as i64), &"EPOCH_UPDATE"],
//         )
//         .await?;

//     Ok(())
// }

// async fn worker_task(mut rx: Receiver<Uuid>, db_client: Client) -> Result<(), Box<dyn Error>> {
//     while let Some(job_id) = rx.recv().await {
//         println!("Worker received job {job_id}");

//         // 4a) Check current status in DB
//         if let Some(status) = fetch_job_status(&db_client, job_id).await? {
//             match status {
//                 JobStatus::Created => {
//                     println!("Fetching proof for job {job_id}...");
//                     // Then update status
//                     update_job_status(&db_client, job_id, JobStatus::FetchedProof).await?;
//                     println!("Job {job_id} updated to FetchedProof");
//                 }
//                 JobStatus::FetchedProof => {
//                     // Already fetched, maybe do next step...
//                     println!("Job {job_id} is already FetchedProof; ignoring for now.");
//                 }
//                 _ => {
//                     println!("Job {job_id} in status {:?}, no action needed.", status);
//                 }
//             }
//         } else {
//             eprintln!("No job found in DB for ID = {job_id}");
//         }
//     }
//     Ok(())
// }

// mpsc jobs //
async fn process_job(
    job: Job,
    db_client: Arc<Client>,
    bankai: Arc<BankaiClient>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match job.job_type {
        JobType::EpochUpdate => {
            // Epoch job
            info!(
                "[EPOCH JOB] Started processing epoch job: {} for epoch {}",
                job.job_id, job.slot
            );

            //update_job_status(&db_client, job.job_id, JobStatus::Created).await?;

            // 1) Fetch the latest on-chain verified epoch
            // let latest_epoch_slot = bankai
            //     .starknet_client
            //     .get_latest_epoch_slot(&bankai.config)
            //     .await?;

            // info!(
            //     "[EPOCH JOB] Latest onchain verified epoch slot: {}",
            //     latest_epoch_slot
            // );

            //let latest_epoch_slot = ;

            // make sure next_epoch % 32 == 0
            let next_epoch = (u64::try_from(job.slot).unwrap() / constants::SLOTS_PER_EPOCH)
                * constants::SLOTS_PER_EPOCH
                + constants::SLOTS_PER_EPOCH;
            info!(
                "[EPOCH JOB] Fetching Inputs for next Epoch: {}...",
                next_epoch
            );

            // 2) Fetch the proof
            let proof = bankai.get_epoch_proof(next_epoch).await?;
            info!(
                "[EPOCH JOB] Fetched Inputs successfully for Epoch: {}",
                next_epoch
            );

            update_job_status(&db_client, job.job_id, JobStatus::FetchedProof).await?;

            // 3) Generate PIE
            info!(
                "[EPOCH JOB] Starting Cairo execution and PIE generation for Epoch: {}...",
                next_epoch
            );

            CairoRunner::generate_pie(&proof, &bankai.config)?;

            info!(
                "[EPOCH JOB] Pie generated successfully for Epoch: {}...",
                next_epoch
            );

            update_job_status(&db_client, job.job_id, JobStatus::PieGenerated).await?;

            // // 4) Submit offchain proof-generation job to Atlantic
            // info!("[EPOCH JOB] Sending proof generation query to Atlantic...");

            // let batch_id = bankai.atlantic_client.submit_batch(proof).await?;

            // info!(
            //     "[EPOCH JOB] Proof generation batch submitted to Atlantic. QueryID: {}",
            //     batch_id
            // );

            // update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRequested).await?;
            // set_atlantic_job_queryid(
            //     &db_client,
            //     job.job_id,
            //     batch_id.clone(),
            //     AtlanticJobType::ProofGeneration,
            // )
            // .await?;

            // // Pool for Atlantic execution done
            // bankai
            //     .atlantic_client
            //     .poll_batch_status_until_done(&batch_id, Duration::new(10, 0), usize::MAX)
            //     .await?;

            // info!(
            //     "[EPOCH JOB] Proof generation done by Atlantic. QueryID: {}",
            //     batch_id
            // );

            // let proof = bankai
            //     .atlantic_client
            //     .fetch_proof(batch_id.as_str())
            //     .await?;

            // info!(
            //     "[EPOCH JOB] Proof retrieved from Atlantic. QueryID: {}",
            //     batch_id
            // );

            // update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRetrieved).await?;

            // // 5) Submit wrapped proof request
            // info!("[EPOCH JOB] Sending proof wrapping query to Atlantic..");
            // let wrapping_batch_id = bankai.atlantic_client.submit_wrapped_proof(proof).await?;
            // info!(
            //     "[EPOCH JOB] Proof wrapping query submitted to Atlantic. Wrapping QueryID: {}",
            //     wrapping_batch_id
            // );

            // update_job_status(&db_client, job.job_id, JobStatus::WrapProofRequested).await?;
            // set_atlantic_job_queryid(
            //     &db_client,
            //     job.job_id,
            //     wrapping_batch_id.clone(),
            //     AtlanticJobType::ProofWrapping,
            // )
            // .await?;

            // // Pool for Atlantic execution done
            // bankai
            //     .atlantic_client
            //     .poll_batch_status_until_done(&wrapping_batch_id, Duration::new(10, 0), usize::MAX)
            //     .await?;

            // update_job_status(&db_client, job.job_id, JobStatus::WrappedProofDone).await?;

            // info!("[EPOCH JOB] Proof wrapping done by Atlantic. Fact registered on Integrity. Wrapping QueryID: {}", wrapping_batch_id);

            // update_job_status(&db_client, job.job_id, JobStatus::VerifiedFactRegistered).await?;

            // // 6) Submit epoch update onchain
            // info!("[EPOCH JOB] Calling epoch update onchain...");
            // let update = EpochUpdate::from_json::<EpochUpdate>(next_epoch)?;

            // let txhash = bankai
            //     .starknet_client
            //     .submit_update(update.expected_circuit_outputs, &bankai.config)
            //     .await?;

            // set_job_txhash(&db_client, job.job_id, txhash).await?;

            // info!("[EPOCH JOB] Successfully submitted epoch update...");

            // update_job_status(&db_client, job.job_id, JobStatus::ProofDecommitmentCalled).await?;

            // Now we can get proof from contract?
            // bankai.starknet_client.get_epoch_proof(
            //     &self,
            //     slot: u64,
            //     config: &BankaiConfig)

            // Insert data to DB after successful onchain epoch verification
            // insert_verified_epoch(&db_client, job.slot / 0x2000, epoch_proof).await?;
        }
        JobType::SyncComiteeUpdate => {
            // Sync committee job
            info!(
                "[SYNC COMMITTEE JOB] Started processing sync committee job: {} for slot {}",
                job.job_id, job.slot
            );

            let latest_committee_id = bankai
                .starknet_client
                .get_latest_committee_id(&bankai.config)
                .await?;

            info!(
                "[SYNC COMMITTEE JOB] Latest onchain verified sync committee id: {}",
                latest_committee_id
            );

            let latest_epoch = bankai
                .starknet_client
                .get_latest_epoch_slot(&bankai.config)
                .await?;

            let lowest_committee_update_slot = (latest_committee_id) * Felt::from(0x2000);

            if latest_epoch < lowest_committee_update_slot {
                error!("[SYNC COMMITTEE JOB] Sync committee update requires newer epoch verified. The lowest needed slot is {}", lowest_committee_update_slot);
                //return Err(Error::RequiresNewerEpoch(latest_epoch));
            }

            let update = bankai
                .get_sync_committee_update(latest_epoch.try_into().unwrap())
                .await?;

            info!(
                "[SYNC COMMITTEE JOB] Received sync committee update: {:?}",
                update
            );

            info!(
                "[SYNC COMMITTEE JOB] Starting Cairo execution and PIE generation for Sync Committee: {:?}...",
                latest_committee_id
            );

            CairoRunner::generate_pie(&update, &bankai.config)?;

            update_job_status(&db_client, job.job_id, JobStatus::PieGenerated).await?;

            info!(
                "[SYNC COMMITTEE JOB] Pie generated successfully for Sync Committee: {}...",
                latest_committee_id
            );
            info!("[SYNC COMMITTEE JOB] Sending proof generation query to Atlantic...");

            let batch_id = bankai.atlantic_client.submit_batch(update).await?;

            update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRequested).await?;
            set_atlantic_job_queryid(
                &db_client,
                job.job_id,
                batch_id.clone(),
                AtlanticJobType::ProofGeneration,
            )
            .await?;

            info!(
                "[SYNC COMMITTEE JOB] Proof generation batch submitted to atlantic. QueryID: {}",
                batch_id
            );

            // Pool for Atlantic execution done
            bankai
                .atlantic_client
                .poll_batch_status_until_done(&batch_id, Duration::new(10, 0), usize::MAX)
                .await?;

            info!(
                "[SYNC COMMITTEE JOB] Proof generation done by Atlantic. QueryID: {}",
                batch_id
            );

            let proof = bankai
                .atlantic_client
                .fetch_proof(batch_id.as_str())
                .await?;

            info!(
                "[SYNC COMMITTEE JOB] Proof retrieved from Atlantic. QueryID: {}",
                batch_id
            );

            update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRetrieved).await?;

            // 5) Submit wrapped proof request
            info!("[SYNC COMMITTEE JOB] Sending proof wrapping query to Atlantic..");
            let wrapping_batch_id = bankai.atlantic_client.submit_wrapped_proof(proof).await?;
            info!(
                "[SYNC COMMITTEE JOB] Proof wrapping query submitted to Atlantic. Wrapping QueryID: {}",
                wrapping_batch_id
            );

            update_job_status(&db_client, job.job_id, JobStatus::WrapProofRequested).await?;
            set_atlantic_job_queryid(
                &db_client,
                job.job_id,
                wrapping_batch_id.clone(),
                AtlanticJobType::ProofWrapping,
            )
            .await?;

            // Pool for Atlantic execution done
            bankai
                .atlantic_client
                .poll_batch_status_until_done(&wrapping_batch_id, Duration::new(10, 0), usize::MAX)
                .await?;

            update_job_status(&db_client, job.job_id, JobStatus::WrappedProofDone).await?;

            info!("[SYNC COMMITTEE JOB] Proof wrapping done by Atlantic. Fact registered on Integrity. Wrapping QueryID: {}", wrapping_batch_id);

            update_job_status(&db_client, job.job_id, JobStatus::VerifiedFactRegistered).await?;

            let update = SyncCommitteeUpdate::from_json::<SyncCommitteeUpdate>(job.slot)?;

            info!("[SYNC COMMITTEE JOB] Calling sync committee update onchain...");

            let txhash = bankai
                .starknet_client
                .submit_update(update.expected_circuit_outputs, &bankai.config)
                .await?;

            set_job_txhash(&db_client, job.job_id, txhash).await?;

            // Insert data to DB after successful onchain sync committee verification
            //insert_verified_sync_committee(&db_client, job.slot, sync_committee_hash).await?;
        }
        JobType::EpochBatchUpdate => {
            let proof = EpochUpdateBatch::new_by_slot(&bankai, &db_client, job.slot).await?;

            CairoRunner::generate_pie(&proof, &bankai.config)?;
            let batch_id = bankai.atlantic_client.submit_batch(proof).await?;

            info!(
                "[EPOCH JOB] Proof generation batch submitted to Atlantic. QueryID: {}",
                batch_id
            );

            update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRequested).await?;
            set_atlantic_job_queryid(
                &db_client,
                job.job_id,
                batch_id.clone(),
                AtlanticJobType::ProofGeneration,
            )
            .await?;

            // Pool for Atlantic execution done
            bankai
                .atlantic_client
                .poll_batch_status_until_done(&batch_id, Duration::new(10, 0), usize::MAX)
                .await?;

            info!(
                "[EPOCH JOB] Proof generation done by Atlantic. QueryID: {}",
                batch_id
            );

            let proof = bankai
                .atlantic_client
                .fetch_proof(batch_id.as_str())
                .await?;

            info!(
                "[EPOCH JOB] Proof retrieved from Atlantic. QueryID: {}",
                batch_id
            );

            update_job_status(&db_client, job.job_id, JobStatus::OffchainProofRetrieved).await?;

            // 5) Submit wrapped proof request
            info!("[EPOCH JOB] Sending proof wrapping query to Atlantic..");
            let wrapping_batch_id = bankai.atlantic_client.submit_wrapped_proof(proof).await?;
            info!(
                "[EPOCH JOB] Proof wrapping query submitted to Atlantic. Wrapping QueryID: {}",
                wrapping_batch_id
            );

            update_job_status(&db_client, job.job_id, JobStatus::WrapProofRequested).await?;
            set_atlantic_job_queryid(
                &db_client,
                job.job_id,
                wrapping_batch_id.clone(),
                AtlanticJobType::ProofWrapping,
            )
            .await?;

            // Pool for Atlantic execution done
            bankai
                .atlantic_client
                .poll_batch_status_until_done(&wrapping_batch_id, Duration::new(10, 0), usize::MAX)
                .await?;

            update_job_status(&db_client, job.job_id, JobStatus::WrappedProofDone).await?;

            info!("[EPOCH JOB] Proof wrapping done by Atlantic. Fact registered on Integrity. Wrapping QueryID: {}", wrapping_batch_id);

            update_job_status(&db_client, job.job_id, JobStatus::VerifiedFactRegistered).await?;

            // 6) Submit epoch update onchain
            info!("[EPOCH JOB] Calling epoch update onchain...");
            //let update = EpochUpdate::from_json::<EpochUpdate>(next_epoch)?;

            // let txhash = bankai
            //     .starknet_client
            //     .submit_update(update.expected_circuit_outputs, &bankai.config)
            //     .await?;

            // set_job_txhash(&db_client, job.job_id, txhash).await?;

            // info!("[EPOCH JOB] Successfully submitted epoch update...");

            // update_job_status(&db_client, job.job_id, JobStatus::ProofDecommitmentCalled).await?;

            // bankai.starknet_client.get_epoch_proof(
            //     &self,
            //     slot: u64,
            //     config: &BankaiConfig)

            //Insert data to DB after successful onchain epoch verification
            // insert_verified_epochs_batch(&db_client, job.slot / 0x2000, epoch_proof).await?;
        }
    }

    Ok(())
}

//  RPC requests handling functions //

async fn handle_get_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(json!({ "success": true }))
}

async fn handle_get_epoch_update(
    Path(slot): Path<u64>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state.bankai.get_epoch_proof(slot).await {
        Ok(epoch_update) => {
            // Convert `EpochUpdate` to `serde_json::Value`
            let value = serde_json::to_value(epoch_update).unwrap_or_else(|err| {
                eprintln!("Failed to serialize EpochUpdate: {:?}", err);
                json!({ "error": "Internal server error" })
            });
            Json(value)
        }
        Err(err) => {
            eprintln!("Failed to fetch proof: {:?}", err);
            Json(json!({ "error": "Failed to fetch proof" }))
        }
    }
}

// async fn handle_get_epoch_proof(
//     Path(slot): Path<u64>,
//     State(state): State<AppState>,
// ) -> impl IntoResponse {
//     match state.bankai.starknet_client.get_epoch_proof(slot).await {
//         Ok(epoch_update) => {
//             // Convert `EpochUpdate` to `serde_json::Value`
//             let value = serde_json::to_value(epoch_update).unwrap_or_else(|err| {
//                 eprintln!("Failed to serialize EpochUpdate: {:?}", err);
//                 json!({ "error": "Internal server error" })
//             });
//             Json(value)
//         }
//         Err(err) => {
//             eprintln!("Failed to fetch proof: {:?}", err);
//             Json(json!({ "error": "Failed to fetch proof" }))
//         }
//     }
// }

// async fn handle_get_committee_hash(
//     Path(committee_id): Path<u64>,
//     State(state): State<AppState>,
// ) -> impl IntoResponse {
//     match state.bankai.starknet_client.get_committee_hash(committee_id).await {
//         Ok(committee_hash) => {
//             // Convert `EpochUpdate` to `serde_json::Value`
//             let value = serde_json::to_value(committee_hash).unwrap_or_else(|err| {
//                 eprintln!("Failed to serialize EpochUpdate: {:?}", err);
//                 json!({ "error": "Internal server error" })
//             });
//             Json(value)
//         }
//         Err(err) => {
//             eprintln!("Failed to fetch proof: {:?}", err);
//             Json(json!({ "error": "Failed to fetch proof" }))
//         }
//     }
// }

async fn handle_get_latest_verified_slot(State(state): State<AppState>) -> impl IntoResponse {
    match state
        .bankai
        .starknet_client
        .get_latest_epoch_slot(&state.bankai.config)
        .await
    {
        Ok(latest_epoch) => {
            // Convert `Felt` to a string and parse it as a hexadecimal number
            let hex_string = latest_epoch.to_string(); // Ensure this converts to a "0x..." string
            match u64::from_str_radix(hex_string.trim_start_matches("0x"), 16) {
                Ok(decimal_epoch) => Json(json!({ "latest_verified_slot": decimal_epoch })),
                Err(err) => {
                    eprintln!("Failed to parse latest_epoch as decimal: {:?}", err);
                    Json(json!({ "error": "Invalid epoch format" }))
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to fetch latest epoch: {:?}", err);
            Json(json!({ "error": "Failed to fetch latest epoch" }))
        }
    }
}

// async fn handle_get_job_status(
//     Path(job_id): Path<u64>,
//     State(state): State<AppState>,
// ) -> impl IntoResponse {
//     match fetch_job_status(&state.db_client, job_id).await {
//         Ok(job_status) => Json(job_status),
//         Err(err) => {
//             eprintln!("Failed to fetch job status: {:?}", err);
//             Json(json!({ "error": "Failed to fetch job status" }))
//         }
//     }
// }

async fn handle_get_merkle_paths_for_epoch(
    Path(epoch_id): Path<i32>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match get_merkle_paths_for_epoch(&state.db_client, epoch_id).await {
        Ok(merkle_paths) => {
            if merkle_paths.len() > 0 {
                Json(json!({ "epoch_id": epoch_id, "merkle_paths": merkle_paths }))
            } else {
                Json(json!({ "error": "Epoch not available now" }))
            }
        }
        Err(err) => {
            error!("Failed to fetch merkle paths epoch: {:?}", err);
            Json(json!({ "error": "Failed to fetch latest epoch" }))
        }
    }
}
