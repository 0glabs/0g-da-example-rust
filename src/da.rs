pub mod disperser {
    tonic::include_proto!("disperser");
}

use crate::metrics::DAMetrics;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use disperser::disperser_client::DisperserClient;
use disperser::{
    BlobStatus, BlobStatusReply, BlobStatusRequest, DisperseBlobRequest, RetrieveBlobRequest,
    SecurityParams,
};
//Dispersing Borsh serialized binary data.
//use engine::ActionId;
use futures::future::join_all;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use prometheus::Registry;
use std::vec::Vec;
use tokio::sync::Semaphore;

#[async_trait]
pub trait DAClient {
    // ping
    //
    //       Ping the data availability service
    async fn ping() -> Result<()>;

    async fn disperse_blob(&self, data: &[u8]) -> Result<Vec<Vec<u8>>>;

    // store_blob
    //
    //       High level function that disperses and confirm a large blob
    //
    // params:
    //
    //       data: single unit of data to be dispersed on the data availability
    //       layer.
    async fn store_blob(&self, data: &[u8]) -> Result<Vec<BlobStatusReply>>;

    // retrieve_blob
    //
    //       Retrieves a blob, stored using the store blob function
    //
    // params:
    //
    // Vec<BlobStatusReply> : Return value of store blob function
    async fn retrieve_blob(&self, blob_status: Vec<BlobStatusReply>) -> Result<Vec<u8>>;
}

// ZGDA implementation for a DA Client
pub struct ZGDA {
    config: ZGDAConfig,
    metrics: DAMetrics,
    disperser_rate_limiter: DefaultDirectRateLimiter,
    disperser_permits: Semaphore,
}

#[derive(clap::Parser, Debug, Clone)]
pub struct ZGDAConfig {
    #[arg(
        long,
        env = "DA_URL",
        default_value_t = String::from("http://0.0.0.0:51001")
    )]
    url: String,

    #[arg(long, default_value_t = 1000)]
    status_retry_delay_ms: u32,
    #[arg(long, default_value_t = 2000)]
    disperser_retry_delay_ms: u32,

    #[arg(long, default_value_t = 25)]
    adversary_threshold: u32,

    #[arg(long, default_value_t = 50)]
    quorum_threshold: u32,

    #[arg(long, default_value_t = 3_145_728)]
    pub block_size: usize,

    #[arg(long, default_value_t = 524288)]
    pub blob_size: usize,

    #[arg(
        long,
        global = true,
        default_value_t = 6,
        help = "request per second issued to ZGDA"
    )]
    rps: u8,

    #[arg(
        long,
        global = true,
        default_value_t = 6,
        help = "max outstanding requests to ZGDA"
    )]
    max_out_standing: u8,

    #[arg(
        long,
        global = true,
        default_value_t = 1024,
        help = "target row number to divide the blob into within ZGDA"
    )]
    target_row_num: u32,
}

impl Default for ZGDAConfig {
    // TODO: replace with our own url
    fn default() -> Self {
        Self {
            url: "http://0.0.0.0:51001".to_string(),
            disperser_retry_delay_ms: 1000,
            status_retry_delay_ms: 2000,
            adversary_threshold: 25,
            quorum_threshold: 50,
            block_size: 12_582_912,
            blob_size: 256,
            rps: 6,
            max_out_standing: 6,
            target_row_num: 32,
        }
    }
}

impl ZGDA {
    #[allow(dead_code)]
    pub fn new(config: ZGDAConfig, metrics_registry: &Registry) -> Self {
        let clock = governor::clock::DefaultClock::default();
        let rps = std::num::NonZeroU32::new(config.rps as u32).expect("rps must be non-zero");
        let drl: DefaultDirectRateLimiter =
            RateLimiter::direct_with_clock(Quota::per_second(rps), &clock);
        let max_out_standing = config.max_out_standing;
        Self {
            config,
            disperser_permits: Semaphore::new(max_out_standing as usize),
            disperser_rate_limiter: drl,
            metrics: DAMetrics::new(metrics_registry),
        }
    }

    // disperse_blob_request
    //
    //       Helper function to generate default security parameters for dispersed
    //       blobs
    //
    // params:
    //
    //       data: single unit of data to be dispersed on the data availability
    //       layer.
    //       adversary_threshold: number of malicious nodes tolerated
    //       quorum_threshold: T of N quorum
    fn disperse_blob_request(&self, data: &[u8]) -> DisperseBlobRequest {
        disperser::DisperseBlobRequest {
            data: data.to_vec(),
            security_params: vec![SecurityParams {
                quorum_id: 0,
                adversary_threshold: self.config.adversary_threshold,
                quorum_threshold: self.config.quorum_threshold,
            }],
            target_row_num: self.config.target_row_num as u32,
        }
    }

