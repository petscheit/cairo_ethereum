use crate::helpers;
use crate::state::{AtlanticJobType, Error, Job, JobStatus, JobType};
use crate::utils::starknet_client::EpochProof;
use alloy_primitives::FixedBytes;
use starknet::core::types::Felt;
use std::str::FromStr;
//use std::error::Error;
use chrono::NaiveDateTime;
use num_traits::ToPrimitive;
use tokio_postgres::{Client, Row};
use tracing::{error, info};
use uuid::Uuid;

#[derive(Debug)]
pub struct JobSchema {
    pub job_uuid: uuid::Uuid,
    pub job_status: JobStatus,
    pub slot: i64,
    pub batch_range_begin_epoch: i64,
    pub batch_range_end_epoch: i64,
    pub job_type: JobType,
    //pub updated_at: i64,
}

#[derive(Debug)]
pub struct DatabaseManager {
    client: Client,
}

impl DatabaseManager {
    pub async fn new(db_url: &str) -> Self {
        let client = match tokio_postgres::connect(db_url, tokio_postgres::NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        eprintln!("Connection error: {}", e);
                    }
                });

                info!("Connected to the database successfully!");
                client
            }
            Err(err) => {
                error!("Failed to connect to the database: {}", err);
                std::process::exit(1); // Exit with non-zero status code
            }
        };

        Self { client }
    }

    pub async fn insert_verified_epoch(
        &self,
        epoch_id: u64,
        epoch_proof: EpochProof,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "INSERT INTO verified_epoch (epoch_id, header_root, state_root, n_signers)
             VALUES ($1, $2, $3, $4, $4, $6)",
                &[
                    &epoch_id.to_string(),
                    &epoch_proof.header_root.to_string(),
                    &epoch_proof.state_root.to_string(),
                    &epoch_proof.n_signers.to_string(),
                    &epoch_proof.execution_hash.to_string(),
                    &epoch_proof.execution_height.to_string(),
                ],
            )
            .await?;

        Ok(())
    }

    pub async fn insert_verified_sync_committee(
        &self,
        sync_committee_id: u64,
        sync_committee_hash: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "INSERT INTO verified_sync_committee (sync_committee_id, sync_committee_hash)
             VALUES ($1, $2)",
                &[&sync_committee_id.to_string(), &sync_committee_hash],
            )
            .await?;

        Ok(())
    }

    pub async fn set_atlantic_job_queryid(
        &self,
        job_id: Uuid,
        batch_id: String,
        atlantic_job_type: AtlanticJobType,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match atlantic_job_type {
            AtlanticJobType::ProofGeneration => {
                self.client
                .execute(
                    "UPDATE jobs SET atlantic_proof_generate_batch_id = $1, updated_at = NOW() WHERE job_uuid = $2",
                    &[&batch_id.to_string(), &job_id],
                )
                .await?;
            }
            AtlanticJobType::ProofWrapping => {
                self.client
                .execute(
                    "UPDATE jobs SET atlantic_proof_wrapper_batch_id = $1, updated_at = NOW() WHERE job_uuid = $2",
                    &[&batch_id.to_string(), &job_id],
                )
                .await?;
            } // _ => {
              //     println!("Unk", status);
              // }
        }

        Ok(())
    }

    pub async fn create_job(
        &self,
        job: Job,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match job.job_type {
            JobType::EpochBatchUpdate => {
                self.client
                    .execute(
                        "INSERT INTO jobs (job_uuid, job_status, slot, type, batch_range_begin_epoch, batch_range_end_epoch) VALUES ($1, $2, $3, $4, $5, $6)",
                        &[
                            &job.job_id,
                            &job.job_status.to_string(),
                            &(job.slot as i64),
                            &"EPOCH_BATCH_UPDATE",
                            &(job.batch_range_begin_epoch.unwrap() as i64),
                            &(job.batch_range_end_epoch.unwrap() as i64),
                        ],
                    )
                    .await
                    .map_err(|e| Error::DatabaseError(e.to_string()))?;
            }
            JobType::EpochUpdate => {}
            JobType::SyncCommitteeUpdate => {
                self.client
                    .execute(
                        "INSERT INTO jobs (job_uuid, job_status, slot, type) VALUES ($1, $2, $3, $4)",
                        &[
                            &job.job_id,
                            &job.job_status.to_string(),
                            &(job.slot as i64),
                            &"SYNC_COMMITTEE_UPDATE",
                        ],
                    )
                    .await
                    .map_err(|e| Error::DatabaseError(e.to_string()))?;
            }
        }

        Ok(())
    }

    pub async fn fetch_job_status(
        &self,
        job_id: Uuid,
    ) -> Result<Option<JobStatus>, Box<dyn std::error::Error + Send + Sync>> {
        let row_opt = self
            .client
            .query_opt("SELECT status FROM jobs WHERE job_id = $1", &[&job_id])
            .await?;

        Ok(row_opt.map(|row| row.get("status")))
    }

    // pub async fn get_latest_slot_id_in_progress(
    //     &self,
    // ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
    //     // Query the latest slot with job_status in ('in_progress', 'initialized')
    //     let row_opt = self
    //         .client
    //         .query_opt(
    //             "SELECT slot FROM jobs
    //              WHERE job_status NOT IN ('DONE', 'CANCELLED', 'ERROR')
    //              ORDER BY slot DESC
    //              LIMIT 1",
    //             &[],
    //         )
    //         .await?;

    //     // Extract and return the slot ID
    //     if let Some(row) = row_opt {
    //         Ok(Some(row.get::<_, i64>("slot").to_u64().unwrap()))
    //     } else {
    //         Ok(Some(0))
    //     }
    // }

    pub async fn get_latest_epoch_in_progress(
        &self,
    ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        // Query the latest slot with job_status in ('in_progress', 'initialized')
        let row_opt = self
            .client
            .query_opt(
                "SELECT batch_range_end_epoch FROM jobs
                 WHERE job_status NOT IN ('DONE', 'CANCELLED', 'ERROR')
                        AND batch_range_end_epoch != 0
                        AND type = 'EPOCH_BATCH_UPDATE'
                 ORDER BY batch_range_end_epoch DESC
                 LIMIT 1",
                &[],
            )
            .await?;

        // Extract and return the slot ID
        if let Some(row) = row_opt {
            Ok(Some(
                row.get::<_, i64>("batch_range_end_epoch").to_u64().unwrap(),
            ))
        } else {
            Ok(Some(0))
        }
    }

    pub async fn get_latest_sync_committee_in_progress(
        &self,
    ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        // Query the latest slot with job_status in ('in_progress', 'initialized')
        let row_opt = self
            .client
            .query_opt(
                "SELECT slot FROM jobs
                 WHERE job_status NOT IN ('DONE', 'CANCELLED', 'ERROR')
                        AND type = 'SYNC_COMMITTEE_UPDATE'
                 ORDER BY slot DESC
                 LIMIT 1",
                &[],
            )
            .await?;

        // Extract and return the slot ID
        if let Some(row) = row_opt {
            Ok(Some(helpers::slot_to_sync_committee_id(
                row.get::<_, i64>("batch_range_end_epoch").to_u64().unwrap(),
            )))
        } else {
            Ok(Some(0))
        }
    }

    pub async fn count_jobs_in_progress(
        &self,
    ) -> Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> {
        // Query the latest slot with job_status in ('in_progress', 'initialized')
        let row_opt = self
            .client
            .query_opt(
                "SELECT COUNT(job_uuid) as count FROM jobs
                 WHERE job_status NOT IN ('DONE', 'CANCELLED', 'ERROR')
                        AND type = 'EPOCH_BATCH_UPDATE'
                ",
                &[],
            )
            .await?;

        // Extract and return the slot ID
        if let Some(row) = row_opt {
            Ok(Some(row.get::<_, i64>("count").to_u64().unwrap()))
        } else {
            Ok(Some(0))
        }
    }

    pub async fn get_merkle_paths_for_epoch(
        &self,
        epoch_id: i32,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        // Query all merkle paths for the given epoch_id
        let rows = self
            .client
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

    // pub async fn get_compute_finsihed_jobs_to_proccess_onchain_call(
    //     &self,
    //     last_epoch: JobStatus,
    // ) -> Result<Vec<JobSchema>, Box<dyn std::error::Error + Send + Sync>> {
    //     let rows = self
    //         .client
    //         .query(
    //             "SELECT * FROM jobs
    //              WHERE job_status = 'OFFCHAIN_COMPUTATION_FINISHED' AND job_type = 'EPOCH_BATCH_UPDATE'  AND batch_range_end_epoch <= $1",
    //             &[&last_epoch],
    //         )
    //         .await?;

    //     // Map rows into Job structs
    //     let jobs: Vec<JobSchema> = rows
    //         .into_iter()
    //         .map(|row: Row| JobSchema {
    //             job_uuid: row.get("job_uuid"),
    //             job_status: row.get("job_status"),
    //             slot: row.get("slot"),
    //             batch_range_begin_epoch: row.get("batch_range_begin_epoch"),
    //             batch_range_end_epoch: row.get("batch_range_end_epoch"),
    //             job_type: row.get("type"),
    //             updated_at: row.get("updated_at"),
    //         })
    //         .collect();

    //     Ok(jobs)
    // }

    pub async fn get_jobs_with_status(
        &self,
        desired_status: JobStatus,
    ) -> Result<Vec<JobSchema>, Box<dyn std::error::Error + Send + Sync>> {
        // Query all jobs with the given job_status
        let rows = self
            .client
            .query(
                "SELECT * FROM jobs
                 WHERE job_status = $1",
                &[&desired_status.to_string()],
            )
            .await?;

        // Map rows into JobSchema structs
        let jobs: Vec<JobSchema> = rows
            .into_iter()
            .map(
                |row: Row| -> Result<JobSchema, Box<dyn std::error::Error + Send + Sync>> {
                    let job_type_str: String = row.get("type");
                    let job_status_str: String = row.get("job_status");

                    let job_type = JobType::from_str(&job_type_str)
                        .map_err(|err| format!("Failed to parse job type: {}", err))?;
                    let job_status = JobStatus::from_str(&job_status_str)
                        .map_err(|err| format!("Failed to parse job status: {}", err))?;

                    Ok(JobSchema {
                        job_uuid: row.get("job_uuid"),
                        job_status,
                        slot: row.get("slot"),
                        batch_range_begin_epoch: row.get("batch_range_begin_epoch"),
                        batch_range_end_epoch: row.get("batch_range_end_epoch"),
                        job_type,
                        //updated_at: row.get("updated_at"),
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;

        Ok(jobs)
    }

    pub async fn update_job_status(
        &self,
        job_id: Uuid,
        new_status: JobStatus,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "UPDATE jobs SET job_status = $1, updated_at = NOW() WHERE job_uuid = $2",
                &[&new_status.to_string(), &job_id],
            )
            .await?;
        Ok(())
    }

    pub async fn set_ready_to_broadcast_for_batch_epochs(
        &self,
        first_epoch: u64,
        last_epoch: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "UPDATE jobs
                SET job_status = 'READY_TO_BROADCAST_ONCHAIN', updated_at = NOW()
                WHERE batch_range_begin_epoch >= $1 AND batch_range_end_epoch <= $2 AND type = 'EPOCH_BATCH_UPDATE'
                      AND job_status = 'OFFCHAIN_COMPUTATION_FINISHED'",
                &[&first_epoch.to_i64(), &last_epoch.to_i64()],
            )
            .await?;
        Ok(())
    }

    pub async fn set_job_txhash(
        &self,
        job_id: Uuid,
        txhash: Felt,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "UPDATE jobs SET tx_hash = $1, updated_at = NOW() WHERE job_uuid = $2",
                &[&txhash.to_string(), &job_id],
            )
            .await?;
        Ok(())
    }

    // pub async fn cancell_all_unfinished_jobs(
    //     &self,
    // ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    //     self.client
    //         .execute(
    //             "UPDATE jobs SET status = $1, updated_at = NOW() WHERE status = 'FETCHING'",
    //             &[&JobStatus::Cancelled.to_string()],
    //         )
    //         .await?;
    //     Ok(())
    // }

    pub async fn insert_merkle_path_for_epoch(
        &self,
        epoch: u64,
        path_index: u64,
        path: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.client
            .execute(
                "INSERT INTO epoch_merkle_paths (epoch_id, path_index, merkle_path) VALUES ($1, $2, $3)",
                &[&epoch.to_i64(), &path_index.to_i64(), &path],
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
}