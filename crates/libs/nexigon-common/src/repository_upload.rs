//! Bounded, mutation-safe repository asset uploads.

use std::fs::File;
use std::io;
use std::io::Read as _;
use std::io::Seek as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Context as _;
use anyhow::bail;
use futures::stream;
use nexigon_api::types::repositories::CreateAssetAction;
use nexigon_api::types::repositories::CreateAssetOutput;
use nexigon_api::types::repositories::FinalizeAssetUploadAction;
use nexigon_api::types::repositories::GetAssetDetailsAction;
use nexigon_api::types::repositories::IssueAssetUploadUrlAction;
use nexigon_api::types::repositories::RepositoryAssetStatus;
use nexigon_client::Execute;
use nexigon_ids::ids::RepositoryId;
use reqwest::header::CONTENT_LENGTH;
use si_crypto_hashes::HashAlgorithm;
use si_crypto_hashes::HashDigest;
use si_crypto_hashes::Hasher;
use tokio::io::AsyncReadExt as _;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;

const UPLOAD_BUFFER_SIZE: usize = 64 * 1024;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const REQUIRED_VERIFIED_HEADERS: [&str; 3] = [
    "x-amz-meta-nexigon-asset-id",
    "x-amz-meta-nexigon-digest",
    "if-none-match",
];
const VERIFIED_INTEGRITY_HEADERS: [&str; 2] = ["x-amz-content-sha256", "x-amz-checksum-sha256"];

#[derive(Clone)]
struct UploadSettings {
    client: reqwest::Client,
    timeout: Duration,
}

impl Default for UploadSettings {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            timeout: UPLOAD_TIMEOUT,
        }
    }
}

/// Metadata that must remain stable from the first hash byte through the last
/// upload byte. On Unix the device/inode pair also binds path checks to the
/// descriptor that was opened before hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mtime_seconds: i64,
    #[cfg(unix)]
    mtime_nanoseconds: i64,
    #[cfg(unix)]
    ctime_seconds: i64,
    #[cfg(unix)]
    ctime_nanoseconds: i64,
}

impl FileSnapshot {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            mtime_seconds: metadata.mtime(),
            #[cfg(unix)]
            mtime_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            ctime_seconds: metadata.ctime(),
            #[cfg(unix)]
            ctime_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

struct PreparedUpload {
    file: tokio::fs::File,
    path: PathBuf,
    snapshot: FileSnapshot,
    digest: HashDigest,
}

#[derive(Debug)]
struct PreparedBlockingUpload {
    file: File,
    path: PathBuf,
    snapshot: FileSnapshot,
    digest: HashDigest,
}

impl PreparedUpload {
    async fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path.to_owned();
        let prepared = run_blocking_task("asset hashing task", {
            let path = path.clone();
            move || open_and_hash(&path)
        })
        .await?
        .context("hashing asset")?;
        Ok(Self {
            file: tokio::fs::File::from_std(prepared.file),
            path: prepared.path,
            snapshot: prepared.snapshot,
            digest: prepared.digest,
        })
    }

    fn size(&self) -> u64 {
        self.snapshot.len
    }

    async fn verify_path(&self, phase: &str) -> anyhow::Result<()> {
        let metadata = tokio::fs::metadata(&self.path)
            .await
            .with_context(|| format!("reading asset metadata {phase}"))?;
        ensure_snapshot(&self.snapshot, &metadata, phase)
    }

    async fn into_body(self) -> io::Result<(reqwest::Body, Arc<UploadProgress>)> {
        let progress = Arc::new(UploadProgress::default());
        let mut state = UploadStream {
            file: self.file,
            path: self.path,
            snapshot: self.snapshot.clone(),
            expected_digest: self.digest,
            hasher: Some(HashAlgorithm::Sha256.hasher()),
            remaining: self.snapshot.len,
            progress: progress.clone(),
        };
        if state.remaining == 0 {
            state.finish().await?;
        }
        let body_stream = stream::try_unfold(state, |state| async move {
            let progress = state.progress.clone();
            match state.next_chunk().await {
                Ok(next) => Ok(next),
                Err(error) => {
                    progress.fail(error.to_string());
                    Err(error)
                }
            }
        });
        Ok((reqwest::Body::wrap_stream(body_stream), progress))
    }
}

#[derive(Default)]
struct UploadProgress {
    bytes_read: AtomicU64,
    finished: AtomicBool,
    failure: OnceLock<String>,
}

