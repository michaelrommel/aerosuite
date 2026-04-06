//! Simple test application for writing files to S3 storage.
//!
//! Demonstrates basic OpenDAL usage with AWS S3 credentials
//! loaded from environment variables or configuration.

use anyhow::Result;
use opendal::options;
// use opendal::services::Azdls;
use opendal::services::S3;
use opendal::Operator;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    pretty_env_logger::init();

    // // 1. Create and configure the AZDLS builder{{{
    // let builder = Azdls::default()
    //     // Required: Set the endpoint (e.g., https://<account>.dfs.core.windows.net)
    //     .endpoint("https://lzstrXXXXXXXXXXXXuplgld.dfs.core.windows.net/")
    //     // Required: Set the filesystem (container) name
    //     .filesystem("mrcan24")
    //     // Set the root path for all operations
    //     .root("/")
    //     // Set credentials (can also be loaded from environment variables)
    //     // .account_name("lzstrXXXXXXXXXXXXectupl")
    //     // .account_key("Ml/5qXXXXXXXXXSt0uYPIw==")
    //     .account_name("lzstrXXXXXXXXXXXXuplgld")
    //     .account_key("GBs99xxxxxxx+9ch+AStEdjlUg==");
    // // .tenant_id("cfd26XXXXXXXXXXXXXXXXXXXXXX15d884")
    // // .client_id("fe72aXXXXXXXXXXXXXXXXXXXXXXXeceb36")
    // // .client_secret("v~R8XXXXXXXXXXXXXXXXXXXXXXX9b5HwquU1aIJ");}}}

    let builder = S3::default()
        .endpoint("https://s3.amazonaws.com")
        .region("${REGION}")
        .bucket("dev-s3-aeroftp")
        .root("/");
    // .access_key_id("ASXXXXXXXXXXXX7BX25D"){{{
    // .secret_access_key("a/KI7XXXXXXXXXXXXXXXXXXXXXXXXXXXX9upV5Xw")
    // .session_token("IQoJb3JpZ2luX21111111111111hkIekIwmty26Ju8mcJrFgbpB1bQo3M55/bD");}}}

    // Initialize the Operator
    let op: Operator = Operator::new(builder)?.finish();

    op.write_options(
        "nucleus-direct-test.txt",
        "Hello, World!",
        options::WriteOptions {
            user_metadata: Some(HashMap::from([(
                "serial".to_string(),
                "123456".to_string(),
            )])),
            ..Default::default()
        },
    )
    .await?;

    // let caps = op.info().full_capability();{{{
    // println!("{}", caps.write_with_user_metadata);

    // op.write_with("test-accountkey-writewith.txt", "Hello, World!")
    //     .user_metadata(vec![("owner".to_string(), "michael".to_string())])
    //     .await?;}}}

    Ok(())
}
