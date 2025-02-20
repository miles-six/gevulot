use grpc::vm_service_client::VmServiceClient;
use grpc::{FileChunk, FileData, FileMetadata};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use std::{path::Path, thread::sleep, time::Duration};
use tokio::io::AsyncReadExt;
use tokio::runtime::Runtime;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_stream::StreamExt;
use tokio_vsock::VsockStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::Request;
use tower::service_fn;
use vsock::VMADDR_CID_HOST;

mod grpc {
    tonic::include_proto!("vm_service");
}

/// DATA_STREAM_CHUNK_SIZE controls the chunk size for streaming byte
/// transfers, e.g. when transferring the result file back to node.
const DATA_STREAM_CHUNK_SIZE: usize = 4096;

/// MOUNT_TIMEOUT is maximum amount of time to wait for workspace mount to be
/// present in /proc/mounts.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(30);

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub type TaskId = String;

#[derive(Debug)]
pub struct Task {
    pub id: TaskId,
    pub args: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Debug)]
pub struct TaskResult {
    id: TaskId,
    data: Vec<u8>,
    files: Vec<String>,
}

impl Task {
    pub fn result(&self, data: Vec<u8>, files: Vec<String>) -> Result<TaskResult> {
        Ok(TaskResult {
            id: self.id.clone(),
            data,
            files,
        })
    }

    pub fn get_task_files_path<'a>(&'a self, workspace: &str) -> Vec<(&'a str, PathBuf)> {
        self.files
            .iter()
            .map(|name| {
                let path = Path::new(workspace).join(&self.id).join(name);
                (name.as_str(), path)
            })
            .collect()
    }
}

struct GRPCClient {
    /// `workspace` is the file directory used for task specific file downloads.
    workspace: String,

    client: Mutex<VmServiceClient<Channel>>,
    rt: Runtime,
}

impl GRPCClient {
    fn new(port: u32, workspace: &str) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let client = rt.block_on(GRPCClient::connect(port))?;

        println!("waiting for {workspace} mount to be present");
        let beginning = Instant::now();
        loop {
            if beginning.elapsed() > MOUNT_TIMEOUT {
                panic!("{} mount timeout", workspace);
            }

            if mount_present(workspace)? {
                println!("{workspace} mount is now present");
                break;
            }

            sleep(Duration::from_secs(1));
        }