impl UploadProgress {
    fn fail(&self, message: String) {
        let _ = self.failure.set(message);
    }

    fn ensure_finished(&self, expected_size: u64) -> anyhow::Result<()> {
        if let Some(error) = self.failure.get() {
            bail!("asset changed or could not be read while uploading: {error}");
        }
        let actual_size = self.bytes_read.load(Ordering::Acquire);
        if !self.finished.load(Ordering::Acquire) || actual_size != expected_size {
            bail!(
                "storage stopped reading the asset before upload completed ({actual_size} of {expected_size} bytes)"
            );
        }
        Ok(())
    }
}

struct UploadStream {
    file: tokio::fs::File,
    path: PathBuf,
    snapshot: FileSnapshot,
    expected_digest: HashDigest,
    hasher: Option<Hasher>,
    remaining: u64,
    progress: Arc<UploadProgress>,
}

impl UploadStream {
    async fn next_chunk(mut self) -> io::Result<Option<(Vec<u8>, Self)>> {
        if self.remaining == 0 {
            self.finish().await?;
            return Ok(None);
        }

        let chunk_size = self.remaining.min(UPLOAD_BUFFER_SIZE as u64) as usize;
        let mut buffer = vec![0; chunk_size];
        let read = self.file.read(&mut buffer).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "asset became shorter while uploading",
            ));
        }
        buffer.truncate(read);
        self.remaining -= read as u64;
        self.progress
            .bytes_read
            .fetch_add(read as u64, Ordering::AcqRel);
        self.hasher
            .as_mut()
            .ok_or_else(|| io::Error::other("upload digest was already finalized"))?
            .update(&buffer);

        if self.remaining == 0 {
            self.finish().await?;
        }
        Ok(Some((buffer, self)))
    }

    async fn finish(&mut self) -> io::Result<()> {
        if self.progress.finished.load(Ordering::Acquire) {
            return Ok(());
        }
        let metadata = self.file.metadata().await?;
        ensure_snapshot_io(&self.snapshot, &metadata, "while uploading")?;
        let path_metadata = tokio::fs::metadata(&self.path).await?;
        ensure_snapshot_io(&self.snapshot, &path_metadata, "while uploading")?;
        let actual_digest: HashDigest = self
            .hasher
            .take()
            .ok_or_else(|| io::Error::other("upload digest was already finalized"))?
            .finalize();
        if actual_digest != self.expected_digest {
            return Err(io::Error::other(
                "asset contents changed between hashing and upload",
            ));
        }
        self.progress.finished.store(true, Ordering::Release);
        Ok(())
    }
}

pub(super) async fn upload_repository_asset(
    executor: &mut impl Execute,
    repository_id: RepositoryId,
    path: &Path,
) -> anyhow::Result<CreateAssetOutput> {
    upload_repository_asset_with_settings(executor, repository_id, path, UploadSettings::default())
        .await
}

async fn upload_repository_asset_with_settings(
    executor: &mut impl Execute,
    repository_id: RepositoryId,
    path: &Path,
    settings: UploadSettings,
) -> anyhow::Result<CreateAssetOutput> {
    let prepared = PreparedUpload::open(path).await?;
    prepared.verify_path("before asset registration").await?;
    let size = prepared.size();
    let output = executor
        .execute(CreateAssetAction::new(
            repository_id,
            size,
            prepared.digest.clone(),
        ))
        .await
        .context("creating asset")?
        .context("creating asset")?;
    let (asset_id, newly_created) = match &output {
        CreateAssetOutput::AssetAlreadyExists(asset_id) => (asset_id, false),
        CreateAssetOutput::Created(asset_id) => (asset_id, true),
    };

    if !newly_created {
        let details = executor
            .execute(GetAssetDetailsAction::new(asset_id.clone()))
            .await
            .context("getting existing asset details")?
            .context("getting existing asset details")?;
        if matches!(details.status, RepositoryAssetStatus::Available) {
            return Ok(output);
        }
        if executor
            .execute(FinalizeAssetUploadAction::new(asset_id.clone()))
            .await
            .context("retrying asset finalization")?
            .is_ok()
        {
            return Ok(output);
        }
    }

    prepared.verify_path("before upload").await?;
    let upload = executor
        .execute(IssueAssetUploadUrlAction::new(asset_id.clone()))
        .await
        .context("issuing upload URL")?
        .context("issuing upload URL")?;
    let upload_url = upload
        .verified_url
        .context("hub did not provide a verified upload URL")?;
    let headers = upload
        .headers
        .context("hub did not provide verified upload headers")?;
    require_verified_headers(&headers)?;

    put_prepared_upload(prepared, upload_url, headers, &settings).await?;

    executor
        .execute(FinalizeAssetUploadAction::new(asset_id.clone()))
        .await
        .context("finalizing asset upload")?
        .context("finalizing asset upload")?;
    Ok(output)
}

