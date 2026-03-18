use anyhow::Error;
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "PascalCase")]
#[allow(unused)]
struct AwsCreds {
    // this is the structure of the AWS Metadata Service response
    role_arn: String,
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

async fn provision_credentials() -> Result<AwsCreds, Error> {
    let response = "{\"RoleArn\":\"arn:aws:iam::${ACCOUNT_ID}:role/ecsTaskExecutionRoleWithSSM\",\"AccessKeyId\":\"ASxxxxxxxxxxxxxxP2AP\",\"SecretAccessKey\":\"99xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxK0l\",\"Token\":\"IQoJb3xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx9e0kJZsW/z4=\",\"Expiration\":\"2026-02-05T22:38:09Z\"}";

    let creds: AwsCreds = serde_json::from_str(response).expect("Failed");
    Ok(creds)
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let credentials = provision_credentials().await?;
    println!("{:?}", credentials);
    Ok(())
}
