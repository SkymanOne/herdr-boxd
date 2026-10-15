use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_stream::StreamExt;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::{Request, Status};

pub mod proto {
    tonic::include_proto!("boxd.api.v1");
}

use proto::boxd_api_client::BoxdApiClient;
use proto::{
    CreateSnapshotRequest, CreateVmFromSnapshotRequest, DestroyVmRequest, ExecChunk,
    GetSnapshotRequest, GetVmRequest, ListVmsRequest, RebootVmRequest, ResizeVmRequest,
    SuggestVmNameRequest, UploadFileChunk, WhoamiRequest,
};

const ENDPOINT: &str = "https://boxd.sh:9443";
const TOKEN_URL: &str = "https://app.boxd.sh/api/v1/auth/token";

pub struct Client {
    inner: BoxdApiClient<Channel>,
    auth: MetadataValue<tonic::metadata::Ascii>,
}

impl Client {
    pub async fn connect(token: &str) -> Result<Self> {
        let auth = format!("Bearer {token}")
            .parse()
            .context("auth token is not valid gRPC metadata")?;
        let tls = ClientTlsConfig::new().with_enabled_roots();
        let channel = Channel::from_static(ENDPOINT)
            .tls_config(tls)
            .context("boxd TLS config")?
            .connect()
            .await
            .context("boxd gRPC connect failed")?;
        let mut client = Self {
            inner: BoxdApiClient::new(channel),
            auth,
        };
        client
            .inner
            .whoami(client.auth(WhoamiRequest {}))
            .await
            .map_err(status_error)?;
        Ok(client)
    }