async fn put_prepared_upload(
    prepared: PreparedUpload,
    upload_url: String,
    headers: std::collections::HashMap<String, String>,
    settings: &UploadSettings,
) -> anyhow::Result<()> {
    let size = prepared.size();
    let path = prepared.path.clone();
    let snapshot = prepared.snapshot.clone();
    let (body, progress) = prepared
        .into_body()
        .await
        .context("preparing asset upload stream")?;
    let mut request = settings
        .client
        .put(upload_url)
        .header(CONTENT_LENGTH, size)
        .body(body);
    for (name, value) in headers {
        request = request.header(name, value);
    }

    let response = match tokio::time::timeout(settings.timeout, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            if let Some(stream_error) = progress.failure.get() {
                bail!("reading asset during upload failed: {stream_error}");
            }
            return Err(error).context("uploading asset");
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "asset upload exceeded its {:?} transfer deadline",
                    settings.timeout
                )
            });
        }
    };
    response
        .error_for_status()
        .context("object storage rejected asset upload")?;
    progress.ensure_finished(size)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .context("reading asset metadata after upload")?;
    ensure_snapshot(&snapshot, &metadata, "after upload")?;
    Ok(())
}

fn require_verified_headers(
    headers: &std::collections::HashMap<String, String>,
) -> anyhow::Result<()> {
    for required in REQUIRED_VERIFIED_HEADERS {
        let present = headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case(required) && !value.is_empty());
        if !present {
            bail!("hub did not provide required verified upload header {required}");
        }
    }
    let has_integrity_header = VERIFIED_INTEGRITY_HEADERS.iter().any(|required| {
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case(required) && !value.is_empty())
    });
    if !has_integrity_header {
        bail!(
            "hub did not provide a required verified upload integrity header ({})",
            VERIFIED_INTEGRITY_HEADERS.join(" or ")
        );
    }
    Ok(())
}

async fn run_blocking_task<T, F>(name: &'static str, task: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    finish_task(name, tokio::task::spawn_blocking(task).await)
}

fn finish_task<T>(
    name: &'static str,
    result: Result<T, tokio::task::JoinError>,
) -> anyhow::Result<T> {
    result.with_context(|| format!("{name} failed"))
}

fn open_and_hash(path: &Path) -> anyhow::Result<PreparedBlockingUpload> {
    open_and_hash_with_observer(path, |_| {})
}

