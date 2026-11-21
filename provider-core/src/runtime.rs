use std::{
    fs::File,
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{process::Child, process::Command, time::timeout};

pub const OLLAMA_VERSION: &str = "0.40.17";
const CHECKSUMS_URL: &str =
    "https://github.com/ollama/ollama/releases/download/v0.40.17/sha256sum.txt";
const REGISTRY_URL: &str = "https://registry.ollama.ai/v2";
const UNPACKED_SPACE_MULTIPLIER: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeState {
    NotInstalled,
    Downloading {
        percent: u8,
        bytes: u64,
        expected_total_download_bytes: u64,
    },
    Verifying,
    Ready,
    Starting,
    Running,
    Stopping,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeSuitability {
    Suitable,
    NotRecommended { reason: String },
}

pub type RuntimeProgressCallback = Arc<dyn Fn(RuntimeState) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub struct RuntimeInstall {
    pub executable_path: PathBuf,
    pub model_download_bytes: u64,
}

#[derive(Clone, Copy)]
struct RuntimeAsset {
    name: &'static str,
    size: u64,
    sha256: &'static str,
}

impl RuntimeAsset {
    fn url(self) -> String {
        format!(
            "https://github.com/ollama/ollama/releases/download/v{OLLAMA_VERSION}/{}",
            self.name
        )
    }

    fn archive_name(self) -> &'static str {
        self.name
    }
}

#[derive(Debug, Deserialize)]
struct RegistryManifest {
    layers: Vec<RegistryLayer>,
}

#[derive(Debug, Deserialize)]
struct RegistryLayer {
    size: u64,
}

pub fn runtime_suitability() -> RuntimeSuitability {
    let gpu = crate::detect_gpu();
    match gpu.support.as_str() {
        "nvidia-supported" | "apple-silicon-best-effort" => RuntimeSuitability::Suitable,
        "integrated-intel-unsupported" => RuntimeSuitability::NotRecommended {
            reason:
                "This integrated Intel GPU is not supported for jobs; inference will run on CPU."
                    .to_string(),
        },
        "integrated-amd-best-effort" => RuntimeSuitability::NotRecommended {
            reason: "AMD GPU job execution is not configured; inference may run on CPU."
                .to_string(),
        },
        "none" => RuntimeSuitability::NotRecommended {
            reason: "No supported GPU was detected; inference will run on CPU.".to_string(),
        },
        _ => RuntimeSuitability::NotRecommended {
            reason: format!(
                "GPU support is not validated for {}; inference may run on CPU.",
                gpu.gpu_name
            ),
        },
    }
}

pub struct RuntimeManager {
    data_dir: PathBuf,
    http: reqwest::Client,
}

impl RuntimeManager {
    pub fn new() -> Result<Self> {
        let data_dir = dirs::data_local_dir()
            .context("could not find the OS application data directory")?
            .join("GpuSwarm");
        Self::with_data_dir(data_dir)
    }

    pub fn with_data_dir(data_dir: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("GpuSwarm/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to configure the runtime HTTP client")?;
        Ok(Self { data_dir, http })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.data_dir.join("runtime").join(OLLAMA_VERSION)
    }

    pub fn executable_path(&self) -> Result<PathBuf> {
        find_ollama_executable(&self.runtime_dir())
    }

    pub async fn install(
        &self,
        model: &str,
        confirmed_not_recommended: bool,
        callback: RuntimeProgressCallback,
    ) -> Result<RuntimeInstall> {
        match self
            .install_inner(model, confirmed_not_recommended, &callback)
            .await
        {
            Ok(path) => {
                callback(RuntimeState::Ready);
                Ok(path)
            }
            Err(error) => {
                callback(RuntimeState::Error {
                    message: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn install_inner(
        &self,
        model: &str,
        confirmed_not_recommended: bool,
        callback: &RuntimeProgressCallback,
    ) -> Result<RuntimeInstall> {
        if self.runtime_dir().exists() {
            let executable_path = self.executable_path().with_context(|| {
                format!(
                    "runtime directory exists but contains no Ollama executable: {}",
                    self.runtime_dir().display()
                )
            });
            return Ok(RuntimeInstall {
                executable_path: executable_path?,
                model_download_bytes: 0,
            });
        }
        callback(RuntimeState::NotInstalled);
        ensure_download_confirmation(runtime_suitability(), confirmed_not_recommended)?;

        let asset = selected_asset()?;
        tokio::fs::create_dir_all(&self.data_dir)
            .await
            .with_context(|| format!("failed to create {}", self.data_dir.display()))?;
        let model_size = self.model_download_size(model).await?;
        let expected_download = asset
            .size
            .checked_add(model_size)
            .context("expected download size overflowed")?;
        let early_reserve = asset
            .size
            .checked_mul(1 + UNPACKED_SPACE_MULTIPLIER)
            .and_then(|bytes| bytes.checked_add(model_size))
            .context("required disk space overflowed")?;
        ensure_free_space(&self.data_dir, early_reserve).await?;

        tokio::fs::create_dir_all(self.models_dir())
            .await
            .context("failed to create the private model directory")?;
        let archive_path = self.data_dir.join(asset.archive_name());
        let part_path = self.data_dir.join(format!("{}.part", asset.archive_name()));
        callback(RuntimeState::Downloading {
            percent: 0,
            bytes: 0,
            expected_total_download_bytes: expected_download,
        });

        let official_checksum = self.fetch_official_checksum(asset).await?;
        let cached_archive = tokio::fs::metadata(&archive_path)
            .await
            .is_ok_and(|metadata| metadata.len() == asset.size);
        if cached_archive {
            callback(RuntimeState::Downloading {
                percent: 100,
                bytes: asset.size,
                expected_total_download_bytes: expected_download,
            });
        } else {
            if tokio::fs::metadata(&archive_path).await.is_ok() {
                tokio::fs::remove_file(&archive_path)
                    .await
                    .context("failed to discard incomplete runtime archive")?;
            }
            download_resumable(
                &self.http,
                &asset.url(),
                &part_path,
                asset.size,
                expected_download,
                callback,
            )
            .await?;
            tokio::fs::rename(&part_path, &archive_path)
                .await
                .context("failed to finalize the downloaded Ollama archive")?;
        }

        callback(RuntimeState::Verifying);
        if let Err(error) = verify_sha256(archive_path.clone(), official_checksum).await {
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(error).context("Ollama runtime archive checksum verification failed");
        }

        let extract_dir = self.data_dir.join("runtime").join(format!(
            ".{OLLAMA_VERSION}-extract-{}",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&extract_dir)
            .await
            .context("failed to create temporary runtime extraction directory")?;
        let extracted = tokio::task::spawn_blocking({
            let archive_path = archive_path.clone();
            let extract_dir = extract_dir.clone();
            move || extract_archive(&archive_path, &extract_dir)
        })
        .await
        .context("runtime extraction worker failed");
        let unpacked_size = match extracted {
            Ok(Ok(size)) => size,
            Ok(Err(error)) => {
                return Err(cleanup_extract_dir(&extract_dir, error).await);
            }
            Err(error) => {
                return Err(cleanup_extract_dir(&extract_dir, error).await);
            }
        };

        let extracted_and_model_size = match unpacked_size.checked_add(model_size) {
            Some(size) => size,
            None => {
                let error = anyhow::anyhow!("required disk space overflowed");
                return Err(cleanup_extract_dir(&extract_dir, error).await);
            }
        };
        if let Err(error) = ensure_free_space(&self.data_dir, extracted_and_model_size).await {
            let error = error.context(
                "insufficient free disk space to extract the runtime and download the model",
            );
            return Err(cleanup_extract_dir(&extract_dir, error).await);
        }

        let executable = match find_ollama_executable(&extract_dir) {
            Ok(executable) => executable,
            Err(error) => {
                return Err(cleanup_extract_dir(&extract_dir, error).await);
            }
        };
        #[cfg(target_os = "macos")]
        if let Err(error) = verify_macos_binary_architecture(&executable) {
            return Err(cleanup_extract_dir(&extract_dir, error).await);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = executable;

        let runtime_root = self.data_dir.join("runtime");
        tokio::fs::create_dir_all(&runtime_root)
            .await
            .context("failed to create the runtime directory")?;
        tokio::fs::rename(&extract_dir, self.runtime_dir())
            .await
            .context("failed to install the verified runtime")?;
        tokio::fs::remove_file(&archive_path)
            .await
            .context("runtime installed but failed to remove its archive")?;
        Ok(RuntimeInstall {
            executable_path: self.executable_path()?,
            model_download_bytes: model_size,
        })
    }

    async fn fetch_official_checksum(&self, asset: RuntimeAsset) -> Result<&'static str> {
        let response = self
            .http
            .get(CHECKSUMS_URL)
            .send()
            .await
            .context(
                "could not reach Ollama's official checksum file; check your internet connection",
            )?
            .error_for_status()
            .context("Ollama's official checksum file returned an HTTP error")?;
        let body = response
            .text()
            .await
            .context("could not read Ollama's official checksum file")?;
        let fetched = parse_checksum_file(&body, asset.name)
            .context("official checksum file does not list the selected runtime archive")?;
        if !fetched.eq_ignore_ascii_case(asset.sha256) {
            bail!(
                "official checksum for {} does not match the pinned v{} checksum",
                asset.name,
                OLLAMA_VERSION
            );
        }
        Ok(asset.sha256)
    }

    pub async fn model_download_size(&self, model: &str) -> Result<u64> {
        model_download_size(&self.http, model).await
    }

    pub async fn start(&self, callback: RuntimeProgressCallback) -> Result<RunningRuntime> {
        match self.start_inner(&callback).await {
            Ok(runtime) => Ok(runtime),
            Err(error) => {
                callback(RuntimeState::Error {
                    message: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn start_inner(&self, callback: &RuntimeProgressCallback) -> Result<RunningRuntime> {
        callback(RuntimeState::Starting);
        let executable = self.executable_path()?;
        tokio::fs::create_dir_all(self.models_dir())
            .await
            .context("failed to create the private model directory")?;
        let port = select_localhost_port()?;
        let api_url = format!("http://127.0.0.1:{port}");
        let mut child = Command::new(&executable)
            .arg("serve")
            .env("OLLAMA_HOST", format!("127.0.0.1:{port}"))
            .env("OLLAMA_MODELS", self.models_dir())
            .current_dir(self.runtime_dir())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start managed runtime at {}",
                    executable.display()
                )
            })?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child
                .try_wait()
                .context("failed to check managed Ollama process")?
            {
                bail!("managed Ollama process exited during startup with {status}");
            }
            if self
                .http
                .get(format!("{api_url}/api/version"))
                .timeout(Duration::from_secs(1))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                callback(RuntimeState::Running);
                return Ok(RunningRuntime {
                    child,
                    api_url,
                    port,
                    http: self.http.clone(),
                    models_dir: self.models_dir(),
                });
            }
            if Instant::now() >= deadline {
                child
                    .kill()
                    .await
                    .context("failed to stop Ollama after its startup timeout")?;
                bail!("managed Ollama runtime did not become ready within 30 seconds");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

pub struct RunningRuntime {
    child: Child,
    api_url: String,
    port: u16,
    http: reqwest::Client,
    models_dir: PathBuf,
}

impl RunningRuntime {
    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub async fn ensure_model(
        &self,
        model: &str,
        expected_model_bytes: u64,
        callback: RuntimeProgressCallback,
    ) -> Result<()> {
        match self
            .ensure_model_inner(model, expected_model_bytes, &callback)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => {
                callback(RuntimeState::Error {
                    message: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn ensure_model_inner(
        &self,
        model: &str,
        expected_model_bytes: u64,
        callback: &RuntimeProgressCallback,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct PullRequest<'a> {
            name: &'a str,
            stream: bool,
        }

        #[derive(Deserialize)]
        struct TagResponse {
            models: Vec<TaggedModel>,
        }

        #[derive(Deserialize)]
        struct TaggedModel {
            name: String,
        }

        let tags: TagResponse = self
            .http
            .get(format!("{}/api/tags", self.api_url))
            .send()
            .await
            .context("could not check models in the managed runtime")?
            .error_for_status()
            .context("managed runtime rejected the model-list request")?
            .json()
            .await
            .context("managed runtime returned an invalid model list")?;
        if tags
            .models
            .iter()
            .any(|entry| entry.name == model || entry.name.starts_with(&format!("{model}:")))
        {
            callback(RuntimeState::Running);
            return Ok(());
        }
        let expected_model_bytes = if expected_model_bytes == 0 {
            model_download_size(&self.http, model).await?
        } else {
            expected_model_bytes
        };
        ensure_free_space(&self.models_dir, expected_model_bytes)
            .await
            .context("insufficient free disk space to download the model")?;
        callback(RuntimeState::Downloading {
            percent: 0,
            bytes: 0,
            expected_total_download_bytes: expected_model_bytes,
        });
        let response = self
            .http
            .post(format!("{}/api/pull", self.api_url))
            .json(&PullRequest {
                name: model,
                stream: true,
            })
            .send()
            .await
            .context("model download failed; check your internet connection")?
            .error_for_status()
            .context("Ollama rejected the model download request")?;
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        let mut pending = Vec::new();
        let mut latest_total = expected_model_bytes;
        let mut latest_completed = 0_u64;
        let mut finished = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("model download was interrupted; retry to resume")?;
            pending.extend_from_slice(&chunk);
            while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = pending.drain(..=newline).collect();
                if process_pull_progress(&line, callback, &mut latest_total, &mut latest_completed)?
                {
                    finished = true;
                }
            }
        }
        if !pending.is_empty()
            && process_pull_progress(&pending, callback, &mut latest_total, &mut latest_completed)?
        {
            finished = true;
        }
        if !finished {
            bail!("model download ended before Ollama reported completion");
        }
        callback(RuntimeState::Running);
        Ok(())
    }

    pub async fn shutdown(mut self, callback: RuntimeProgressCallback) -> Result<()> {
        callback(RuntimeState::Stopping);
        if self
            .child
            .try_wait()
            .context("failed to check managed Ollama process before shutdown")?
            .is_none()
        {
            timeout(Duration::from_secs(10), self.child.kill())
                .await
                .context("timed out while stopping managed Ollama")?
                .context("failed to stop managed Ollama")?;
        }
        callback(RuntimeState::Ready);
        Ok(())
    }
}

async fn model_download_size(http: &reqwest::Client, model: &str) -> Result<u64> {
    let (name, tag) = model.rsplit_once(':').unwrap_or((model, "latest"));
    let name = name.strip_prefix("registry.ollama.ai/").unwrap_or(name);
    let name = if name.contains('/') {
        name.to_string()
    } else {
        format!("library/{name}")
    };
    let url = format!("{REGISTRY_URL}/{name}/manifests/{tag}");
    let response = http
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .send()
        .await
        .context("could not reach the model registry; check your internet connection")?
        .error_for_status()
        .context("model registry returned an error while estimating model size")?;
    let manifest: RegistryManifest = response
        .json()
        .await
        .context("model registry returned an invalid model manifest")?;
    manifest.layers.iter().try_fold(0_u64, |total, layer| {
        total
            .checked_add(layer.size)
            .context("model manifest size overflowed")
    })
}

#[derive(Deserialize)]
struct PullProgress {
    status: Option<String>,
    total: Option<u64>,
    completed: Option<u64>,
    error: Option<String>,
}

fn process_pull_progress(
    line: &[u8],
    callback: &RuntimeProgressCallback,
    total: &mut u64,
    completed: &mut u64,
) -> Result<bool> {
    let progress: PullProgress =
        serde_json::from_slice(line).context("Ollama returned invalid model-download progress")?;
    if let Some(error) = progress.error {
        bail!("Ollama model download failed: {error}");
    }
    if let Some(reported_total) = progress.total {
        *total = reported_total;
    }
    if let Some(reported_completed) = progress.completed {
        *completed = reported_completed;
    }
    let finished = progress.status.as_deref() == Some("success");
    let percent = if finished {
        100
    } else {
        (*completed)
            .saturating_mul(100)
            .checked_div(*total)
            .unwrap_or(0)
            .min(100) as u8
    };
    callback(RuntimeState::Downloading {
        percent,
        bytes: *completed,
        expected_total_download_bytes: *total,
    });
    Ok(finished)
}

fn select_localhost_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("failed to select a private localhost port for Ollama")?;
    Ok(listener.local_addr()?.port())
}

fn ensure_download_confirmation(
    suitability: RuntimeSuitability,
    confirmed_not_recommended: bool,
) -> Result<()> {
    if let RuntimeSuitability::NotRecommended { reason } = suitability {
        if !confirmed_not_recommended {
            bail!("runtime download requires explicit confirmation: {reason}");
        }
    }
    Ok(())
}

async fn cleanup_extract_dir(path: &Path, error: anyhow::Error) -> anyhow::Error {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "also failed to remove temporary runtime directory {}: {cleanup_error}",
            path.display()
        )),
    }
}

fn selected_asset() -> Result<RuntimeAsset> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Ok(RuntimeAsset {
            name: "ollama-windows-amd64.zip",
            size: 1_618_522_644,
            sha256: "3dd80f6e3a9699f2eadf4ac418f00529cee594df9557541806eda1f1f3f5588a",
        })
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        Ok(RuntimeAsset {
            name: "ollama-windows-arm64.zip",
            size: 387_167_070,
            sha256: "f5eab93c7e6dca27f47d556489fa89f760979c6f6f83e0bf1cb26d38630ce83a",
        })
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok(RuntimeAsset {
            name: "ollama-linux-amd64.tar.zst",
            size: 1_545_470_208,
            sha256: "1df9d6228bac5a22e420b242708c0d10e5b886db97bab99d446a41760f4e79af",
        })
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Ok(RuntimeAsset {
            name: "ollama-linux-arm64.tar.zst",
            size: 1_601_174_092,
            sha256: "0ec632480057cef6300459d1fa7dbbb3fc188b9a7d56cd0b0a72d05927d90ecc",
        })
    }
    #[cfg(target_os = "macos")]
    {
        Ok(RuntimeAsset {
            name: "ollama-darwin.tgz",
            size: 187_626_746,
            sha256: "4b9db20d077c5a1e52fb4a6f2e6791f76ae945fd07c9b1a5647c82e927826653",
        })
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "windows", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        target_os = "macos"
    )))]
    {
        bail!(
            "managed Ollama runtime is not available for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    }
}

fn parse_checksum_file<'a>(contents: &'a str, filename: &str) -> Result<&'a str> {
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let digest = fields.next()?;
            let listed_name = fields
                .next()?
                .trim_start_matches('*')
                .trim_start_matches("./");
            (listed_name == filename).then_some(digest)
        })
        .next()
        .context("checksum entry not found")
}

async fn download_resumable(
    http: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected_size: u64,
    expected_total_download_bytes: u64,
    callback: &RuntimeProgressCallback,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    if tokio::fs::metadata(destination)
        .await
        .is_ok_and(|metadata| metadata.len() == expected_size)
    {
        callback(RuntimeState::Downloading {
            percent: 100,
            bytes: expected_size,
            expected_total_download_bytes,
        });
        return Ok(());
    }

    let mut existing = tokio::fs::metadata(destination)
        .await
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if existing > expected_size {
        tokio::fs::remove_file(destination)
            .await
            .context("failed to discard oversized partial runtime archive")?;
        existing = 0;
    }

    let mut request = http.get(url);
    if existing > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
    }
    let response = request
        .send()
        .await
        .context("runtime download failed; check your internet connection")?
        .error_for_status()
        .context("runtime download returned an HTTP error")?;
    let append = existing > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    if append {
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .context("resumed download did not include a valid Content-Range")?;
        if !content_range.starts_with(&format!("bytes {existing}-")) {
            bail!("resumed runtime download returned an unexpected byte range");
        }
    }
    let offset = if append { existing } else { 0 };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(destination)
        .await
        .context("could not open partial runtime archive for writing")?;
    let mut received = offset;
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.context("runtime download was interrupted; it can be resumed on retry")?;
        file.write_all(&chunk)
            .await
            .context("failed to write runtime archive")?;
        received = received
            .checked_add(chunk.len() as u64)
            .context("runtime download size overflowed")?;
        if received > expected_size {
            bail!("runtime download exceeded the pinned archive size");
        }
        callback(RuntimeState::Downloading {
            percent: ((received.saturating_mul(100) / expected_total_download_bytes).min(99)) as u8,
            bytes: received,
            expected_total_download_bytes,
        });
    }
    file.flush()
        .await
        .context("failed to flush runtime archive")?;
    if received != expected_size {
        bail!(
            "runtime download is incomplete ({received} of {expected_size} bytes); retry to resume"
        );
    }
    Ok(())
}

