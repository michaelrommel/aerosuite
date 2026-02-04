mod http;
mod metrics;

// use anyhow::Result;
// use opendal::services::Azdls;
use chrono::{TimeZone, Utc};
use opendal::{services::S3, Operator};
use reqsign::AwsCredential;
use reqwest::Client;
use std::{
    boxed::Box,
    // collections::HashMap,
    env,
    net::SocketAddr,
    process,
    result::Result,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
use unftp_auth_jsonfile::JsonFileAuthenticator;
use unftp_sbe_opendal::OpendalStorage;

// use aws_config::identity::IdentityCache;
use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;
use aws_credential_types::{provider::ProvideCredentials, Credentials};
use aws_types::SdkConfig;
// use reqwest::Error;
// use serde::Deserialize;
// use std::env;

struct CachingAwsCredentialLoader {
    config: SdkConfig,
    credentials: Arc<RwLock<Credentials>>,
}

impl CachingAwsCredentialLoader {
    pub fn new(sdk_config: aws_config::SdkConfig) -> Self {
        Self {
            config: sdk_config,
            credentials: Arc::new(RwLock::new(Credentials::new("", "", None, None, "custom"))),
        }
    }

    async fn check_cache(&self) -> Option<Credentials> {
        let cached_credentials = self.credentials.read().await;
        if let Some(expiry) = cached_credentials.expiry() {
            // println!("Credentials were cached, expiry is {:?}", expiry);
            // let expiry_utc = expiry.with_timezone(&Utc);
            match expiry.duration_since(SystemTime::now()) {
                Ok(n) => {
                    // println!("Credentials expire in {}", n.as_secs());
                    if n < Duration::from_secs(15 * 60) {
                        None
                    } else {
                        Some(cached_credentials.clone())
                    }
                }
                Err(_) => {
                    // println!("Credentials already expired");
                    None
                }
            }
        } else {
            None
        }
    }
}

#[async_trait::async_trait]
impl reqsign::AwsCredentialLoad for CachingAwsCredentialLoader {
    async fn load_credential(&self, _client: Client) -> anyhow::Result<Option<AwsCredential>> {
        let credentials: Credentials;
        match self.check_cache().await {
            Some(c) => credentials = c,
            None => {
                let provider = self.config.credentials_provider().unwrap();
                credentials = provider.provide_credentials().await?;
                {
                    let mut credential_cache = self.credentials.write().await;
                    *credential_cache = credentials.clone();
                    // println!("New credentials fetched and cached");
                }
            }
        }
        let duration = credentials
            .expiry()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .expect("SystemTime is before UNIX_EPOCH");
        let expiry = Utc
            .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
            .single();
        return Ok(Some(AwsCredential {
            access_key_id: credentials.access_key_id().to_string(),
            secret_access_key: credentials.secret_access_key().to_string(),
            session_token: credentials.session_token().map(|s| s.to_string()),
            expires_in: expiry,
        }));
    }
}

async fn start_ftp(
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
    done: tokio::sync::mpsc::Sender<()>,
) -> Result<(), String> {
    pretty_env_logger::init();

    let authenticator = JsonFileAuthenticator::from_file(String::from("credentials.json"))
        .map_err(|e| format!("Could not load credentials file: {}", e))?;

    // let builder = Azdls::default()
    //     // Required: Set the filesystem (container) name
    //     .filesystem("ingress")
    //     // Required: Set the endpoint (e.g., https://<account>.dfs.core.windows.net)
    //     .endpoint("https://lzstrXXXXXXXXXXXXectupl.dfs.core.windows.net/")
    //     // Set credentials (can also be loaded from en vironment variables)
    //     .account_name("lzstrXXXXXXXXXXXXectupl")
    //     .account_key("Ml/5qXXXXXXXXXSt0uYPIw==")
    //     // Set the root path for all operations
    //     .root("/mrcan24/");

    // let base_provider = aws_config::default_provider::credentials::default_provider().await;

    let region_provider = RegionProviderChain::default_provider().or_else("us-east-1");
    let config: SdkConfig = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        // .credentials_provider(SharedCredentialsProvider::new(base_provider))
        // .identity_cache(
        //     IdentityCache::lazy()
        //         .load_timeout(Duration::from_secs(5))
        //         .build(),
        // )
        .load()
        .await;

    let caching_provider = CachingAwsCredentialLoader::new(config);

    let region = env::var("AWS_S3_REGION").unwrap_or("${REGION}".to_string());
    let bucket = env::var("AWS_S3_BUCKET").unwrap_or("dev-s3-aeroftp".to_string());

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

    // Build the actual unftp server
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

    let server6 = libunftp::ServerBuilder::new(Box::new(move || backend.clone()))
        .authenticator(Arc::new(authenticator))
        .shutdown_indicator(async move {
            shutdown.recv().await.ok();
            println!("Shutting down FTP server");
            libunftp::options::Shutdown::new().grace_period(Duration::from_secs(11))
        })
        .passive_host(libunftp::options::PassiveHost::FromConnection)
        .passive_ports(40000..=49999)
        .metrics()
        .build()
        .map_err(|e| format!("Could not build server: {}", e))?;

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

    tokio::spawn(async move {
        // let addr = "[fc00:1234:0:0:13:0:0:13]:2121";
        let addr = "[::]:21";
        println!("Starting ftp server on {}", addr);
        if let Err(e) = server6.listen(addr).await {
            println!("FTP server error: {:?}", e)
        }
        println!("FTP exiting");
        drop(done)
    });

    Ok(())
}

#[derive(PartialEq)]
struct ExitSignal(pub &'static str);

async fn listen_for_signals() -> Result<ExitSignal, String> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term_sig = signal(SignalKind::terminate())
        .map_err(|e| format!("could not listen for TERM signals: {}", e))?;
    let mut int_sig = signal(SignalKind::interrupt())
        .map_err(|e| format!("Could not listen for INT signal: {}", e))?;
    let mut hup_sig = signal(SignalKind::hangup())
        .map_err(|e| format!("Could not listen for HUP signal: {}", e))?;

    let sig_name = tokio::select! {
        Some(_signal) = term_sig.recv() => {
            "SIG_TERM"
        },
        Some(_signal) = int_sig.recv() => {
            "SIG_INT"
        },
        Some(_signal) = hup_sig.recv() => {
            "SIG_HUP"
        },
    };
    Ok(ExitSignal(sig_name))
}

async fn main_task() -> Result<ExitSignal, String> {
    let (shutdown_sender, http_receiver) = tokio::sync::broadcast::channel(1);
    let (http_done_sender, mut shutdown_done_received) = tokio::sync::mpsc::channel(1);
    let ftp_done_sender = http_done_sender.clone();

    let addr = String::from("[::]:9090");
    tokio::spawn(async move {
        if let Err(e) = http::start(&addr, http_receiver, http_done_sender).await {
            println!("HTTP Server error: {}", e)
        }
    });

    start_ftp(shutdown_sender.subscribe(), ftp_done_sender).await?;

    let signal = listen_for_signals().await?;
    println!("Received signal {}, shutting down...", signal.0);

    drop(shutdown_sender);

    // When every sender has gone out of scope, the recv call
    // will return with an error. We ignore the error.
    let _ = shutdown_done_received.recv().await;

    Ok(signal)
}

async fn run() -> Result<(), String> {
    // We wait for a signal (HUP, INT, TERM). If the signal is a HUP,
    // we restart, otherwise we exit the loop and the program ends.
    while main_task().await? == ExitSignal("SIG_HUP") {
        println!("Restarting on HUP");
    }
    println!("Exiting");
    Ok(())
}

#[tokio::main]
async fn main() {
    // let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // let tmp_dir = env::temp_dir();
    // let _tmp_dir = tmp_dir.as_path().to_str().unwrap();

    // #[cfg(feature = "tokio_console")]
    // {
    let console_addr: SocketAddr = "127.0.0.1:6669"
        .parse()
        .map_err(|e| format!("could not parse tokio-console address: {}", e))
        .unwrap();

    // Convert SocketAddr to the format expected by console_subscriber
    let (ip, port) = match console_addr {
        SocketAddr::V4(addr) => (addr.ip().octets(), addr.port()),
        SocketAddr::V6(_) => {
            eprintln!("Error: tokio-console only supports IPv4 addresses");
            process::exit(1);
        }
    };

    console_subscriber::ConsoleLayer::builder()
        // set the address the server is bound to
        .server_addr((ip, port))
        // ... other configurations ...
        .init();
    // }

    if let Err(e) = run().await {
        eprintln!("\nError: {}", e);
        process::exit(1);
    };
}