    // disperse_blob
    //
    //       Disperses a single blob data to data availability provider
    //
    // params:
    // blob_id : logical sequence of the blob in a block
    // data     : blob data
    //
    // returns:
    //
    // request_id: The request ID can be used for getting the next
    // state of the dispersed blob
    async fn disperse_blob_inner(&self, blob_id: usize, data: &[u8]) -> Result<Vec<u8>> {
        let _permit = self
            .disperser_permits
            .acquire()
            .await
            .expect("request permit");
        self.disperser_rate_limiter.until_ready().await;

        let mut client = DisperserClient::connect(self.config.url.clone()).await?;
        let response = loop {
            let request = tonic::Request::new(self.disperse_blob_request(&data));
            match client.disperse_blob(request).await {
                Ok(resp) => {
                    break resp;
                }
                Err(resp) => {
                    self.metrics.dispersal_rate_limited.inc();
                    println!("Err: disperse_blob {blob_id:?} {:?}", resp.message());
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        self.config.disperser_retry_delay_ms.into(),
                    ))
                    .await;
                }
            }
        };
        self.metrics.dispersed_bytes.inc_by(data.len() as u64);
        Ok(response.into_inner().request_id.clone())
    }

    // wait_for_blob_confirmation
    //
    //       Waits for a blob to be confirmed. The wait is achieved
    //       using a poll loop. The loop involves a sleep which is
    //       "fine" as confirmation is not in the hot throughput path.
    //
    // params:
    // request_id : The request-id received from the disperser
    // data       : blob data
    //
    // returns:
    // BlobStatusReply to be used in retrieval
    //
    // TODO:      : Handling poll errors outside on un-confirmed blocks
    async fn wait_for_blob_confirmation(
        &self,
        blob_id: usize,
        request_id: Vec<u8>,
    ) -> Result<BlobStatusReply> {
        let mut client = DisperserClient::connect(self.config.url.clone()).await?;
        let response = loop {
            let response = client
                .get_blob_status(BlobStatusRequest {
                    request_id: request_id.clone(),
                })
                .await;
            let r = response.unwrap().into_inner();
            println!("{blob_id} Response {r:?}");
            self.metrics.poll_confirmation_count.inc();
            let blob_status = BlobStatus::try_from(r.status).ok();
            if let Some(BlobStatus::Confirmed) = blob_status {
                break r;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                self.config.status_retry_delay_ms.into(),
            ))
            .await
        };

        let data_len = response.info.as_ref().map_or_else(
            || 0,
            |info| {
                info.blob_header
                    .as_ref()
                    .map_or_else(|| 0, |header| header.data_length)
            },
        );
        self.metrics.confirmed_bytes.inc_by(data_len as u64);
        Ok(response)
    }

    // retrieve_blob
    //
    //       Retrieves a single blob of data from the data availability provider
    //
    // params:
    // batch_header_hash : The message that the operators will sign their signatures
    // on.
    // blob_index: index of blob in the batch
    async fn retrieve_blob_inner(
        &self,
        batch_header_hash: Vec<u8>,
        blob_index: u32,
    ) -> Result<Vec<u8>> {
        let mut client = DisperserClient::connect(self.config.url.clone()).await?;
        let request = tonic::Request::new(RetrieveBlobRequest {
            blob_index,
            batch_header_hash,
        });

        let resp = client.retrieve_blob(request).await?;
        Ok(resp.into_inner().data)
    }
}

#[async_trait]
impl DAClient for ZGDA {
    async fn ping() -> Result<()> {
        todo!();
    }

    async fn disperse_blob(&self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        // disperse
        let v = data
            .chunks(self.config.blob_size)
            .into_iter()
            .enumerate()
            .map(|(blob_id, data)| self.disperse_blob_inner(blob_id, data))
            .collect::<Vec<_>>();
        let ids = join_all(v)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    async fn store_blob(&self, data: &[u8]) -> Result<Vec<BlobStatusReply>> {
        let ids = self.disperse_blob(data).await?;
        // confirm blobs
        let confirmations = ids
            .into_iter()
            .enumerate()
            .map(|(blob_id, request_id)| self.wait_for_blob_confirmation(blob_id, request_id))
            .collect::<Vec<_>>();
        join_all(confirmations)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
    }

    async fn retrieve_blob(&self, blob_status: Vec<BlobStatusReply>) -> Result<Vec<u8>> {
        // Following code block simply extracts (blob_index, batch_header_hash).
        // The code complexity is due to most of Prost generated types of ZGDA
        // are unnecessarily wrapped as options types.
        let v = blob_status
            .into_iter()
            .map(|reply| {
                let proof = reply
                    .info
                    .ok_or(anyhow!("None() for BlobInfo"))?
                    .blob_verification_proof
                    .ok_or(anyhow!("None() for Verification Proof"))?;
                let blob_index = proof.blob_index;
                let batch_header_hash = proof
                    .batch_metadata
                    .ok_or(anyhow!("None() for BatchMetadata"))?
                    .batch_header_hash;
                Ok::<_, anyhow::Error>((blob_index, batch_header_hash))
            })
            .collect::<Vec<_>>();

        // Collect and reconstruct all blobs
        let retrievals = v
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(blob_index, batch_header_hash)| {
                self.retrieve_blob_inner(batch_header_hash, blob_index)
            })
            .collect::<Vec<_>>();
        let res = join_all(retrievals)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(res.into_iter().flatten().collect())
    }
}
