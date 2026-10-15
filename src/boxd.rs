use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Request, Status};

use crate::{ui, Coded};

pub mod proto {
    // Generated clients return tonic::Status by value.
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("boxd.api.v1");
}

use proto::boxd_api_client::BoxdApiClient;
use proto::{
    CreateSnapshotRequest, CreateVmFromSnapshotRequest, DestroyVmRequest, ExecChunk,
    GetSnapshotRequest, GetVmRequest, GetVmResponse, ListVmsRequest, ResizeVmRequest,
    ResumeVmRequest, StartVmRequest, SuggestVmNameRequest, UploadFileChunk, UploadFileRequest,
    WakeVmRequest, WhoamiRequest,
};

const ENDPOINT: &str = "https://boxd.sh:9443";
const TOKEN_URL: &str = "https://app.boxd.sh/api/v1/auth/token";
const UNARY_TIMEOUT: Duration = Duration::from_secs(60);
/// The unary UploadFile cap; larger payloads must stream.
pub const UNARY_UPLOAD_MAX: usize = 4 * 1024 * 1024 - 64 * 1024;

pub struct Client {
    inner: BoxdApiClient<Channel>,
    auth: MetadataValue<tonic::metadata::Ascii>,
}

pub struct ExecOutput {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn check(self, what: &str) -> Result<Self> {
        if self.code == 0 {
            return Ok(self);
        }
        let stderr = String::from_utf8_lossy(&self.stderr);
        let detail = stderr.trim().lines().last().unwrap_or("").to_owned();
        bail!(Coded::new(
            "remote_failed",
            format!("{what} failed on the machine (exit {}) {detail}", self.code).trim_end(),
        ));
    }
}

impl Client {
    pub async fn connect(token: &str) -> Result<Self> {
        let auth = format!("Bearer {token}")
            .parse()
            .context("auth token is not valid gRPC metadata")?;
        let tls = ClientTlsConfig::new().with_enabled_roots();
        // Streaming calls (exec, upload) can run for minutes, so no overall
        // timeout here; keepalives catch a dead connection instead.
        let channel = Endpoint::from_static(ENDPOINT)
            .tls_config(tls)
            .context("boxd TLS config")?
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_timeout(Duration::from_secs(20))
            .keep_alive_while_idle(true)
            .connect()
            .await
            .map_err(|err| {
                Coded::new(
                    "unavailable",
                    format!("cannot reach boxd at {ENDPOINT} ({err}); check your network"),
                )
            })?;
        let mut client = Self {
            inner: BoxdApiClient::new(channel),
            auth,
        };
        client
            .inner
            .whoami(client.unary(WhoamiRequest {}))
            .await
            .map_err(status_error)?;
        Ok(client)
    }

    fn unary<T>(&self, message: T) -> Request<T> {
        let mut request = self.stream(message);
        request.set_timeout(UNARY_TIMEOUT);
        request
    }

