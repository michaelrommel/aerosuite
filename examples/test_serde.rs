use anyhow::Error;
use reqwest::Client;
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
struct AwsCreds {
    // this is the structure of the AWS Metadata Service response
    role_arn: String,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

async fn provision_credentials(client: Client) -> Result<AwsCreds, Error> {
    let response = "{\"RoleArn\":\"arn:aws:iam::295934382486:role/ecsTaskExecutionRoleWithSSM\",\"AccessKeyId\":\"ASIXXXXXXXXXXXXXX2AP\",\"SecretAccessKey\":\"997NBxxxxxxxxxxxxxxxxvH60K0l\",\"Token\":\"IQoJbxxxxxxxxxxxxbQ9e0kJZsW/z4=\",\"Expiration\":\"2026-02-05T22:38:09Z\"}";

    let creds: AwsCreds = serde_json::from_str(response).expect("Failed");
    println!("{:#?}", creds);
    Ok(creds)
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let client = reqwest::Client::new();
    let credentials = provision_credentials(client).await?;
    println!("{:?}", credentials);
    Ok(())
}