    fn auth<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.auth.clone());
        request
    }

    pub async fn create_from_snapshot(&mut self, snapshot: &str, name: &str) -> Result<String> {
        let response = self
            .inner
            .create_vm_from_snapshot(self.auth(CreateVmFromSnapshotRequest {
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

    pub async fn wait_ssh(&mut self, name: &str) -> Result<proto::GetVmResponse> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let vm = self
                .inner
                .get_vm(self.auth(GetVmRequest {
                    vm_id: name.to_owned(),
                }))
                .await
                .map_err(status_error)?
                .into_inner();
            if vm.status == "running" && vm.ssh_port != 0 {
                return Ok(vm);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "{name} is not ready (status {}, ssh port {})",
                    vm.status,
                    vm.ssh_port
                );
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn ssh_host_key(&mut self) -> Result<String> {
        let response = self
            .inner
            .list_vms(self.auth(ListVmsRequest {
                org: String::new(),
                all_contexts: false,
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        if response.ssh_host_key.is_empty() {
            bail!("boxd did not return an SSH host key");
        }
        Ok(response.ssh_host_key)
    }

    pub async fn suggest_name(&mut self) -> Result<String> {
        let response = self
            .inner
            .suggest_vm_name(self.auth(SuggestVmNameRequest {}))
            .await
            .map_err(status_error)?
            .into_inner();
        if response.name.is_empty() {
            bail!("boxd did not suggest a machine name");
        }
        Ok(response.name)
    }

    pub async fn reboot(&mut self, name: &str) -> Result<()> {
        self.inner
            .reboot_vm(self.auth(RebootVmRequest {
                vm_id: name.to_owned(),
            }))
            .await
            .map_err(status_error)?;
        Ok(())
    }

    pub async fn resize(&mut self, name: &str, vcpu: u32, memory_bytes: u64) -> Result<bool> {
        let response = self
            .inner
            .resize_vm(self.auth(ResizeVmRequest {
                vm_id: name.to_owned(),
                vcpu,
                memory_bytes,
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        Ok(response.rebooted)
    }

    pub async fn destroy(&mut self, name: &str) -> Result<()> {
        self.inner
            .destroy_vm(self.auth(DestroyVmRequest {
                vm_id: name.to_owned(),
            }))
            .await
            .map_err(status_error)?;
        Ok(())
    }

    pub async fn save_snapshot(&mut self, vm: &str, name: &str) -> Result<u64> {
        let created = self
            .inner
            .create_snapshot(self.auth(CreateSnapshotRequest {
                vm: vm.to_owned(),
                name: name.to_owned(),
            }))
            .await
            .map_err(status_error)?
            .into_inner();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            let info = self
                .inner
                .get_snapshot(self.auth(GetSnapshotRequest {
                    name: name.to_owned(),
                    org: String::new(),
                }))
                .await
                .map_err(status_error)?
                .into_inner()
                .snapshot
                .context("boxd returned no snapshot")?;
            match info.status.as_str() {
                "ready" => return Ok(info.version.max(created.version)),
                "failed" => bail!("snapshot {name} failed"),
                _ if tokio::time::Instant::now() >= deadline => {
                    bail!("snapshot {name} still {}", info.status);
                }
                status => {
                    println!("{name} snapshot {status}…");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    pub async fn upload_tar(&mut self, vm: &str, remote_tar: &str, tar: &Path) -> Result<()> {
        let file = tokio::fs::File::open(tar).await.context("open tar")?;
        let total = file.metadata().await.context("tar size")?.len();
        println!("uploading {} MB", total.div_ceil(1024 * 1024));
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let vm_owned = vm.to_owned();
        let path_owned = remote_tar.to_owned();
        tokio::spawn(async move {
            let mut file = file;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut first = true;
            let mut sent = 0u64;
            loop {
                let n = match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => return,
                };
                sent += n as u64;
                if sent == n as u64 || sent % (32 * 1024 * 1024) < n as u64 {
                    println!(
                        "uploaded {}/{} MB",
                        sent / (1024 * 1024),
                        total / (1024 * 1024)
                    );
                    let _ = std::io::Write::flush(&mut std::io::stdout());
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
                if tx.send(chunk).await.is_err() {
                    return;
                }
            }
            if first {
                let _ = tx
                    .send(UploadFileChunk {
                        vm_id: vm_owned,
                        path: path_owned,
                        total_size: total,
                        data: Vec::new(),
                    })
                    .await;
            }
        });
        let response = self
            .inner
            .upload_file_stream(self.auth(tokio_stream::wrappers::ReceiverStream::new(rx)))
            .await
            .map_err(status_error)?
            .into_inner();
        if response.bytes_written != 0 && response.bytes_written != total {
            bail!(
                "upload wrote {} bytes, tar is {total}",
                response.bytes_written
            );
        }
        Ok(())
    }

    pub async fn exec(&mut self, vm: &str, command: &str) -> Result<()> {
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
            .exec(self.auth(outbound))
            .await
            .map_err(status_error)?
            .into_inner();
        let mut exit = 0i32;
        while let Some(chunk) = inbound.next().await {
            let chunk = chunk.map_err(status_error)?;
            if !chunk.data.is_empty() {
                let mut out = tokio::io::stdout();
                out.write_all(&chunk.data).await.ok();
                out.flush().await.ok();
            }
            if chunk.exit_code != 0 {
                exit = chunk.exit_code;
            }
        }
        if exit != 0 {
            bail!("remote command exited {exit}");
        }
        Ok(())
    }
}

pub fn load_token(api_key: Option<&str>) -> Result<String> {
    if let Some(key) = api_key {
        return exchange(key);
    }
    cli_token().context(
        "no boxd login found; run `boxd auth login` or `herdr-boxd config set api-key bxd_...`",
    )
}

fn exchange(api_key: &str) -> Result<String> {
    let body = serde_json::json!({ "api_key": api_key }).to_string();
    let output = std::process::Command::new("curl")
        .args([
            "-fsS",
            "-X",
            "POST",
            TOKEN_URL,
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
        ])
        .output()
        .context("token exchange failed")?;
    if !output.status.success() {
        bail!(
            "token exchange failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("token response is not JSON")?;
    parsed
        .get("token")
        .and_then(|value| value.as_str())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("token response has no token")
}

fn cli_token() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    for path in [
        Path::new(&home).join("Library/Application Support/boxd/credentials.json"),
        Path::new(&home).join(".config/boxd/credentials.json"),
    ] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(token) = json.get("token").and_then(|value| value.as_str()) {
            if !token.is_empty() {
                return Some(token.to_owned());
            }
        }
    }
    None
}

fn status_error(status: Status) -> anyhow::Error {
    let message = status.message();
    if message.is_empty() {
        anyhow::anyhow!("boxd gRPC {}", status.code())
    } else {
        anyhow::anyhow!("boxd: {message}")
    }
}
