mod http;
mod metrics;

use anyhow::{Context, Error};
use chrono::{DateTime, TimeZone, Utc};
use libunftp::options::ActivePassiveMode;
use opendal::{services::S3, Operator};
use reqsign::AwsCredential;
use reqwest::Client;
use serde::Deserialize;
use std::{
    boxed::Box,
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

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
struct AwsCreds {
    // this is the structure of the AWS Metadata Service response
    #[allow(dead_code)]
    role_arn: String,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
struct Ec2Creds {
    // this is the structure of the AWS Metadata Service response from EC2
    #[allow(dead_code)]
    code: String,
    #[allow(dead_code)]
    last_updated: String,
    #[allow(dead_code)]
    #[serde(rename = "Type")]
    r#type: String,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

impl AwsCreds {
    // this allows us to get the expiry as a time from the string
    pub fn expiry(&self) -> Option<SystemTime> {
        if self.expiration.is_empty() {
            None
        } else {
            Some(
                DateTime::parse_from_rfc3339(&self.expiration)
                    .unwrap()
                    .into(),
            )
        }
    }
}

struct CachingAwsCredentialLoader {
    // this is a reference that can be shared across async tasks
    credentials: Arc<RwLock<AwsCreds>>,
}

impl CachingAwsCredentialLoader {
    pub fn new() -> Self {
        Self {
            credentials: Arc::new(RwLock::new(AwsCreds::default())),
        }
    }

    async fn check_cache(&self) -> Option<AwsCreds> {
        // this function is the read lock block
        let cached_credentials = self.credentials.read().await;
        if let Some(expiry) = cached_credentials.expiry() {
            // println!("Credentials were cached, expiry is {:?}", expiry);
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
                    println!("Credentials are expired");
                    None
                }
            }
        } else {
            None
        }
    }

    async fn get_ec2_credentials(&self, client: Client) -> Result<AwsCreds, Error> {
        let temp_token = client
            .put("http://169.254.169.254/latest/api/token")
            .header("X-aws-ec2-metadata-token-ttl-seconds", "21600")
            .send()
            .await
            .context("Could not fetch temp token")?
            .text()
            .await
            .context("Could not parse temp token")?;
        println!("temp_token: {:?}", temp_token);
        let iam_role = client
            .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("Could not fetch IAM role")?
            .text()
            .await
            .context("Could not parse IAM role")?;
        println!("iam role: {}", iam_role);
        let response = client
            .get(format!(
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/{}",
                iam_role
            ))
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("Could not fetch credentials")?
            .text()
            .await
            .unwrap();
        println!("Response: {:?}", response);
        let credentials = client
            .get(format!(
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/{}",
                iam_role
            ))
            .header("X-aws-ec2-metadata-token", &temp_token)
            .send()
            .await
            .context("Could not fetch credentials")?
            .json::<Ec2Creds>()
            .await
            .context("Failed to parse credentials")?;

        Ok(AwsCreds {
            role_arn: "".to_string(),
            access_key_id: credentials.access_key_id,
            secret_access_key: credentials.secret_access_key,
            token: credentials.token,
            expiration: credentials.expiration,
        })
    }

    async fn provision_credentials(&self, client: Client) -> Result<AwsCreds, Error> {
        let url = if let Ok(full_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
            Some(full_uri)
        } else if let Ok(rel_uri) = std::env::var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
            Some(format!("http://169.254.170.2{}", rel_uri))
        } else {
            None
        };

        match url {
            Some(url) => {
                // println!("Fetching from {}", url);
                client
                    .get(url)
                    .send()
                    .await
                    .context("Could not fetch metadata info")?
                    .json::<AwsCreds>()
                    .await
                    .context("Failed to parse credentials")
            }
            None => self.get_ec2_credentials(client).await,
        }
    }
}

#[async_trait::async_trait]
impl reqsign::AwsCredentialLoad for CachingAwsCredentialLoader {
    async fn load_credential(&self, client: Client) -> anyhow::Result<Option<AwsCredential>> {
        let credentials: AwsCreds;
        match self.check_cache().await {
            Some(c) => credentials = c,
            None => {
                credentials = self.provision_credentials(client).await?;
                // This creates a write lock block
                {
                    let mut credential_cache = self.credentials.write().await;
                    *credential_cache = credentials.clone();
                    println!(
                        "New credentials fetched and cached, expire at {:?}",
                        credentials.expiry()
                    );
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
        // struct AwsCredential is what the reqsign crate expects
        return Ok(Some(AwsCredential {
            access_key_id: credentials.access_key_id,
            secret_access_key: credentials.secret_access_key,
            session_token: Some(credentials.token),
            expires_in: expiry,
        }));
    }
}

async fn start_ftp(
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
    done: tokio::sync::mpsc::Sender<()>,
) -> Result<(), String> {
    pretty_env_logger::init();

    let caching_provider = CachingAwsCredentialLoader::new();

    let region = env::var("AWS_S3_REGION").unwrap_or("eu-west-2".to_string());
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
        .authenticator(Arc::new(authenticator))
        .shutdown_indicator(async move {
            shutdown.recv().await.ok();
            println!("Shutting down FTP server");
            libunftp::options::Shutdown::new().grace_period(Duration::from_secs(11))
        })
        .idle_session_timeout(600)
        // .proxy_protocol_mode(21)
        .active_passive_mode(ActivePassiveMode::ActiveAndPassive)
        .passive_host(libunftp::options::PassiveHost::FromConnection)
        .passive_ports(30000..=49999)
        .metrics()
        .build()
        .map_err(|e| format!("Could not build server: {}", e))?;

    tokio::spawn(async move {
        // this allows us to listen on IPv4 and IPv6 simultaneously
        //let addr = "[::]:21";
        // this is now IPv4 only
        let addr = "0.0.0.0:21";
        println!("Starting ftp server on {}", addr);
        if let Err(e) = server.listen(addr).await {
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
