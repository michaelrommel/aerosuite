//! Unit tests for the AWS credentials module.
//!
//! This test file covers:
//! - Type-safe credential wrappers (AccessKeyId, SecretAccessKey, SessionToken)
//! - AwsCreds struct parsing and validation
//! - CachingAwsCredentialLoader caching logic
//! - Concurrent access patterns

#[cfg(test)]
mod tests {
    use crate::aws::creds::{
        AccessKeyId, AwsCreds, CachingAwsCredentialLoader, Ec2Creds, SecretAccessKey, SessionToken,
    };
    use chrono::{TimeZone, Utc};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Tests for `AccessKeyId` type-safe wrapper
    mod access_key_id_tests {
        use super::*;

        #[test]
        fn test_new_key_id() {
            let key = AccessKeyId::new("MIRO1234567890ABCDEF".to_string());
            assert_eq!(key.as_str(), "MIRO1234567890ABCDEF");
        }

        #[test]
        fn test_display_key_id() {
            let key = AccessKeyId::new("MIRO1234567890ABCDEF".to_string());
            assert_eq!(format!("{}", key), "MIRO1234567890ABCDEF");
        }

        #[test]
        fn test_from_string() {
            let s = "MIRO1234567890ABCDEF".to_string();
            let key: AccessKeyId = s.clone().into();
            assert_eq!(key.as_str(), s);
        }

        #[test]
        fn test_from_ref_str() {
            let key: AccessKeyId = "MIRO1234567890ABCDEF".into();
            assert_eq!(key.as_str(), "MIRO1234567890ABCDEF");
        }

        #[test]
        fn test_from_borrowed_str() {
            let s = "MIRO9876543210FEDCBA";
            let key: AccessKeyId = s.into();
            assert_eq!(key.as_str(), s);
        }

        #[derive(serde::Deserialize)]
        struct TestWrapper {
            pub key: AccessKeyId,
        }

        #[test]
        fn test_deserialize_key_id() {
            let json = r#"{"key": "MIRO1234567890ABCDEF"}"#;
            let wrapper: TestWrapper = serde_json::from_str(json).unwrap();
            assert_eq!(wrapper.key.as_str(), "MIRO1234567890ABCDEF");
        }
    }

    /// Tests for `SecretAccessKey` secret protection
    mod secret_access_key_tests {
        use super::*;

        #[test]
        fn test_new_secret() {
            let secret = SecretAccessKey::new("pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2".to_string());
            assert_eq!(secret.as_str(), "pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2");
        }

        #[test]
        fn test_debug_redaction() {
            let secret = SecretAccessKey::new("testsecretkey".to_string());
            let debug_output = format!("{:?}", secret);
            assert_eq!(debug_output, "SecretAccessKey([REDACTED])");
            assert!(!debug_output.contains("testsecretkey"));
        }

        #[test]
        fn test_from_string() {
            let s = "pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2".to_string();
            let secret: SecretAccessKey = s.clone().into();
            assert_eq!(secret.as_str(), s);
        }

        #[test]
        fn test_from_ref_str() {
            let secret: SecretAccessKey = "pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2".into();
            assert_eq!(secret.as_str(), "pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2");
        }

        #[test]
        fn test_from_borrowed_str() {
            let s = "secretaccesskey123";
            let secret: SecretAccessKey = s.into();
            assert_eq!(secret.as_str(), s);
        }
    }

    /// Tests for `SessionToken` with temporary credential handling
    mod session_token_tests {
        use super::*;

        #[test]
        fn test_new_session_token() {
            let token =
                SessionToken::new("IQoJb3JpZ2luX2VjEJzDAgY5ZDExNzUwMjE1ODM4IgZI".to_string());
            assert_eq!(
                token.as_str(),
                "IQoJb3JpZ2luX2VjEJzDAgY5ZDExNzUwMjE1ODM4IgZI"
            );
        }

        #[test]
        fn test_display_session_token() {
            let token = SessionToken::new("testtoken".to_string());
            assert_eq!(format!("{}", token), "testtoken");
        }

        #[test]
        fn test_from_string() {
            let s = "testtoken".to_string();
            let token: SessionToken = s.clone().into();
            assert_eq!(token.as_str(), s);
        }

        #[test]
        fn test_from_ref_str() {
            let token: SessionToken = "testtoken123".into();
            assert_eq!(token.as_str(), "testtoken123");
        }