fn open_and_hash_with_observer(
    path: &Path,
    mut observer: impl FnMut(u64),
) -> anyhow::Result<PreparedBlockingUpload> {
    let mut file = File::open(path).with_context(|| format!("opening asset {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("reading asset metadata for {}", path.display()))?;
    if !metadata.is_file() {
        bail!("asset path is not a regular file: {}", path.display());
    }
    let snapshot = FileSnapshot::from_metadata(&metadata);
    let mut hasher = HashAlgorithm::Sha256.hasher();
    let mut buffer = vec![0; UPLOAD_BUFFER_SIZE];
    let mut hashed = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading asset {} while hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        hashed += read as u64;
        observer(hashed);
    }
    ensure_snapshot(
        &snapshot,
        &file
            .metadata()
            .with_context(|| format!("reading asset metadata for {}", path.display()))?,
        "while hashing",
    )?;
    ensure_snapshot(
        &snapshot,
        &std::fs::metadata(path)
            .with_context(|| format!("reading asset path metadata for {}", path.display()))?,
        "while hashing",
    )?;
    file.seek(io::SeekFrom::Start(0))
        .with_context(|| format!("rewinding asset {} after hashing", path.display()))?;
    Ok(PreparedBlockingUpload {
        file,
        path: path.to_owned(),
        snapshot,
        digest: hasher.finalize(),
    })
}

fn ensure_snapshot(
    expected: &FileSnapshot,
    actual_metadata: &std::fs::Metadata,
    phase: &str,
) -> anyhow::Result<()> {
    let actual = FileSnapshot::from_metadata(actual_metadata);
    if &actual != expected {
        bail!("asset identity or metadata changed {phase}");
    }
    Ok(())
}

fn ensure_snapshot_io(
    expected: &FileSnapshot,
    actual_metadata: &std::fs::Metadata,
    phase: &str,
) -> io::Result<()> {
    ensure_snapshot(expected, actual_metadata, phase).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::collections::HashMap;
    use std::io;
    use std::io::Seek as _;
    use std::io::SeekFrom;
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use nexigon_api::Action;
    use nexigon_api::types::errors::ActionError;
    use nexigon_api::types::errors::ActionErrorKind;
    use nexigon_api::types::outputs::Empty;
    use nexigon_api::types::repositories::CreateAssetAction;
    use nexigon_api::types::repositories::CreateAssetOutput;
    use nexigon_api::types::repositories::FinalizeAssetUploadAction;
    use nexigon_api::types::repositories::GetAssetDetailsAction;
    use nexigon_api::types::repositories::GetAssetDetailsOutput;
    use nexigon_api::types::repositories::IssueAssetUploadUrlAction;
    use nexigon_api::types::repositories::IssueAssetUploadUrlOutput;
    use nexigon_api::types::repositories::RepositoryAssetStatus;
    use nexigon_client::Execute;
    use nexigon_ids::Generate as _;
    use nexigon_ids::ids::RepositoryAssetId;
    use nexigon_ids::ids::RepositoryId;
    use nexigon_rpc::ExecuteError;
    use tempfile::NamedTempFile;
    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    use super::FileSnapshot;
    use super::PreparedUpload;
    use super::REQUIRED_VERIFIED_HEADERS;
    use super::UPLOAD_BUFFER_SIZE;
    use super::UploadProgress;
    use super::UploadSettings;
    use super::UploadStream;
    use super::VERIFIED_INTEGRITY_HEADERS;
    use super::finish_task;
    use super::open_and_hash_with_observer;
    use super::put_prepared_upload;
    use super::require_verified_headers;
    use super::upload_repository_asset_with_settings;
    use si_crypto_hashes::HashAlgorithm;
    use si_crypto_hashes::HashDigest;

    #[derive(Clone, Copy)]
    enum StorageReply {
        Status(u16),
        Stall,
    }

    #[derive(Default)]
    struct StorageState {
        requests: AtomicUsize,
        successes: AtomicUsize,
        received_digests: Mutex<Vec<HashDigest>>,
    }

    fn required_headers() -> HashMap<String, String> {
        REQUIRED_VERIFIED_HEADERS
            .into_iter()
            .chain(VERIFIED_INTEGRITY_HEADERS)
            .map(|name| {
                let value = if name == "if-none-match" { "*" } else { "test" };
                (name.to_owned(), value.to_owned())
            })
            .collect()
    }

    async fn spawn_storage(
        replies: Vec<StorageReply>,
    ) -> (String, Arc<StorageState>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test storage");
        let address = listener.local_addr().expect("test storage address");
        let state = Arc::new(StorageState::default());
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.expect("accept upload");
                task_state.requests.fetch_add(1, Ordering::AcqRel);
                let (content_length, initial_body) = read_headers(&mut socket).await;
                if matches!(reply, StorageReply::Stall) {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }

                let mut hasher = HashAlgorithm::Sha256.hasher();
                let mut received = initial_body.len() as u64;
                hasher.update(&initial_body);
                let mut buffer = [0_u8; 16 * 1024];
                while received < content_length {
                    let limit =
                        usize::try_from((content_length - received).min(buffer.len() as u64))
                            .expect("bounded socket read");
                    let read = socket
                        .read(&mut buffer[..limit])
                        .await
                        .expect("read upload body");
                    assert_ne!(read, 0, "uploader closed before Content-Length");
                    hasher.update(&buffer[..read]);
                    received += read as u64;
                }
                let status = match reply {
                    StorageReply::Status(status) => status,
                    StorageReply::Stall => unreachable!(),
                };
                let reason = if (200..300).contains(&status) {
                    task_state.successes.fetch_add(1, Ordering::AcqRel);
                    task_state
                        .received_digests
                        .lock()
                        .expect("digest lock")
                        .push(hasher.finalize());
                    "OK"
                } else {
                    "Service Unavailable"
                };
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write upload response");
            }
        });
        (format!("http://{address}/asset"), state, task)
    }

    async fn read_headers(socket: &mut tokio::net::TcpStream) -> (u64, Vec<u8>) {
        let mut request = Vec::new();
        let header_end = loop {
            let mut buffer = [0_u8; 1024];
            let read = socket
                .read(&mut buffer)
                .await
                .expect("read request headers");
            assert_ne!(read, 0, "uploader closed before request headers");
            request.extend_from_slice(&buffer[..read]);
            assert!(request.len() <= 64 * 1024, "request headers too large");
            if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("ASCII request headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<u64>().expect("numeric Content-Length"))
            })
            .expect("Content-Length header");
        (content_length, request[header_end..].to_vec())
    }

    fn write_fixture(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create fixture");
        file.write_all(bytes).expect("write fixture");
        file.flush().expect("flush fixture");
        file
    }

    #[tokio::test]
    async fn multi_gib_sparse_file_is_read_in_bounded_chunks() {
        const SPARSE_SIZE: u64 = 3 * 1024 * 1024 * 1024;
        let file = NamedTempFile::new().expect("create sparse fixture");
        file.as_file()
            .set_len(SPARSE_SIZE)
            .expect("size sparse fixture");
        let std_file = std::fs::File::open(file.path()).expect("open sparse fixture");
        let snapshot =
            FileSnapshot::from_metadata(&std_file.metadata().expect("sparse fixture metadata"));
        let expected_digest: HashDigest = HashAlgorithm::Sha256.hash(b"");
        let progress = Arc::new(UploadProgress::default());
        let mut stream = UploadStream {
            file: tokio::fs::File::from_std(std_file),
            path: file.path().to_owned(),
            snapshot,
            expected_digest,
            hasher: Some(HashAlgorithm::Sha256.hasher()),
            remaining: SPARSE_SIZE,
            progress,
        };

        for _ in 0..32 {
            let (chunk, next) = stream
                .next_chunk()
                .await
                .expect("read sparse chunk")
                .expect("sparse stream continues");
            assert_eq!(chunk.len(), UPLOAD_BUFFER_SIZE);
            assert!(chunk.capacity() <= UPLOAD_BUFFER_SIZE);
            stream = next;
        }
    }

    #[test]
    fn verified_upload_requires_an_integrity_contract() {
        let mut headers = required_headers();
        headers.remove("x-amz-checksum-sha256");
        assert!(require_verified_headers(&headers).is_ok());

        let mut legacy_headers = required_headers();
        legacy_headers.remove("x-amz-content-sha256");
        assert!(require_verified_headers(&legacy_headers).is_ok());

        headers.remove("x-amz-content-sha256");
        let error = require_verified_headers(&headers).expect_err("integrity header is mandatory");
        assert!(
            error.to_string().contains("x-amz-content-sha256"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn mutation_during_hash_is_rejected() {
        let file = write_fixture(&vec![7; UPLOAD_BUFFER_SIZE * 3]);
        let path = file.path().to_owned();
        let mutation_path = path.clone();
        let mut mutated = false;
        let error = open_and_hash_with_observer(&path, |hashed| {
            if !mutated && hashed >= UPLOAD_BUFFER_SIZE as u64 {
                mutated = true;
                let mut writer = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&mutation_path)
                    .expect("open fixture for mutation");
                writer.seek(SeekFrom::End(-1)).expect("seek mutation");
                writer.write_all(&[9]).expect("mutate fixture");
                writer.flush().expect("flush mutation");
            }
        })
        .expect_err("hash must reject mutation");
        assert!(
            error.to_string().contains("changed while hashing"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn mutation_during_upload_is_rejected() {
        let file = write_fixture(&vec![3; UPLOAD_BUFFER_SIZE * 3]);
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare fixture");
        let progress = Arc::new(UploadProgress::default());
        let mut stream = UploadStream {
            file: prepared.file,
            path: prepared.path,
            snapshot: prepared.snapshot.clone(),
            expected_digest: prepared.digest,
            hasher: Some(HashAlgorithm::Sha256.hasher()),
            remaining: prepared.snapshot.len,
            progress,
        };
        let (_, next) = stream
            .next_chunk()
            .await
            .expect("first upload chunk")
            .expect("upload continues");
        stream = next;

        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(file.path())
            .expect("open fixture for mutation");
        writer
            .seek(SeekFrom::Start((UPLOAD_BUFFER_SIZE * 2) as u64))
            .expect("seek mutation");
        writer.write_all(&[4]).expect("mutate fixture");
        writer.flush().expect("flush mutation");

        let error = loop {
            match stream.next_chunk().await {
                Ok(Some((_, next))) => stream = next,
                Ok(None) => panic!("mutated stream completed"),
                Err(error) => break error,
            }
        };
        assert!(
            error.to_string().contains("changed"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn storage_non_success_and_stall_return_errors() {
        let file = write_fixture(b"storage failure fixture");
        let (url, _, server) = spawn_storage(vec![StorageReply::Status(503)]).await;
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare fixture");
        let error = put_prepared_upload(
            prepared,
            url,
            required_headers(),
            &UploadSettings::default(),
        )
        .await
        .expect_err("503 must fail upload");
        assert!(
            error.to_string().contains("object storage rejected"),
            "unexpected error: {error:#}"
        );
        server.await.expect("storage server task");

        let (url, _, server) = spawn_storage(vec![StorageReply::Stall]).await;
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare fixture");
        let settings = UploadSettings {
            client: reqwest::Client::new(),
            timeout: Duration::from_millis(50),
        };
        let error = put_prepared_upload(prepared, url, required_headers(), &settings)
            .await
            .expect_err("stalled storage must time out");
        assert!(
            error.to_string().contains("transfer deadline"),
            "unexpected error: {error:#}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn storage_connection_failure_is_returned() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("reserve unused port");
        let address = listener.local_addr().expect("unused port address");
        drop(listener);
        let file = write_fixture(b"connection failure fixture");
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare fixture");
        let error = put_prepared_upload(
            prepared,
            format!("http://{address}/asset"),
            required_headers(),
            &UploadSettings::default(),
        )
        .await
        .expect_err("connection failure must be returned");
        assert!(
            error.to_string().contains("uploading asset"),
            "unexpected error: {error:#}"
        );
    }

    #[tokio::test]
    async fn empty_asset_completes_the_stream_contract() {
        let file = write_fixture(b"");
        let (url, storage, server) = spawn_storage(vec![StorageReply::Status(200)]).await;
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare empty fixture");
        put_prepared_upload(
            prepared,
            url,
            required_headers(),
            &UploadSettings::default(),
        )
        .await
        .expect("upload empty fixture");
        assert_eq!(storage.successes.load(Ordering::Acquire), 1);
        server.await.expect("storage server task");
    }

    #[tokio::test]
    async fn upload_read_failure_is_returned() {
        let file = write_fixture(b"truncate after hashing");
        let prepared = PreparedUpload::open(file.path())
            .await
            .expect("prepare fixture");
        file.as_file().set_len(0).expect("truncate fixture");
        let progress = Arc::new(UploadProgress::default());
        let stream = UploadStream {
            file: prepared.file,
            path: prepared.path,
            snapshot: prepared.snapshot.clone(),
            expected_digest: prepared.digest,
            hasher: Some(HashAlgorithm::Sha256.hasher()),
            remaining: prepared.snapshot.len,
            progress,
        };
        let error = match stream.next_chunk().await {
            Ok(_) => panic!("truncated file unexpectedly uploaded"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn hashing_io_and_task_failures_are_errors_not_panics() {
        let missing = tempfile::tempdir()
            .expect("temporary directory")
            .path()
            .join("missing");
        let error = match PreparedUpload::open(&missing).await {
            Ok(_) => panic!("missing fixture unexpectedly opened"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("opening asset"),
            "unexpected error: {error:#}"
        );

        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let error = finish_task("injected hashing task", task.await)
            .expect_err("join failure must be returned");
        assert!(
            error.to_string().contains("injected hashing task failed"),
            "unexpected error: {error:#}"
        );
    }

    struct MockExecutor {
        asset_id: RepositoryAssetId,
        created: bool,
        available: bool,
        size: Option<u64>,
        digest: Option<HashDigest>,
        create_calls: usize,
        finalize_calls: usize,
        upload_url: String,
        storage: Arc<StorageState>,
    }

    impl MockExecutor {
        fn new(upload_url: String, storage: Arc<StorageState>) -> Self {
            Self {
                asset_id: RepositoryAssetId::generate(),
                created: false,
                available: false,
                size: None,
                digest: None,
                create_calls: 0,
                finalize_calls: 0,
                upload_url,
                storage,
            }
        }
    }

    fn output<A: Action, T: Any>(value: T) -> A::Output {
        let value: Box<dyn Any> = Box::new(value);
        *value
            .downcast::<A::Output>()
            .unwrap_or_else(|_| panic!("wrong mock output for {}", A::NAME))
    }

    impl Execute for MockExecutor {
        async fn execute<A: Action>(
            &mut self,
            action: A,
        ) -> Result<Result<A::Output, ActionError>, ExecuteError> {
            let action = &action as &dyn Any;
            if let Some(create) = action.downcast_ref::<CreateAssetAction>() {
                self.create_calls += 1;
                if self.created {
                    assert_eq!(self.size, Some(create.size));
                    assert_eq!(self.digest.as_ref(), Some(&create.digest));
                    return Ok(Ok(output::<A, _>(CreateAssetOutput::AssetAlreadyExists(
                        self.asset_id.clone(),
                    ))));
                }
                self.created = true;
                self.size = Some(create.size);
                self.digest = Some(create.digest.clone());
                return Ok(Ok(output::<A, _>(CreateAssetOutput::Created(
                    self.asset_id.clone(),
                ))));
            }
            if action.downcast_ref::<GetAssetDetailsAction>().is_some() {
                let status = if self.available {
                    RepositoryAssetStatus::Available
                } else {
                    RepositoryAssetStatus::Dangling
                };
                return Ok(Ok(output::<A, _>(GetAssetDetailsOutput::new(
                    self.asset_id.clone(),
                    self.size.expect("created asset size"),
                    self.digest.clone().expect("created asset digest"),
                    status,
                    0,
                ))));
            }
            if action.downcast_ref::<FinalizeAssetUploadAction>().is_some() {
                self.finalize_calls += 1;
                if self.storage.successes.load(Ordering::Acquire) == 0 {
                    return Ok(Err(ActionError::new(
                        ActionErrorKind::Invalid,
                        "uploaded object is missing".to_owned(),
                    )));
                }
                self.available = true;
                return Ok(Ok(output::<A, _>(Empty::new())));
            }
            if action.downcast_ref::<IssueAssetUploadUrlAction>().is_some() {
                return Ok(Ok(output::<A, _>(IssueAssetUploadUrlOutput {
                    url: self.upload_url.clone(),
                    verified_url: Some(self.upload_url.clone()),
                    headers: Some(required_headers()),
                })));
            }
            panic!("unexpected mock action {}", A::NAME);
        }
    }

    #[tokio::test]
    async fn retry_creates_one_available_asset_after_failed_put() {
        let fixture = b"one stable repository asset";
        let file = write_fixture(fixture);
        let expected_digest: HashDigest = HashAlgorithm::Sha256.hash(fixture);
        let (url, storage, server) =
            spawn_storage(vec![StorageReply::Status(503), StorageReply::Status(200)]).await;
        let mut executor = MockExecutor::new(url, storage.clone());
        let repository_id = RepositoryId::generate();

        upload_repository_asset_with_settings(
            &mut executor,
            repository_id.clone(),
            file.path(),
            UploadSettings::default(),
        )
        .await
        .expect_err("first PUT must fail");
        assert!(executor.created);
        assert!(!executor.available);

        let result = upload_repository_asset_with_settings(
            &mut executor,
            repository_id.clone(),
            file.path(),
            UploadSettings::default(),
        )
        .await
        .expect("retry succeeds");
        assert!(matches!(result, CreateAssetOutput::AssetAlreadyExists(_)));
        assert!(executor.available);
        assert_eq!(executor.create_calls, 2);
        assert_eq!(storage.requests.load(Ordering::Acquire), 2);
        assert_eq!(storage.successes.load(Ordering::Acquire), 1);
        assert_eq!(
            storage
                .received_digests
                .lock()
                .expect("digest lock")
                .as_slice(),
            &[expected_digest]
        );

        let result = upload_repository_asset_with_settings(
            &mut executor,
            repository_id,
            file.path(),
            UploadSettings::default(),
        )
        .await
        .expect("available retry is idempotent");
        assert!(matches!(result, CreateAssetOutput::AssetAlreadyExists(_)));
        assert_eq!(storage.requests.load(Ordering::Acquire), 2);
        server.await.expect("storage server task");
    }
}
