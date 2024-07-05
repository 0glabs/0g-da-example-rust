use anyhow::Result;
use rand::Rng;
use zgdatestharness::{DAClient, ZGDAConfig, ZGDA};

#[derive(clap::Parser)]
struct Cli {
    #[arg(
        long,
        global = true,
        default_value_t = 9148,
        help = "port for prometheus metrics"
    )]
    metrics_port: u16,

    #[arg(
        long,
        global = true,
        help = "Stop after doing a single ZGDA store/dipersal"
    )]
    stop: bool,

    #[arg(long, global = true, default_value_t = std::u32::MAX, help = "Keep running for a fixed amount of seconds")]
    run_for_secs: u32,

    #[arg(
        long,
        global = true,
        default_value_t = 0,
        help = "Sleep after every data dispersal/store call"
    )]
    sleep_for_secs: u32,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    ZGDADisperse(ZGDAConfig),
    ZGDAStore(ZGDAConfig),
}

async fn zgdadisperse(da: &ZGDA, data: &[u8]) {
    println!("dispersing blob {} bytes", data.len());
    let _id = da.disperse_blob(&data).await.expect("request ids");
}

async fn zgdastore(da: &ZGDA, data: &[u8]) {
    println!("storing blob");
    let responses = da.store_blob(&data).await.expect("availability proofs");
    let data_retrieved = da.retrieve_blob(responses).await.expect("retrieved data");
    for i in 0..data.len() {
        assert_eq!(data[i], data_retrieved[i]);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("ZGDA rust client");
    let Cli {
        metrics_port,
        stop,
        cmd,
        run_for_secs,
        sleep_for_secs,
    } = <Cli as clap::Parser>::parse();
    println!("{cmd:?}");

    let da_config = match &cmd {
        Command::ZGDAStore(cfg) => cfg.clone(),
        Command::ZGDADisperse(cfg) => cfg.clone(),
    };

    let addr = format!("0.0.0.0:{}", metrics_port);
    let metrics_server = tokio::task::spawn_blocking(move || {
        prometheus_exporter::start(addr.parse().expect("failed to parse binding"))
            .expect("failed to start prometheus exporter");
    });

    let blob_size = da_config.block_size;
    let mut data = vec![0; blob_size];
    let mut rng = rand::thread_rng();

    let da = ZGDA::new(da_config, prometheus::default_registry());

    let prog_start = std::time::Instant::now();
    let sleep_for_secs = sleep_for_secs as u64;
    let mut blob_idx = 0;
    loop {
        let start = std::time::Instant::now();
        println!("blob index {:?}", blob_idx);
        for i in 0..blob_size {
            let random_u8: u8 = rng.gen();
            data[i] = random_u8;
        }

        match cmd {
            Command::ZGDAStore(_) => zgdastore(&da, &data).await,
            Command::ZGDADisperse(_) => zgdadisperse(&da, &data).await,
        };
        println!("Took {:?}", start.elapsed());
        if stop {
            break;
        }
        if prog_start.elapsed() > std::time::Duration::from_secs(run_for_secs.into()) {
            println!("Terminating after {run_for_secs} seconds");
            break ();
        }

        let t = start.elapsed().as_secs();
        if sleep_for_secs > t {
            tokio::time::sleep(std::time::Duration::from_secs(sleep_for_secs - t)).await;
        }

        blob_idx += 1;
    }

    let _ = metrics_server.await;
    Ok(())
}