async fn verify_sha256(path: PathBuf, expected: &str) -> Result<()> {
    let actual = tokio::task::spawn_blocking(move || -> Result<String> {
        let mut file = File::open(&path)
            .with_context(|| format!("failed to open archive {}", path.display()))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .context("failed while hashing runtime archive")?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        Ok(format!("{:x}", digest.finalize()))
    })
    .await
    .context("checksum worker failed")??;
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("SHA-256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

async fn ensure_free_space(path: &Path, required: u64) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let free = fs2::available_space(&path)
            .with_context(|| format!("could not check free disk space at {}", path.display()))?;
        if free < required {
            bail!(
                "insufficient disk space: need at least {} free, found {}",
                format_bytes(required),
                format_bytes(free)
            );
        }
        Ok(())
    })
    .await
    .context("disk-space check worker failed")?
}

fn format_bytes(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1024_f64.powi(3)))
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<u64> {
    #[cfg(target_os = "windows")]
    {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open {}", archive_path.display()))?;
        let mut archive = zip::ZipArchive::new(file).context("invalid Ollama ZIP archive")?;
        let mut unpacked_size = 0_u64;
        for index in 0..archive.len() {
            unpacked_size = unpacked_size
                .checked_add(archive.by_index(index)?.size())
                .context("runtime extracted size overflowed")?;
        }
        archive
            .extract(destination)
            .context("failed to extract Ollama ZIP archive")?;
        Ok(unpacked_size)
    }
    #[cfg(target_os = "linux")]
    {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open {}", archive_path.display()))?;
        let decoder = zstd::Decoder::new(file).context("invalid Ollama zstd archive")?;
        let mut archive = tar::Archive::new(decoder);
        let mut unpacked_size = 0_u64;
        for entry in archive.entries().context("invalid Ollama tar archive")? {
            unpacked_size = unpacked_size
                .checked_add(entry?.size())
                .context("runtime extracted size overflowed")?;
        }
        let file = File::open(archive_path)?;
        let decoder = zstd::Decoder::new(file)?;
        tar::Archive::new(decoder)
            .unpack(destination)
            .context("failed to extract Ollama zstd archive")?;
        Ok(unpacked_size)
    }
    #[cfg(target_os = "macos")]
    {
        let file = File::open(archive_path)
            .with_context(|| format!("failed to open {}", archive_path.display()))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut unpacked_size = 0_u64;
        for entry in archive.entries().context("invalid Ollama tar.gz archive")? {
            unpacked_size = unpacked_size
                .checked_add(entry?.size())
                .context("runtime extracted size overflowed")?;
        }
        let file = File::open(archive_path)?;
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(destination)
            .context("failed to extract Ollama tar.gz archive")?;
        Ok(unpacked_size)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        bail!("runtime archive extraction is unsupported on this platform")
    }
}

