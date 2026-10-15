use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
const VM_PORT: u16 = 9002;
const VM_SKEW_SECS: i64 = 60;
const DEFAULT_GATEWAY: &str = "10.42.0.1";
const UNARY_TIMEOUT: Duration = Duration::from_secs(60);
/// The unary UploadFile cap; larger payloads must stream.
pub const UNARY_UPLOAD_MAX: usize = 4 * 1024 * 1024 - 64 * 1024;

pub struct Client {
    inner: BoxdApiClient<Channel>,
    auth: MetadataValue<tonic::metadata::Ascii>,
    source: &'static str,
    expires_at: Option<i64>,
}

struct Credential {
    source: &'static str,
    token: String,
    expires_at: Option<i64>,
    endpoint: String,
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

macro_rules! call_unary {
    ($self:ident.$method:ident($req:expr)) => {{
        $self.prepare().await?;
        let req = $req;
        match $self.inner.$method($self.unary(req.clone())).await {
            Err(status) if status.code() == Code::Unauthenticated && $self.source == "in_vm" => {
                $self.remint().await?;
                $self.inner.$method($self.unary(req)).await
            }
            other => other,
        }
    }};
}

impl Client {
    pub async fn connect() -> Result<Self> {
        let cred = resolve()?;
        let mut client = Self::open(&cred).await?;
        call_unary!(client.whoami(WhoamiRequest {})).map_err(status_error)?;
        Ok(client)
    }

    async fn open(cred: &Credential) -> Result<Self> {
        let channel = connect_channel(&cred.endpoint).await?;
        Ok(Self {
            inner: BoxdApiClient::new(channel),
            auth: bearer(&cred.token)?,
            source: cred.source,
            expires_at: cred.expires_at,
        })
    }

    async fn prepare(&mut self) -> Result<()> {
        let Some(exp) = self.expires_at else {
            return Ok(());
        };
        if self.source == "in_vm" && now_secs() + VM_SKEW_SECS >= exp {
            self.remint().await?;
        }
        Ok(())
    }

    async fn remint(&mut self) -> Result<()> {
        let minted = mint_vm()?
            .ok_or_else(|| Coded::new("auth", "the VM token expired and could not be renewed"))?;
        self.auth = bearer(&minted.token)?;
        self.expires_at = minted.expires_at;
        Ok(())
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
        match call_unary!(self.get_vm(GetVmRequest {
            vm_id: name.to_owned(),
        })) {
            Ok(response) => Ok(Some(response.into_inner())),
            Err(status) if status.code() == Code::NotFound => Ok(None),
            Err(status) => Err(status_error(status)),
        }
    }

    pub async fn list_vms(&mut self) -> Result<proto::ListVmsResponse> {
        Ok(call_unary!(self.list_vms(ListVmsRequest {
            org: String::new(),
            all_contexts: false,
        }))
        .map_err(status_error)?
        .into_inner())
    }

