//! AWS S3 storage wrapper around `aws-sdk-s3` library.
//!
//! Respects `prefix_in_bucket` property from [`S3Config`],
//! allowing multiple pageservers to independently work with the same S3 bucket, if
//! their bucket prefixes are both specified and different.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use anyhow::Context;
use aws_config::ecs::EcsCredentialsProvider;
use aws_sdk_s3::{config, types::ByteStream, Client, Credentials, Endpoint, Region};
use tokio::io::{self, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;
use tracing::{debug, trace};

use crate::{
    config::S3Config,
    remote_storage::{strip_path_prefix, RemoteStorage},
};

use super::StorageMetadata;

const S3_FILE_SEPARATOR: char = '/';

#[derive(Debug, Eq, PartialEq)]
pub struct S3ObjectKey(String);

impl S3ObjectKey {
    fn key(&self) -> &str {
        &self.0
    }

    fn download_destination(
        &self,
        pageserver_workdir: &Path,
        prefix_to_strip: Option<&str>,
    ) -> PathBuf {
        let path_without_prefix = match prefix_to_strip {
            Some(prefix) => self.0.strip_prefix(prefix).unwrap_or_else(|| {
                panic!(
                    "Could not strip prefix '{}' from S3 object key '{}'",
                    prefix, self.0
                )
            }),
            None => &self.0,
        };

        pageserver_workdir.join(
            path_without_prefix
                .split(S3_FILE_SEPARATOR)
                .collect::<PathBuf>(),
        )
    }
}

/// AWS S3 storage.
pub struct S3Bucket {
    pageserver_workdir: &'static Path,
    client: Client,
    bucket_name: String,
    prefix_in_bucket: Option<String>,
}

impl S3Bucket {
    /// Creates the S3 storage, errors if incorrect AWS S3 configuration provided.
    pub fn new(aws_config: &S3Config, pageserver_workdir: &'static Path) -> anyhow::Result<Self> {
        // TODO kb check this
        // Keeping a single client may cause issues due to timeouts.
        // https://github.com/rusoto/rusoto/issues/1686

        debug!(
            "Creating s3 remote storage for S3 bucket {}",
            aws_config.bucket_name
        );
        let mut config_builder = config::Builder::new()
            .region(Region::new(Cow::Owned(aws_config.bucket_region.clone())));

        if let Some(custom_endpoint) = &aws_config.endpoint {
            config_builder = config_builder.endpoint_resolver(Endpoint::immutable(
                custom_endpoint.parse().with_context(|| {
                    format!("Failed to parse endpoint '{}' as url", custom_endpoint)
                })?,
            ));
        }
        if aws_config.access_key_id.is_none() && aws_config.secret_access_key.is_none() {
            trace!("Using IAM-based AWS access");
            config_builder =
                config_builder.credentials_provider(EcsCredentialsProvider::builder().build());
        } else {
            trace!("Using credentials-based AWS access");
            config_builder = config_builder.credentials_provider(Credentials::new(
                aws_config.access_key_id.as_deref().unwrap_or_default(),
                aws_config.secret_access_key.as_deref().unwrap_or_default(),
                None,
                None,
                "pageserver static credentials",
            ));
        }
        let client = Client::from_conf(config_builder.build());

        let prefix_in_bucket = aws_config.prefix_in_bucket.as_deref().map(|prefix| {
            let mut prefix = prefix;
            while prefix.starts_with(S3_FILE_SEPARATOR) {
                prefix = &prefix[1..]
            }

            let mut prefix = prefix.to_string();
            while prefix.ends_with(S3_FILE_SEPARATOR) {
                prefix.pop();
            }
            prefix
        });

        Ok(Self {
            client,
            pageserver_workdir,
            bucket_name: aws_config.bucket_name.clone(),
            prefix_in_bucket,
        })
    }
}

#[async_trait::async_trait]
impl RemoteStorage for S3Bucket {
    type StoragePath = S3ObjectKey;

    fn storage_path(&self, local_path: &Path) -> anyhow::Result<Self::StoragePath> {
        let relative_path = strip_path_prefix(self.pageserver_workdir, local_path)?;
        let mut key = self.prefix_in_bucket.clone().unwrap_or_default();
        for segment in relative_path {
            key.push(S3_FILE_SEPARATOR);
            key.push_str(&segment.to_string_lossy());
        }
        Ok(S3ObjectKey(key))
    }

    fn local_path(&self, storage_path: &Self::StoragePath) -> anyhow::Result<PathBuf> {
        Ok(storage_path
            .download_destination(self.pageserver_workdir, self.prefix_in_bucket.as_deref()))
    }

    async fn list(&self) -> anyhow::Result<Vec<Self::StoragePath>> {
        let mut document_keys = Vec::new();

        let mut continuation_token = None;
        loop {
            let mut request = self.client.list_objects_v2().bucket(&self.bucket_name);
            if let Some(prefix) = &self.prefix_in_bucket {
                request = request.prefix(prefix);
            }
            if let Some(token) = &continuation_token {
                request = request.continuation_token(token);
            }

            let response = request.send().await?;
            document_keys.extend(
                response
                    .contents
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|o| Some(S3ObjectKey(o.key?))),
            );

            continuation_token = response.continuation_token;
            if continuation_token.is_none() {
                break;
            }
        }

        Ok(document_keys)
    }

    async fn upload(
        &self,
        from: impl io::AsyncRead + Unpin + Send + Sync + 'static,
        to: &Self::StoragePath,
        metadata: Option<StorageMetadata>,
    ) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket_name)
            .key(to.key())
            .body(ByteStream::from(hyper::Body::wrap_stream(
                ReaderStream::new(from),
            )))
            .set_metadata(metadata.map(|m| m.0))
            .send()
            .await?;
        Ok(())
    }

    async fn download(
        &self,
        from: &Self::StoragePath,
        to: &mut (impl io::AsyncWrite + Unpin + Send + Sync),
    ) -> anyhow::Result<Option<StorageMetadata>> {
        let object_output = self
            .client
            .get_object()
            .bucket(&self.bucket_name)
            .key(from.key())
            .send()
            .await?;

        let mut body = object_output.body;
        while let Some(bytes) = body.try_next().await? {
            to.write_all(&bytes).await?;
        }

        Ok(object_output.metadata.map(StorageMetadata))
    }

    async fn download_range(
        &self,
        from: &Self::StoragePath,
        start_inclusive: u64,
        end_exclusive: Option<u64>,
        to: &mut (impl io::AsyncWrite + Unpin + Send + Sync),
    ) -> anyhow::Result<Option<StorageMetadata>> {
        // S3 accepts ranges as https://www.w3.org/Protocols/rfc2616/rfc2616-sec14.html#sec14.35
        // and needs both ends to be exclusive
        let end_inclusive = end_exclusive.map(|end| end.saturating_sub(1));
        let range = Some(match end_inclusive {
            Some(end_inclusive) => format!("bytes={}-{}", start_inclusive, end_inclusive),
            None => format!("bytes={}-", start_inclusive),
        });
        let mut request = self
            .client
            .get_object()
            .bucket(&self.bucket_name)
            .key(from.key());
        if let Some(range) = range {
            request = request.range(range);
        }
        let object_output = request.send().await?;

        let mut body = object_output.body;
        while let Some(bytes) = body.try_next().await? {
            to.write_all(&bytes).await?;
        }

        Ok(object_output.metadata.map(StorageMetadata))
    }

    async fn delete(&self, path: &Self::StoragePath) -> anyhow::Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket_name)
            .key(path.key())
            .send()
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        layered_repository::metadata::METADATA_FILE_NAME,
        repository::repo_harness::{RepoHarness, TIMELINE_ID},
    };

    use super::*;

    #[test]
    fn download_destination() -> anyhow::Result<()> {
        let repo_harness = RepoHarness::create("download_destination")?;

        let local_path = repo_harness.timeline_path(&TIMELINE_ID).join("test_name");
        let relative_path = local_path.strip_prefix(&repo_harness.conf.workdir)?;

        let key = S3ObjectKey(format!(
            "{}{}",
            S3_FILE_SEPARATOR,
            relative_path
                .iter()
                .map(|segment| segment.to_str().unwrap())
                .collect::<Vec<_>>()
                .join(&S3_FILE_SEPARATOR.to_string()),
        ));

        assert_eq!(
            local_path,
            key.download_destination(&repo_harness.conf.workdir, None),
            "Download destination should consist of s3 path joined with the pageserver workdir prefix"
        );

        Ok(())
    }

    #[test]
    fn storage_path_positive() -> anyhow::Result<()> {
        let repo_harness = RepoHarness::create("storage_path_positive")?;

        let segment_1 = "matching";
        let segment_2 = "file";
        let local_path = &repo_harness.conf.workdir.join(segment_1).join(segment_2);

        let storage = dummy_storage(&repo_harness.conf.workdir);

        let expected_key = S3ObjectKey(format!(
            "{}{SEPARATOR}{}{SEPARATOR}{}",
            storage.prefix_in_bucket.as_deref().unwrap_or_default(),
            segment_1,
            segment_2,
            SEPARATOR = S3_FILE_SEPARATOR,
        ));

        let actual_key = storage
            .storage_path(local_path)
            .expect("Matching path should map to S3 path normally");
        assert_eq!(
            expected_key,
            actual_key,
            "S3 key from the matching path should contain all segments after the workspace prefix, separated with S3 separator"
        );

        Ok(())
    }

    #[test]
    fn storage_path_negatives() -> anyhow::Result<()> {
        #[track_caller]
        fn storage_path_error(storage: &S3Bucket, mismatching_path: &Path) -> String {
            match storage.storage_path(mismatching_path) {
                Ok(wrong_key) => panic!(
                    "Expected path '{}' to error, but got S3 key: {:?}",
                    mismatching_path.display(),
                    wrong_key,
                ),
                Err(e) => e.to_string(),
            }
        }

        let repo_harness = RepoHarness::create("storage_path_negatives")?;
        let storage = dummy_storage(&repo_harness.conf.workdir);

        let error_message = storage_path_error(&storage, &repo_harness.conf.workdir);
        assert!(
            error_message.contains("Prefix and the path are equal"),
            "Message '{}' does not contain the required string",
            error_message
        );

        let mismatching_path = PathBuf::from("somewhere").join("else");
        let error_message = storage_path_error(&storage, &mismatching_path);
        assert!(
            error_message.contains(mismatching_path.to_str().unwrap()),
            "Error should mention wrong path"
        );
        assert!(
            error_message.contains(repo_harness.conf.workdir.to_str().unwrap()),
            "Error should mention server workdir"
        );
        assert!(
            error_message.contains("is not prefixed with"),
            "Message '{}' does not contain a required string",
            error_message
        );

        Ok(())
    }

    #[test]
    fn local_path_positive() -> anyhow::Result<()> {
        let repo_harness = RepoHarness::create("local_path_positive")?;
        let storage = dummy_storage(&repo_harness.conf.workdir);
        let timeline_dir = repo_harness.timeline_path(&TIMELINE_ID);
        let relative_timeline_path = timeline_dir.strip_prefix(&repo_harness.conf.workdir)?;

        let s3_key = create_s3_key(
            &relative_timeline_path.join("not a metadata"),
            storage.prefix_in_bucket.as_deref(),
        );
        assert_eq!(
            s3_key.download_destination(
                &repo_harness.conf.workdir,
                storage.prefix_in_bucket.as_deref()
            ),
            storage
                .local_path(&s3_key)
                .expect("For a valid input, valid S3 info should be parsed"),
            "Should be able to parse metadata out of the correctly named remote delta file"
        );

        let s3_key = create_s3_key(
            &relative_timeline_path.join(METADATA_FILE_NAME),
            storage.prefix_in_bucket.as_deref(),
        );
        assert_eq!(
            s3_key.download_destination(
                &repo_harness.conf.workdir,
                storage.prefix_in_bucket.as_deref()
            ),
            storage
                .local_path(&s3_key)
                .expect("For a valid input, valid S3 info should be parsed"),
            "Should be able to parse metadata out of the correctly named remote metadata file"
        );

        Ok(())
    }

    #[test]
    fn download_destination_matches_original_path() -> anyhow::Result<()> {
        let repo_harness = RepoHarness::create("download_destination_matches_original_path")?;
        let original_path = repo_harness.timeline_path(&TIMELINE_ID).join("some name");

        let dummy_storage = dummy_storage(&repo_harness.conf.workdir);

        let key = dummy_storage.storage_path(&original_path)?;
        let download_destination = dummy_storage.local_path(&key)?;

        assert_eq!(
            original_path, download_destination,
            "'original path -> storage key -> matching fs path' transformation should produce the same path as the input one for the correct path"
        );

        Ok(())
    }

    fn dummy_storage(pageserver_workdir: &'static Path) -> S3Bucket {
        S3Bucket {
            pageserver_workdir,
            client: Client::new(&aws_types::SdkConfig::builder().build()),
            bucket_name: "dummy-bucket".to_string(),
            prefix_in_bucket: Some("dummy_prefix/".to_string()),
        }
    }

    fn create_s3_key(relative_file_path: &Path, prefix: Option<&str>) -> S3ObjectKey {
        S3ObjectKey(relative_file_path.iter().fold(
            prefix.unwrap_or_default().to_string(),
            |mut path_string, segment| {
                path_string.push(S3_FILE_SEPARATOR);
                path_string.push_str(segment.to_str().unwrap());
                path_string
            },
        ))
    }
}