        #[test]
        fn test_from_borrowed_str() {
            let s = "IQoJb3JpZ2luX2VjEJzDAgY5ZDExNzUwMjE1ODM4IgZI";
            let token: SessionToken = s.into();
            assert_eq!(token.as_str(), s);
        }
    }

    /// Tests for `AwsCreds` struct and parsing
    mod aws_creds_tests {
        use super::*;

        #[test]
        fn test_default_creates_empty_creds() {
            let creds = AwsCreds::default();
            assert!(creds.role_arn().is_empty());
            assert!(creds.access_key_id().as_str().is_empty());
            assert!(creds.secret_access_key().as_str().is_empty());
            assert!(creds.session_token().as_str().is_empty());
            assert!(creds.expiration().is_empty());
        }

        #[test]
        fn test_debug_redacts_secrets() {
            let creds = AwsCreds::new(
                "arn:aws:iam::123456789012:role/test-role".to_string(),
                AccessKeyId::new("MIRO1234567890ABCDEF".to_string()),
                SecretAccessKey::new("secretkey".to_string()),
                SessionToken::new("testtoken".to_string()),
                "2030-12-31T23:59:59Z".to_string(),
            );
            let debug_output = format!("{:?}", creds);
            assert!(debug_output.contains("MIRO1234567890ABCDEF"));
            assert!(debug_output.contains("expiration")); // structure shows
            assert!(debug_output.contains("[REDACTED]"));
            assert!(!debug_output.contains("secretkey")); // actual secret redacted
        }

        #[test]
        fn test_expiry_from_valid_timestamp() {
            let valid_ts = "2030-12-31T23:59:59Z";
            let creds = AwsCreds::new(
                String::new(),
                AccessKeyId::new(String::new()),
                SecretAccessKey::new(String::new()),
                SessionToken::new(String::new()),
                valid_ts.to_string(),
            );
            let expiry = creds.expiry();
            assert!(expiry.is_some());
            let expiry_time = expiry.unwrap();
            // Should be in the future (after 2030)
            assert!(expiry_time > SystemTime::now());
        }

        #[test]
        fn test_expiry_from_empty_timestamp() {
            let creds = AwsCreds::default(); // Empty expiration
            let expiry = creds.expiry();
            assert!(expiry.is_none());
        }

        #[test]
        fn test_expiry_from_invalid_timestamp() {
            let creds = AwsCreds::new(
                String::new(),
                AccessKeyId::new(String::new()),
                SecretAccessKey::new(String::new()),
                SessionToken::new(String::new()),
                "invalid-rfc3339-date".to_string(),
            );
            let expiry = creds.expiry();
            assert!(expiry.is_none());
        }

        #[test]
        fn test_deserialize_aws_creds() {
            let json = r#"
            {
                "Type": "AWS",
                "AccessKeyId": "MIRO1234567890ABCDEF",
                "SecretAccessKey": "pK9dLmN2oP3qR4sT5uV6wX7yZ8aB1cD2",
                "Token": "sessiontoken",
                "Expiration": "2030-12-31T23:59:59Z"
            }
            "#;

            // Simulate what get_ec2_credentials does
            let ec2_creds: Ec2Creds = serde_json::from_str(json).unwrap();
            let creds = AwsCreds::new(
                String::new(),
                AccessKeyId::from(ec2_creds.access_key_id()),
                SecretAccessKey::from(ec2_creds.secret_access_key()),
                SessionToken::from(ec2_creds.token()),
                ec2_creds.expiration().to_string(),
            );

            assert_eq!(creds.access_key_id().as_str(), "MIRO1234567890ABCDEF");
            assert_eq!(creds.expiration(), "2030-12-31T23:59:59Z");
        }
    }

    /// Integration tests for `CachingAwsCredentialLoader`
    mod credential_loader_tests {
        use super::*;

        #[tokio::test]
        async fn test_default_loader() {
            let loader = CachingAwsCredentialLoader::default();
            let creds = loader.credentials.read().await;
            assert!(creds.expiration().is_empty());
        }

        #[tokio::test]
        async fn test_new_loader_has_empty_creds() {
            let loader = CachingAwsCredentialLoader::new();
            let creds = loader.credentials.read().await;
            assert!(creds.access_key_id().as_str().is_empty());
        }

        #[tokio::test]
        async fn test_cache_check_empty_credentials() {
            let loader = CachingAwsCredentialLoader::new();
            let cache: Option<AwsCreds> = loader.cache_check().await;
            assert!(cache.is_none());
        }