    /// Fails before anything is created when the snapshot is missing or not ready.
    pub async fn check_snapshot(&mut self, snapshot: &str) -> Result<()> {
        let info = match call_unary!(self.get_snapshot(GetSnapshotRequest {
            name: snapshot.to_owned(),
            org: String::new(),
        })) {
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
        let response = call_unary!(self.create_vm_from_snapshot(CreateVmFromSnapshotRequest {
            snapshot: snapshot.to_owned(),
            name: name.to_owned(),
        }))
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
                call_unary!(self.resume_vm(ResumeVmRequest {
                    vm_id: name.to_owned(),
                }))
                .map_err(status_error)?;
                true
            }
            "hibernated" => {
                call_unary!(self.wake_vm(WakeVmRequest {
                    vm_id: name.to_owned(),
                }))
                .map_err(status_error)?;
                true
            }
            "stopped" => {
                call_unary!(self.start_vm(StartVmRequest {
                    vm_id: name.to_owned(),
                }))
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
        let response = call_unary!(self.suggest_vm_name(SuggestVmNameRequest {}))
            .map_err(status_error)?
            .into_inner();
        if response.name.is_empty() {
            bail!("boxd did not suggest a machine name");
        }
        Ok(response.name)
    }

    pub async fn resize(&mut self, name: &str, vcpu: u32, memory_bytes: u64) -> Result<bool> {
        let response = call_unary!(self.resize_vm(ResizeVmRequest {
            vm_id: name.to_owned(),
            vcpu,
            memory_bytes,
        }))
        .map_err(status_error)?
        .into_inner();
        Ok(response.rebooted)
    }

    /// Ok(false) when the machine was already gone.
    pub async fn destroy(&mut self, name: &str) -> Result<bool> {
        match call_unary!(self.destroy_vm(DestroyVmRequest {
            vm_id: name.to_owned(),
        })) {
            Ok(_) => Ok(true),
            Err(status) if status.code() == Code::NotFound => Ok(false),
            Err(status) => Err(status_error(status)),
        }
    }

    pub async fn save_snapshot(&mut self, vm: &str, name: &str) -> Result<u64> {
        let created = call_unary!(self.create_snapshot(CreateSnapshotRequest {
            vm: vm.to_owned(),
            name: name.to_owned(),
        }))
        .map_err(status_error)?
        .into_inner();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let mut last = String::new();
        loop {
            let info = call_unary!(self.get_snapshot(GetSnapshotRequest {
                name: name.to_owned(),
                org: String::new(),
            }))
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
        call_unary!(self.upload_file(UploadFileRequest {
            vm_id: vm.to_owned(),
            path: path.to_owned(),
            data,
        }))
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
        self.prepare().await?;
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
        self.prepare().await?;
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

pub fn auth_source() -> Result<&'static str> {
    Ok(resolve()?.source)
}

fn resolve() -> Result<Credential> {
    let config_key = crate::store::api_key(&crate::store::dirs());
    let env_token = nonempty_env("BOXD_TOKEN");
    let env_api_key = nonempty_env("BOXD_API_KEY");
    let login = cli_token();
    let source = select_source(Available {
        config_key: config_key.is_some(),
        env_token: env_token.is_some(),
        env_api_key: env_api_key.is_some(),
        login: login.is_some(),
        vm: vm_hint(),
    })
    .ok_or_else(|| {
        Coded::new(
            "auth",
            "no boxd login found; run `boxd auth login`, set an API key, or run inside a boxd VM",
        )
    })?;
    match source {
        "api_key" => exchanged("api_key", &config_key.unwrap_or_default()),
        "BOXD_TOKEN" => Ok(Credential {
            source,
            token: env_token.unwrap_or_default(),
            expires_at: None,
            endpoint: ENDPOINT.to_owned(),
        }),
        "BOXD_API_KEY" => exchanged("BOXD_API_KEY", &env_api_key.unwrap_or_default()),
        "boxd_login" => Ok(Credential {
            source,
            token: login.unwrap_or_default(),
            expires_at: None,
            endpoint: ENDPOINT.to_owned(),
        }),
        "in_vm" => mint_vm()?
            .ok_or_else(|| {
                Coded::new(
                    "auth",
                    "inside a boxd VM but the metadata token was not available",
                )
            })
            .map_err(Into::into),
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy)]
struct Available {
    config_key: bool,
    env_token: bool,
    env_api_key: bool,
    login: bool,
    vm: bool,
}

fn select_source(have: Available) -> Option<&'static str> {
    if have.config_key {
        Some("api_key")
    } else if have.env_token {
        Some("BOXD_TOKEN")
    } else if have.env_api_key {
        Some("BOXD_API_KEY")
    } else if have.login {
        Some("boxd_login")
    } else if have.vm {
        Some("in_vm")
    } else {
        None
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// True inside a boxd VM, where there is no boxd daemon or login.
pub fn in_vm() -> bool {
    vm_hint()
}

/// `boxd machine exec` sets only the lowercase names.
fn vm_hint() -> bool {
    [
        "BOXD_VM_ID",
        "BOXD_VM_NAME",
        "BOXD_PROXY_ENDPOINT",
        "boxd_vm_id",
        "boxd_vm_name",
    ]
    .into_iter()
    .any(|name| nonempty_env(name).is_some())
}

fn exchanged(source: &'static str, api_key: &str) -> Result<Credential> {
    Ok(Credential {
        source,
        token: exchange(api_key)?,
        expires_at: None,
        endpoint: ENDPOINT.to_owned(),
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

fn mint_vm() -> Result<Option<Credential>> {
    if !vm_hint() {
        return Ok(None);
    }
    let gateway = std::fs::read_to_string("/proc/net/route")
        .ok()
        .and_then(|text| gateway_from_route(&text))
        .unwrap_or_else(|| DEFAULT_GATEWAY.to_owned());
    let url = format!("http://{gateway}:{VM_PORT}/v1/auth/token");
    let output = std::process::Command::new("curl")
        .args(["-fsS", "--max-time", "2", &url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("run curl for the VM token")?;
    if !output.status.success() {
        return Ok(None);
    }
    let minted = parse_vm_token(&output.stdout)?;
    Ok(Some(Credential {
        source: "in_vm",
        token: minted.token,
        expires_at: Some(minted.expires_at),
        endpoint: minted.endpoint,
    }))
}

struct Minted {
    token: String,
    expires_at: i64,
    endpoint: String,
}

fn parse_vm_token(bytes: &[u8]) -> Result<Minted> {
    let json: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| Coded::new("auth", "VM token response was not JSON"))?;
    let token = json
        .get("token")
        .and_then(|value| value.as_str())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| Coded::new("auth", "VM token response has no token"))?
        .to_owned();
    let expires_at = json
        .get("expires_at")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| Coded::new("auth", "VM token response has no expires_at"))?;
    let endpoint = match json.get("api_url").and_then(|value| value.as_str()) {
        Some(url) if !url.is_empty() => grpc_endpoint(url)?,
        _ => ENDPOINT.to_owned(),
    };
    Ok(Minted {
        token,
        expires_at,
        endpoint,
    })
}

/// `api_url` from the VM metadata service is `http://host:port`, not https.
fn grpc_endpoint(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| {
            Coded::new(
                "auth",
                format!("boxd api_url must be http or https, got {url}"),
            )
        })?;
    if rest.contains('@') {
        bail!(Coded::new(
            "auth",
            "boxd api_url must not include credentials",
        ));
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        bail!(Coded::new("auth", "boxd api_url has no host"));
    }
    Ok(url.to_owned())
}

fn gateway_from_route(text: &str) -> Option<String> {
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || fields[1] != "00000000" {
            continue;
        }
        let ip = u32::from_str_radix(fields[2], 16).ok()?;
        return Some(format!(
            "{}.{}.{}.{}",
            ip & 0xff,
            (ip >> 8) & 0xff,
            (ip >> 16) & 0xff,
            (ip >> 24) & 0xff
        ));
    }
    None
}

fn bearer(token: &str) -> Result<MetadataValue<tonic::metadata::Ascii>> {
    format!("Bearer {token}")
        .parse()
        .map_err(|_| Coded::new("auth", "auth token is not valid gRPC metadata").into())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

async fn connect_channel(endpoint: &str) -> Result<Channel> {
    let mut channel = Endpoint::from_shared(endpoint.to_owned())
        .map_err(|_| Coded::new("auth", format!("boxd endpoint {endpoint} is not a URL")))?;
    if endpoint.starts_with("https://") {
        let tls = ClientTlsConfig::new().with_enabled_roots();
        channel = channel.tls_config(tls).context("boxd TLS config")?;
    }
    // Streaming calls (exec, upload) can run for minutes, so no overall
    // timeout here; keepalives catch a dead connection instead.
    channel
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
                format!("cannot reach boxd at {endpoint} ({err}); check your network"),
            )
            .into()
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    const ROUTE: &str = "\
Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT
eth0 000012AC 0100A8C0 0003 0 0 0 00000000 0 0 0
eth0 00000000 0100270A 0003 0 0 0 00000000 0 0 0
";

    #[test]
    fn gateway_is_little_endian_default_route() {
        assert_eq!(gateway_from_route(ROUTE).as_deref(), Some("10.39.0.1"));
        assert!(
            gateway_from_route("Iface Destination Gateway\neth0 00000000 nothex 0\n").is_none()
        );
        assert!(gateway_from_route("Iface Destination Gateway\n").is_none());
    }

    #[test]
    fn auth_source_follows_the_resolution_order() {
        let none = Available {
            config_key: false,
            env_token: false,
            env_api_key: false,
            login: false,
            vm: false,
        };
        assert_eq!(select_source(none), None);
        assert_eq!(select_source(Available { vm: true, ..none }), Some("in_vm"));
        assert_eq!(
            select_source(Available {
                login: true,
                vm: true,
                ..none
            }),
            Some("boxd_login")
        );
        assert_eq!(
            select_source(Available {
                env_api_key: true,
                login: true,
                vm: true,
                ..none
            }),
            Some("BOXD_API_KEY")
        );
        assert_eq!(
            select_source(Available {
                env_token: true,
                env_api_key: true,
                login: true,
                vm: true,
                ..none
            }),
            Some("BOXD_TOKEN")
        );
        assert_eq!(
            select_source(Available {
                config_key: true,
                env_token: true,
                env_api_key: true,
                login: true,
                vm: true,
            }),
            Some("api_key")
        );
    }

    #[test]
    fn vm_token_json_parses_http_api_url() {
        let minted = parse_vm_token(
            br#"{"token":"test-token","expires_at":100,"api_url":"http://boxd.sh:9443"}"#,
        )
        .unwrap();
        assert_eq!(minted.token, "test-token");
        assert_eq!(minted.expires_at, 100);
        assert_eq!(minted.endpoint, "http://boxd.sh:9443");
        let fallback = parse_vm_token(br#"{"token":"t","expires_at":1,"api_url":""}"#).unwrap();
        assert_eq!(fallback.endpoint, ENDPOINT);
        assert!(parse_vm_token(br#"{"token":"","expires_at":1}"#).is_err());
        assert!(parse_vm_token(br#"{"token":"t"}"#).is_err());
        assert!(grpc_endpoint("file:///tmp").is_err());
        assert!(grpc_endpoint("http://user:pass@boxd.sh:9443").is_err());
    }
}
