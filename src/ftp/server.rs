use libunftp::options::{ActivePassiveMode, PassiveHost};
use opendal::{services::S3, Operator};
use tokio::sync::mpsc;

use crate::aws::CachingAwsCredentialLoader;
use anyhow::Result;
use log::{debug, error, info};
use unftp_auth_jsonfile::JsonFileAuthenticator;
use unftp_sbe_opendal::OpendalStorage;

const DEFAULT_REGION: &str = "eu-west-2";
const DEFAULT_BUCKET: &str = "dev-s3-aeroftp";
const FTP_ADDRESS: &str = "0.0.0.0:21";
const PASSIVE_PORT_RANGE_START: u16 = 30000;
const PASSIVE_PORT_RANGE_END: u16 = 49999;

pub async fn start_ftp(
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
    done: mpsc::Sender<()>,
) -> Result<(), String> {
    let caching_provider = CachingAwsCredentialLoader::new();

    let region = std::env::var("AWS_S3_REGION").unwrap_or(DEFAULT_REGION.to_string());
    let bucket = std::env::var("AWS_S3_BUCKET").unwrap_or(DEFAULT_BUCKET.to_string());

    let builder = S3::default()
        .customized_credential_load(Box::new(caching_provider))
        .endpoint("https://s3.amazonaws.com")
        .region(&region)
        .bucket(&bucket)
        .root("/");

    // 2. Initialize the Operator
    let op: Operator = Operator::new(builder)
        .map_err(|e| format!("Could not build operator: {}", e))?
        .finish();

    // Wrap the operator with `OpendalStorage`
    let backend = OpendalStorage::new(op);

    let authenticator = JsonFileAuthenticator::from_file(String::from("credentials.json"))
        .map_err(|e| format!("Could not load credentials file: {}", e))?;

    // Build the actual unftp server, this could be used to create two separate
    // IPv4 and IPv6 servers with different settings
    // let auth4 = authenticator.clone();
    // let backend4 = backend.clone();
    // let mut shutdown4 = shutdown.resubscribe();
    // let done4 = done.clone();
    // let server4 = libunftp::ServerBuilder::new(Box::new(move || backend4.clone()))
    //     .authenticator(Arc::new(auth4))
    //     .shutdown_indicator(async move {
    //         shutdown4.recv().await.ok();
    //         println!("Shutting down FTP server");
    //         libunftp::options::Shutdown::new().grace_period(Duration::from_secs(11))
    //     })
    //     .passive_host(libunftp::options::PassiveHost::FromConnection)
    //     .metrics()
    //     .build()
    //     .map_err(|e| format!("Could not build server: {}", e))?;

    // // Start the v4 server
    // tokio::spawn(async move {
    //     let addr = "0.0.0.0:2121";
    //     println!("Starting ftp server on {}", addr);
    //     if let Err(e) = server4.listen(addr).await {
    //         println!("FTP server error: {:?}", e)
    //     }
    //     println!("FTP exiting");
    //     drop(done4)
    // });

    let server = libunftp::ServerBuilder::new(Box::new(move || backend.clone()))
        .authenticator(std::sync::Arc::new(authenticator))
        .shutdown_indicator(async move {
            shutdown.recv().await.ok();
            debug!("Shutting down FTP server");
            libunftp::options::Shutdown::new().grace_period(std::time::Duration::from_secs(11))
        })
        .idle_session_timeout(600)
        // .proxy_protocol_mode(21)
        .active_passive_mode(ActivePassiveMode::ActiveAndPassive)
        .passive_host(PassiveHost::FromConnection)
        .passive_ports(PASSIVE_PORT_RANGE_START..=PASSIVE_PORT_RANGE_END)
        .metrics()
        .build()
        .map_err(|e| format!("Could not build server: {}", e))?;

    tokio::spawn(async move {
        // this allows us to listen on IPv4 and IPv6 simultaneously
        //let addr = "[::]:21";
        // this is now IPv4 only
        let addr = FTP_ADDRESS;
        info!("Starting ftp server on {}", addr);
        if let Err(e) = server.listen(addr).await {
            error!("FTP server error: {:?}", e)
        }
        debug!("FTP exiting");
        drop(done)
    });

    Ok(())
}