        #[tokio::test]
        async fn test_cache_check_with_fresh_credentials() {
            let loader = CachingAwsCredentialLoader::new();

            // Simulate fresh credentials with 24-hour expiry
            let future_ts = (SystemTime::now() + Duration::from_secs(24 * 3600))
                .duration_since(UNIX_EPOCH)
                .unwrap();
            let iso_string = Utc
                .timestamp_opt(future_ts.as_secs() as i64, 0)
                .single()
                .unwrap()
                .to_rfc3339();

            let creds = AwsCreds::new(
                String::new(),
                AccessKeyId::new("MIRO1234567890ABCDEF".to_string()),
                SecretAccessKey::new("secretkey".to_string()),
                SessionToken::new("sessiontoken".to_string()),
                iso_string,
            );

            // Assign to the lock guard directly
            {
                let mut guard = loader.credentials.write().await;
                *guard = creds.clone();
            }

            // Should return cached credentials (24h > 15min)
            let cache: Option<AwsCreds> = loader.cache_check().await;
            assert!(cache.is_some());
            assert_eq!(
                cache.unwrap().access_key_id().as_str(),
                "MIRO1234567890ABCDEF"
            );
        }

        #[tokio::test]
        async fn test_cache_check_expiring_credentials() {
            let loader = CachingAwsCredentialLoader::new();

            // Simulate credentials expiring in 5 minutes
            let soon_ts = (SystemTime::now() + Duration::from_secs(5 * 60))
                .duration_since(UNIX_EPOCH)
                .unwrap();
            let iso_string = Utc
                .timestamp_opt(soon_ts.as_secs() as i64, 0)
                .single()
                .unwrap()
                .to_rfc3339();

            let creds = AwsCreds::new(
                String::new(),
                AccessKeyId::new("MIRO1234567890ABCDEF".to_string()),
                SecretAccessKey::new("secretkey".to_string()),
                SessionToken::new("sessiontoken".to_string()),
                iso_string,
            );

            // Assign to the lock guard directly
            {
                let mut guard = loader.credentials.write().await;
                *guard = creds.clone();
            }

            // Should NOT return cached credentials (5min < 15min)
            let cache: Option<AwsCreds> = loader.cache_check().await;
            assert!(cache.is_none());
        }

        #[tokio::test]
        async fn test_cache_check_concurrent_access() {
            let loader = CachingAwsCredentialLoader::new();

            // Spawn multiple concurrent readers using Arc clone
            let loader_arc = std::sync::Arc::new(loader);
            let handles: Vec<_> = (0..10)
                .map(|_| {
                    let loader_clone = std::sync::Arc::clone(&loader_arc);
                    tokio::spawn(async move { loader_clone.cache_check().await })
                })
                .collect();

            // Wait for all tasks to complete and collect results
            let mut results = Vec::new();
            for handle in handles {
                match handle.await {
                    Ok(result) => results.push(result),
                    Err(e) => panic!("Task panicked: {}", e),
                }
            }

            // All should return None (empty cache)
            assert!(results.iter().all(|r: &Option<AwsCreds>| r.is_none()));
        }
    }

    /// Tests for credential provisioning logic
    mod provisioning_tests {
        use super::*;

        #[test]
        fn test_ec2_creds_parsing() {
            let json = r#"
            {
                "Code": "Success",
                "LastUpdated": "2024-01-01T12:00:00Z",
                "Type": "AWS",
                "AccessKeyId": "MIROXXXXXXXXXXXXX",
                "SecretAccessKey": "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
                "Token": "IQoJb3JpZ2luX2VjEJzDAgY5ZDExNzUwMjE1ODM4IgZI",
                "Expiration": "2024-12-31T23:59:59Z"
            }
            "#;

            let ec2_creds: Ec2Creds = serde_json::from_str(json).unwrap();
            assert_eq!(ec2_creds.cred_type(), "AWS");
            assert!(ec2_creds.expiration().contains("2024-12-31"));
        }

        #[test]
        fn test_ec2_creds_missing_optional_fields() {
            let json = r#"
            {
                "AccessKeyId": "MIROXXXXXXXXXXXXX",
                "SecretAccessKey": "secret",
                "Token": "token",
                "Expiration": "2024-12-31T23:59:59Z"
            }
            "#;

            let ec2_creds: Ec2Creds = serde_json::from_str(json).unwrap();
            assert_eq!(ec2_creds.access_key_id(), "MIROXXXXXXXXXXXXX");
            assert_eq!(ec2_creds.secret_access_key(), "secret");
        }
    }
}
