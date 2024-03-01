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

    #[arg(long, default_value_t = 507904)]
    pub block_size: usize,

    #[arg(long, default_value_t = 507904)]
    pub chunk_size: usize,

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
        default_value_t = 32,
        help = "target chunk number"
    )]
    target_chunk_num: u8,
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
            chunk_size: 512 * 31 / 32,
            rps: 12,
            max_out_standing: 6,
            target_chunk_num: 32,
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
    //       chunks
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
            target_chunk_num: self.config.target_chunk_num as u32,
        }
    }

    // disperse_chunk
    //
    //       Disperses a single chunk of data to data availability provider
    //
    // params:
    // chunk_id : logical sequence of the chunk within a blob
    // data     : data for the chunk
    //
    // returns:
    //
    // request_id: The request ID can be used for getting the next
    // state of the dispersed chunk
    async fn disperse_chunk(&self, chunk_id: usize, data: &[u8]) -> Result<Vec<u8>> {
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
                    println!("Err: disperse_chunk {chunk_id:?} {:?}", resp.message());
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

    // wait_for_chunk_confirmation
    //
    //       Waits for a chunk to be confirmed. The wait is achieved
    //       using a poll loop. The loop involves a sleep which is
    //       "fine" as confirmation is not in the hot throughput path.
    //
    // params:
    // request_id : The chunk-id received from the disperser
    // data       : data for the chunk
    //
    // returns:
    // BlobStatusReply to be used in retrieval
    //
    // TODO:      : Handling poll errors outside on un-confirmed blocks
    async fn wait_for_chunk_confirmation(
        &self,
        chunk_id: usize,
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
            println!("{chunk_id} Response {r:?}");
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

    // retrieve_chunk
    //
    //       Retrieves a single chunk of data from the data availability provider
    //
    // params:
    // batch_header_hash : The message that the operators will sign their signatures
    // on.
    // blob_index: index of blob in the batch
    async fn retrieve_chunk(&self, batch_header_hash: Vec<u8>, blob_index: u32) -> Result<Vec<u8>> {
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
            .chunks(self.config.chunk_size)
            .into_iter()
            .enumerate()
            .map(|(chunk_id, data)| self.disperse_chunk(chunk_id, data))
            .collect::<Vec<_>>();
        let ids = join_all(v)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    async fn store_blob(&self, data: &[u8]) -> Result<Vec<BlobStatusReply>> {
        let ids = self.disperse_blob(data).await?;
        // confirm chunks
        let confirmations = ids
            .into_iter()
            .enumerate()
            .map(|(chunk_id, request_id)| self.wait_for_chunk_confirmation(chunk_id, request_id))
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

        // Collect and reconstruct all chunks
        let retrievals = v
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(blob_index, batch_header_hash)| {
                self.retrieve_chunk(batch_header_hash, blob_index)
            })
            .collect::<Vec<_>>();
        let res = join_all(retrievals)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(res.into_iter().flatten().collect())
    }
}

#[cfg(test)]
mod test {
    const KB: usize = 1024;
    const BLOB_SIZE: usize = 512;
    const CHUNK_SIZE: usize = 128;
    use super::{ZGDA, ZGDAConfig};
    use crate::da::DAClient;
    use crate::rollup::test::MockNordRollup;
    use crate::rollup::RollupClient;
    use rand::{Rng, SeedableRng};
    use rand_pcg::Pcg64;
    #[tokio::test]
    async fn da_round_trip() {
        let da = ZGDA::new(ZGDAConfig::default(), prometheus::default_registry());
        let mut data = Vec::<u8>::with_capacity(BLOB_SIZE);
        for i in 0..BLOB_SIZE {
            data.push(i as u8)
        }
        let responses = da
            .store_blob(&data)
            .await
            .expect("availability proofs");
        let data = da.retrieve_blob(responses).await.expect("retrieved data");
        for i in 0..BLOB_SIZE {
            assert_eq!(data[i], i as u8);
        }
    }

    #[tokio::test]
    async fn da_zg_stress() {
        let seed = Pcg64::from_entropy().gen();
        let rollup = MockNordRollup::new(seed, 1000.);
        let action_list_resp = rollup
            .fetch_transactions(0, 100_000)
            .await
            .expect("Mock txn expected");
        println!("action_list_resp size: {}", action_list_resp.len());
    }
}