fn find_ollama_executable(root: &Path) -> Result<PathBuf> {
    let expected = if cfg!(target_os = "windows") {
        "ollama.exe"
    } else {
        "ollama"
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {}", directory.display()))
            }
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(expected)
            {
                return Ok(path);
            }
        }
    }
    bail!(
        "Ollama executable ({expected}) was not found in {}",
        root.display()
    )
}

#[cfg(target_os = "macos")]
fn verify_macos_binary_architecture(binary: &Path) -> Result<()> {
    let mut file = File::open(binary)
        .with_context(|| format!("failed to open Ollama binary {}", binary.display()))?;
    let mut header = [0_u8; 4096];
    let read = file.read(&mut header)?;
    if !mach_header_supports_arch(&header[..read], std::env::consts::ARCH) {
        bail!(
            "Ollama macOS archive does not contain a binary for this machine architecture ({})",
            std::env::consts::ARCH
        );
    }
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mach_header_supports_arch(header: &[u8], expected_arch: &str) -> bool {
    let expected_cpu = match expected_arch {
        "x86_64" => 0x0100_0007_u32,
        "aarch64" => 0x0100_000c_u32,
        _ => return false,
    };
    if header.len() < 8 {
        return false;
    }
    match &header[..4] {
        [0xcf, 0xfa, 0xed, 0xfe] => {
            u32::from_le_bytes(header[4..8].try_into().unwrap()) == expected_cpu
        }
        [0xfe, 0xed, 0xfa, 0xcf] => {
            u32::from_be_bytes(header[4..8].try_into().unwrap()) == expected_cpu
        }
        [0xca, 0xfe, 0xba, 0xbe] => fat_mach_supports_arch(header, expected_cpu, false),
        [0xca, 0xfe, 0xba, 0xbf] => fat_mach_supports_arch(header, expected_cpu, true),
        _ => false,
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn fat_mach_supports_arch(header: &[u8], expected_cpu: u32, fat64: bool) -> bool {
    let count = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    let entry_size = if fat64 { 32 } else { 20 };
    (0..count).any(|index| {
        let Some(offset) = index
            .checked_mul(entry_size)
            .and_then(|offset| offset.checked_add(8))
        else {
            return false;
        };
        header
            .get(offset..offset + 4)
            .map(|cpu| u32::from_be_bytes(cpu.try_into().unwrap()) == expected_cpu)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{
        download_resumable, ensure_download_confirmation, mach_header_supports_arch,
        parse_checksum_file, process_pull_progress, select_localhost_port, verify_sha256,
        RuntimeProgressCallback, RuntimeState, RuntimeSuitability,
    };

    #[test]
    fn parses_official_checksum_names() {
        let sums = "abc123  ./runtime.zip\nfff999 *other.zip\n";
        assert_eq!(parse_checksum_file(sums, "runtime.zip").unwrap(), "abc123");
        assert_eq!(
            parse_checksum_file(sums, "missing.zip")
                .unwrap_err()
                .to_string(),
            "checksum entry not found"
        );
    }

    #[tokio::test]
    async fn accepts_a_matching_sha256() {
        let path = std::env::temp_dir().join(format!(
            "gpuswarm-checksum-valid-test-{}.bin",
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, b"abc").unwrap();
        verify_sha256(
            path.clone(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        )
        .await
        .unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn download_state_exposes_expected_total() {
        let state = RuntimeState::Downloading {
            percent: 12,
            bytes: 120,
            expected_total_download_bytes: 1000,
        };
        assert!(matches!(
            state,
            RuntimeState::Downloading {
                expected_total_download_bytes: 1000,
                ..
            }
        ));
    }

    #[test]
    fn selects_an_available_private_localhost_port() {
        let port = select_localhost_port().unwrap();
        assert_ne!(port, 0);
        assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok());
    }

    #[test]
    fn requires_explicit_consent_on_not_recommended_hardware() {
        let suitability = RuntimeSuitability::NotRecommended {
            reason: "CPU-only".to_string(),
        };
        assert!(ensure_download_confirmation(suitability.clone(), false).is_err());
        assert!(ensure_download_confirmation(suitability, true).is_ok());
        assert!(ensure_download_confirmation(RuntimeSuitability::Suitable, false).is_ok());
    }

    #[test]
    fn parses_model_download_progress() {
        let states = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = states.clone();
        let callback: RuntimeProgressCallback = Arc::new(move |state| {
            observed.lock().unwrap().push(state);
        });
        let mut total = 100;
        let mut completed = 0;
        assert!(!process_pull_progress(
            br#"{"status":"downloading","total":100,"completed":25}"#,
            &callback,
            &mut total,
            &mut completed,
        )
        .unwrap());
        assert_eq!(total, 100);
        assert_eq!(completed, 25);
        assert_eq!(
            states.lock().unwrap().last(),
            Some(&RuntimeState::Downloading {
                percent: 25,
                bytes: 25,
                expected_total_download_bytes: 100,
            })
        );
    }

    #[tokio::test]
    async fn resumes_an_interrupted_archive_download() {
        let payload = b"verified runtime archive";
        let resume_from = 9;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 1024];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains(&format!("range: bytes={resume_from}-")));
            let rest = &payload[resume_from..];
            let response = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                rest.len(),
                resume_from,
                payload.len() - 1,
                payload.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(rest).await.unwrap();
        });

        let temp_dir =
            std::env::temp_dir().join(format!("gpuswarm-resume-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).unwrap();
        let partial_path = temp_dir.join("runtime.zip.part");
        fs::write(&partial_path, &payload[..resume_from]).unwrap();
        let callback: RuntimeProgressCallback = Arc::new(|_| {});
        download_resumable(
            &reqwest::Client::new(),
            &format!("http://{address}/archive"),
            &partial_path,
            payload.len() as u64,
            payload.len() as u64,
            &callback,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(fs::read(&partial_path).unwrap(), payload);
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_a_checksum_mismatch() {
        let path = std::env::temp_dir().join(format!(
            "gpuswarm-checksum-test-{}.bin",
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, b"not the expected content").unwrap();
        assert!(verify_sha256(path.clone(), &"0".repeat(64)).await.is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn recognizes_thin_and_universal_macos_architectures() {
        assert!(mach_header_supports_arch(
            &[0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01],
            "x86_64"
        ));
        let universal = [
            0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 1, 0x01, 0x00, 0x00, 0x0c, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert!(mach_header_supports_arch(&universal, "aarch64"));
        assert!(!mach_header_supports_arch(&universal, "x86_64"));
    }
}