    fn stream<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.auth.clone());
        request
    }

    pub async fn get_vm(&mut self, name: &str) -> Result<Option<GetVmResponse>> {
        match self
            .inner
            .get_vm(self.unary(GetVmRequest {
                vm_id: name.to_owned(),
            }))
            .await
        {
            Ok(response) => Ok(Some(response.into_inner())),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status_error(status)),
        }
    }

    pub async fn list_vms(&mut self) -> Result<proto::ListVmsResponse> {
        Ok(self
            .inner
            .list_vms(self.unary(ListVmsRequest {
                org: String::new(),
                all_contexts: false,
            }))
            .await
            .map_err(status_error)?
            .into_inner())
    }

    /// Fails before anything is created when the snapshot is missing or not ready.
    pub async fn check_snapshot(&mut self, snapshot: &str) -> Result<()> {
        let info = match self
            .inner
            .get_snapshot(self.unary(GetSnapshotRequest {
                name: snapshot.to_owned(),
                org: String::new(),
            }))
            .await
        {
            Ok(response) => response.into_inner().snapshot,
            Err(status) if status.code() == Code::NotFound => None,
            Err(status) => return Err(status_error(status)),
        };
        match info {
            None => bail!(Coded::new(
                "snapshot_not_found",
                format!(
                    "snapshot {snapshot} does not exist; pick another with `herdr-boxd config set snapshot NAME`"
                ),
            )),
            Some(info) if info.status != "ready" => bail!(Coded::new(
                "snapshot_not_ready",
                format!("snapshot {snapshot} is {}; try again when it is ready", info.status),
            )),
            Some(_) => Ok(()),
        }
    }

    pub async fn create_from_snapshot(&mut self, snapshot: &str, name: &str) -> Result<String> {
        let response = self
            .inner
            .create_vm_from_snapshot(self.unary(CreateVmFromSnapshotRequest {
                snapshot: snapshot.to_owned(),
                name: name.to_owned(),
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        if response.name.is_empty() {
            bail!("boxd created a machine with no name");
        }
        Ok(response.name)
    }

    /// Wakes a suspended, hibernated, or stopped machine and waits for SSH.
    pub async fn ensure_running(&mut self, name: &str) -> Result<GetVmResponse> {
        let vm = self.get_vm(name).await?.ok_or_else(|| gone(name))?;
        let woke = match vm.status.as_str() {
            "running" if vm.ssh_port != 0 => return Ok(vm),
            "suspended" | "paused" => {
                self.inner
                    .resume_vm(self.unary(ResumeVmRequest {
                        vm_id: name.to_owned(),
                    }))
                    .await
                    .map_err(status_error)?;
                true
            }
            "hibernated" => {
                self.inner
                    .wake_vm(self.unary(WakeVmRequest {
                        vm_id: name.to_owned(),
                    }))
                    .await
                    .map_err(status_error)?;
                true
            }
            "stopped" => {
                self.inner
                    .start_vm(self.unary(StartVmRequest {
                        vm_id: name.to_owned(),
                    }))
                    .await
                    .map_err(status_error)?;
                true
            }
            _ => false,
        };
        if woke {
            ui::step(format!("waking {name} (was {})", vm.status));
        }
        self.wait_running(name, Duration::from_secs(180)).await
    }

    pub async fn wait_running(&mut self, name: &str, limit: Duration) -> Result<GetVmResponse> {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let vm = self.get_vm(name).await?.ok_or_else(|| gone(name))?;
            if vm.status == "running" && vm.ssh_port != 0 {
                return Ok(vm);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(Coded::new(
                    "timeout",
                    format!(
                        "{name} did not start in {}s (status {}, ssh port {})",
                        limit.as_secs(),
                        vm.status,
                        vm.ssh_port
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn ssh_host_key(&mut self) -> Result<String> {
        Ok(self.list_vms().await?.ssh_host_key)
    }

    pub async fn suggest_name(&mut self) -> Result<String> {
        let response = self
            .inner
            .suggest_vm_name(self.unary(SuggestVmNameRequest {}))
            .await
            .map_err(status_error)?
            .into_inner();
        if response.name.is_empty() {
            bail!("boxd did not suggest a machine name");
        }
        Ok(response.name)
    }

    pub async fn resize(&mut self, name: &str, vcpu: u32, memory_bytes: u64) -> Result<bool> {
        let response = self
            .inner
            .resize_vm(self.unary(ResizeVmRequest {
                vm_id: name.to_owned(),
                vcpu,
                memory_bytes,
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        Ok(response.rebooted)
    }

    /// Ok(false) when the machine was already gone.
    pub async fn destroy(&mut self, name: &str) -> Result<bool> {
        match self
            .inner
            .destroy_vm(self.unary(DestroyVmRequest {
                vm_id: name.to_owned(),
            }))
            .await
        {
            Ok(_) => Ok(true),
            Err(status) if status.code() == Code::NotFound => Ok(false),
            Err(status) => Err(status_error(status)),
        }
    }

    pub async fn save_snapshot(&mut self, vm: &str, name: &str) -> Result<u64> {
        let created = self
            .inner
            .create_snapshot(self.unary(CreateSnapshotRequest {
                vm: vm.to_owned(),
                name: name.to_owned(),
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let mut last = String::new();
        loop {
            let info = self
                .inner
                .get_snapshot(self.unary(GetSnapshotRequest {
                    name: name.to_owned(),
                    org: String::new(),
                }))
                .await
                .map_err(status_error)?
                .into_inner()
                .snapshot
                .context("boxd returned no snapshot")?;
            match info.status.as_str() {
                "ready" if info.version >= created.version => return Ok(info.version),
                "failed" => bail!("snapshot {name} failed"),
                _ if tokio::time::Instant::now() >= deadline => {
                    bail!(Coded::new(
                        "timeout",
                        format!("snapshot {name} is still {}", info.status)
                    ));
                }
                status => {
                    if status != last {
                        ui::step(format!("snapshot {name}: {status}"));
                        last = status.to_owned();
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    pub async fn upload_bytes(&mut self, vm: &str, path: &str, data: Vec<u8>) -> Result<()> {
        if data.len() > UNARY_UPLOAD_MAX {
            bail!(Coded::new(
                "too_large",
                format!("{path} is over {} MB", UNARY_UPLOAD_MAX / (1024 * 1024))
            ));
        }
        self.inner
            .upload_file(self.unary(UploadFileRequest {
                vm_id: vm.to_owned(),
                path: path.to_owned(),
                data,
            }))
            .await
            .map_err(status_error)?;
        Ok(())
    }

    pub async fn upload_file(&mut self, vm: &str, remote: &str, local: &Path) -> Result<()> {
        let file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("open {}", local.display()))?;
        let total = file.metadata().await.context("file size")?.len();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let vm_owned = vm.to_owned();
        let path_owned = remote.to_owned();
        let reader = tokio::spawn(async move {
            let mut file = file;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut first = true;
            let mut sent = 0u64;
            let mut next_report = 0u64;
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 && !first {
                    break;
                }
                sent += n as u64;
                // Small uploads finish before a progress line would help.
                if sent >= next_report && total >= 32 * 1024 * 1024 {
                    ui::step(format!(
                        "uploading {}/{} MB",
                        sent / (1024 * 1024),
                        total.div_ceil(1024 * 1024)
                    ));
                    next_report = sent + 32 * 1024 * 1024;
                }
                let chunk = UploadFileChunk {
                    vm_id: if first {
                        vm_owned.clone()
                    } else {
                        String::new()
                    },
                    path: if first {
                        path_owned.clone()
                    } else {
                        String::new()
                    },
                    total_size: if first { total } else { 0 },
                    data: buf[..n].to_vec(),
                };
                first = false;
                if tx.send(chunk).await.is_err() || n == 0 {
                    break;
                }
            }
            Ok::<u64, std::io::Error>(sent)
        });
        let response = self
            .inner
            .upload_file_stream(self.stream(tokio_stream::wrappers::ReceiverStream::new(rx)))
            .await
            .map_err(status_error);
        let sent = reader
            .await
            .context("upload reader")?
            .context("read upload file")?;
        let written = response?.into_inner().bytes_written;
        if sent != total || (written != 0 && written != total) {
            bail!(
                "upload of {} was short: sent {sent}, wrote {written}, expected {total} bytes",
                local.display()
            );
        }
        Ok(())
    }

    /// Runs `sh -c command` in the machine and collects its output.
    pub async fn exec(&mut self, vm: &str, command: &str) -> Result<ExecOutput> {
        let outbound = tokio_stream::iter(vec![ExecChunk {
            data: Vec::new(),
            stdin: false,
            tty: false,
            command: command.to_owned(),
            vm_id: vm.to_owned(),
            exit_code: 0,
            cols: 0,
            rows: 0,
            window_change: false,
            is_stderr: false,
            device_id: String::new(),
        }]);
        let mut inbound = self
            .inner
            .exec(self.stream(outbound))
            .await
            .map_err(status_error)?
            .into_inner();
        let mut out = ExecOutput {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        while let Some(chunk) = inbound.next().await {
            let chunk = chunk.map_err(status_error)?;
            if chunk.is_stderr {
                out.stderr.extend_from_slice(&chunk.data);
            } else {
                out.stdout.extend_from_slice(&chunk.data);
            }
            if chunk.exit_code != 0 {
                out.code = chunk.exit_code;
            }
        }
        Ok(out)
    }
}

fn gone(name: &str) -> anyhow::Error {
    Coded::new(
        "machine_gone",
        format!("{name} no longer exists in boxd; run `herdr-boxd destroy {name}` to clean up"),
    )
    .into()
}

pub fn load_token(api_key: Option<&str>) -> Result<String> {
    if let Some(key) = api_key {
        return exchange(key);
    }
    cli_token().ok_or_else(|| {
        Coded::new(
            "auth",
            "no boxd login found; run `boxd auth login` or `herdr-boxd config set api-key`",
        )
        .into()
    })
}

/// The key goes over stdin so it never shows up in `ps`.
fn exchange(api_key: &str) -> Result<String> {
    let body = serde_json::json!({ "api_key": api_key }).to_string();
    let mut child = std::process::Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "--max-time",
            "30",
            "-X",
            "POST",
            TOKEN_URL,
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run curl for the API key exchange")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(body.as_bytes()).context("send API key")?;
    }
    let output = child.wait_with_output().context("API key exchange")?;
    let parsed: Option<serde_json::Value> = serde_json::from_slice(&output.stdout).ok();
    if !output.status.success() {
        let detail = parsed
            .as_ref()
            .and_then(|v| v.get("error").or_else(|| v.get("message")))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_owned());
        bail!(Coded::new(
            "auth",
            format!("boxd rejected the API key ({detail}); set a new one with `herdr-boxd config set api-key`"),
        ));
    }
    parsed
        .as_ref()
        .and_then(|value| value.get("token"))
        .and_then(|value| value.as_str())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("token response has no token")
}

fn cli_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    [
        Path::new(&home).join("Library/Application Support/boxd/credentials.json"),
        Path::new(&home).join(".config/boxd/credentials.json"),
    ]
    .into_iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .find_map(|json| {
        json.get("token")
            .and_then(|value| value.as_str())
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    })
}

fn status_error(status: Status) -> anyhow::Error {
    let message = status.message();
    let detail = if message.is_empty() {
        format!("boxd gRPC {}", status.code())
    } else {
        format!("boxd: {message}")
    };
    let (code, hint) = match status.code() {
        Code::Unauthenticated => (
            "auth",
            " (run `boxd auth login`, or set an API key with `herdr-boxd config set api-key`)",
        ),
        Code::PermissionDenied => ("permission_denied", ""),
        Code::NotFound => ("not_found", ""),
        Code::AlreadyExists => ("already_exists", ""),
        Code::ResourceExhausted => ("quota", " (check your boxd plan and running machines)"),
        Code::InvalidArgument | Code::FailedPrecondition | Code::OutOfRange => ("invalid", ""),
        Code::Unavailable | Code::DeadlineExceeded => {
            ("unavailable", " (boxd is unreachable; retry shortly)")
        }
        _ => ("boxd", ""),
    };
    Coded::new(code, format!("{detail}{hint}")).into()
}