        Ok(GRPCClient {
            workspace: workspace.to_string(),
            client: Mutex::new(client),
            rt,
        })
    }

    async fn connect(port: u32) -> Result<VmServiceClient<Channel>> {
        let channel = Endpoint::try_from(format!("http://[::]:{}", port))?
            .connect_with_connector(service_fn(move |_: Uri| {
                // Connect to a VSOCK server
                VsockStream::connect(VMADDR_CID_HOST, port)
            }))
            .await?;

        Ok(grpc::vm_service_client::VmServiceClient::new(channel))
    }

    fn get_task(&mut self) -> Result<Option<Task>> {
        let task_response = self
            .rt
            .block_on(async {
                self.client
                    .lock()
                    .await
                    .get_task(grpc::TaskRequest {})
                    .await
            })?
            .into_inner();

        let mut task = match task_response.result {
            Some(grpc::task_response::Result::Task(task)) => Task {
                id: task.id,
                args: task.args,
                files: task.files,
            },
            Some(grpc::task_response::Result::Error(code)) => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("get_task() resulted with error code {}", code),
                )));
            }
            None => return Ok(None),
        };

        let mut files = vec![];
        for file in task.files {
            let path = match self.rt.block_on(self.download_file(task.id.clone(), &file)) {
                Ok(path) => path,
                Err(err) => {
                    println!("failed to download file {file} for task {}: {err}", task.id);
                    return Err(err);
                }
            };
            files.push(path);
        }
        task.files = files;

        Ok(Some(task))
    }

    fn submit_file(&mut self, task_id: TaskId, file_path: String) -> Result<[u8; 32]> {
        let hasher = Arc::new(Mutex::new(blake3::Hasher::new()));
        self.rt.block_on(async {
            let stream_hasher = Arc::clone(&hasher);
            let outbound = async_stream::stream! {

                let fd = match tokio::fs::File::open(&file_path).await {
                    Ok(fd) => fd,
                    Err(err) => {
                        println!("failed to open file {file_path}: {}", err);
                        return;
                    }
                };

                let mut file = tokio::io::BufReader::new(fd);

                let metadata = FileData{ result: Some(grpc::file_data::Result::Metadata(FileMetadata {
                    task_id,
                    path: file_path,
                }))};

                yield metadata;

                let mut buf: [u8; DATA_STREAM_CHUNK_SIZE] = [0; DATA_STREAM_CHUNK_SIZE];
                loop {
                    match file.read(&mut buf).await {
                        Ok(0) => return,
                        Ok(n) => {

                            stream_hasher.lock().await.update(&buf[..n]);
                            yield FileData{ result: Some(grpc::file_data::Result::Chunk(FileChunk{ data: buf[..n].to_vec() }))};
                        },
                        Err(err) => {
                            println!("failed to read file: {}", err);
                            yield FileData{ result: Some(grpc::file_data::Result::Error(1))};
                        }
                    }
                }
            };

            if let Err(err) = self.client.lock().await.submit_file(Request::new(outbound)).await {
                println!("failed to submit file: {}", err);
            }
        });
        let hasher = Arc::into_inner(hasher).unwrap().into_inner();
        let hash = hasher.finalize().into();

        Ok(hash)
    }

    fn submit_result(&mut self, result: &TaskResult) -> Result<bool> {
        let files = result
            .files
            .iter()
            .map(|file| {
                self.submit_file(result.id.clone(), file.clone())
                    .map(|checksum| crate::grpc::File {
                        path: file.to_string(),
                        checksum: checksum.to_vec(),
                    })
            })
            .collect::<Result<Vec<_>>>()?;

        let task_result_req = grpc::TaskResultRequest {
            result: Some(grpc::task_result_request::Result::Task(grpc::TaskResult {
                id: result.id.clone(),
                data: result.data.clone(),
                files,
            })),
        };

        let response = self
            .rt
            .block_on(async {
                self.client
                    .lock()
                    .await
                    .submit_result(task_result_req)
                    .await
            })?
            .into_inner();

        Ok(response.r#continue)
    }

    /// download_file asks gRPC server for file with a `name` and writes it to
    /// `workspace`.
    async fn download_file(&self, task_id: TaskId, name: &str) -> Result<String> {
        let file_req = grpc::GetFileRequest {
            task_id: task_id.clone(),
            path: name.to_string(),
        };

        let file_path = Path::new(&self.workspace).join(name);
        if let Some(parent) = file_path.parent() {
            if let Ok(false) = tokio::fs::try_exists(parent).await {
                if let Err(err) = tokio::fs::create_dir_all(parent).await {
                    println!(
                        "failed to create directory: {}: {err}",
                        parent.to_str().unwrap()
                    );
                    return Err(Box::new(err));
                };
            }
        }
        let path = match file_path.into_os_string().into_string() {
            Ok(path) => path,
            Err(e) => panic!("failed to construct path for a file to write: {:?}", e),
        };

        // Ensure any necessary subdirectories exists.
        if let Some(parent) = Path::new(&path).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .expect("task file mkdir");
        }

        let mut stream = self
            .client
            .lock()
            .await
            .get_file(file_req)
            .await?
            .into_inner();

        let out_file = tokio::fs::File::create(path.clone()).await?;
        let mut writer = tokio::io::BufWriter::new(out_file);

        let mut total_bytes = 0;

        while let Some(Ok(grpc::FileData { result: resp })) = stream.next().await {
            match resp {
                Some(grpc::file_data::Result::Metadata(..)) => {
                    // Ignore metadata as we already know it.
                    continue;
                }
                Some(grpc::file_data::Result::Chunk(file_chunk)) => {
                    total_bytes += file_chunk.data.len();
                    writer.write_all(file_chunk.data.as_ref()).await?;
                }
                Some(grpc::file_data::Result::Error(err)) => {
                    panic!("error while fetching file {}: {}", name, err)
                }
                None => {
                    println!("stream broken");
                    break;
                }
            }
        }
        writer.flush().await?;

        println!("downloaded {} bytes for {}", &total_bytes, &name);

        Ok(path)
    }
}

fn mount_present(mount_point: &str) -> Result<bool> {
    let file = File::open("/proc/mounts")?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        let line = line.expect("read /proc/mounts");
        if line.contains(mount_point) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// run function takes `callback` that is invoked with executable `Task` and
/// which is expected to return `TaskResult`.
pub fn run(callback: impl Fn(&Task) -> Result<TaskResult>) -> Result<()> {
    let mut client = GRPCClient::new(8080, "/workspace")?;

    loop {
        let task = match client.get_task() {
            Ok(Some(task)) => task,
            Ok(None) => {
                sleep(Duration::from_secs(1));
                continue;
            }
            Err(err) => {
                println!("get_task(): {}", err);
                return Err(err);
            }
        };

        let result = callback(&task)?;

        let should_continue = client.submit_result(&result)?;
        if !should_continue {
            break;
        }
    }

    Ok(())
}
