use clap::{Arg, Command};
use reqwest::Client;
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress, ProgressState};
use std::path::PathBuf;
use std::env;
use url::Url;
use futures::future;
use http::StatusCode;
use colored::*;
use std::fmt::Write;
use std::sync::Arc;

// 自定义结构体，用于管理临时文件的清理
struct TempFileGuard {
    path: PathBuf,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            println!("Cleaning up temporary file: {}", self.path.display());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[tokio::main]
async fn main() {
    let matches = Command::new("Rget")
        .version("1.0")
        .author("by Luke Mars")
        .about("A multi-threaded file downloader")
        .arg(
            Arg::new("url")
                .index(1)
                .value_name("URL")
                .help("The URL to download from")
                .required(true)
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("FILE")
                .help("The output file path")
                .required(false)
                .value_parser(clap::value_parser!(String)),
        )
        .arg(
            Arg::new("chunks")
                .short('c')
                .long("chunks")
                .value_name("CHUNKS")
                .help("Number of chunks to download in parallel")
                .default_value("4")
                .value_parser(clap::value_parser!(u64)),
        )
        .get_matches();

    let url = matches.get_one::<String>("url").unwrap();
    let output = matches.get_one::<String>("output").map(|s| PathBuf::from(s));
    let chunks: u64 = *matches.get_one::<u64>("chunks").unwrap();

    let client = Client::new();

    // Step 1: Try HEAD request to get Content-Length
    log_info(&format!("Starting download: {}", url));
    log_info("Resolving host...");
    let head_response = client.head(url).send().await;
    let mut total_size: u64 = 0;
    match head_response {
        Ok(resp) if resp.status() == StatusCode::OK => {
            if let Some(content_length) = resp.headers().get("Content-Length") {
                if let Ok(len_str) = content_length.to_str() {
                    if let Ok(len) = len_str.parse::<u64>() {
                        total_size = len;
                        log_success(&format!("File size: {} bytes", len));
                    } else {
                        log_error("Invalid Content-Length format.");
                        return;
                    }
                } else {
                    log_error("Failed to parse Content-Length.");
                    return;
                }
            } else {
                log_warning("No Content-Length in HEAD response, trying Range request...");
                // Fallback to Range request
            }
        },
        Ok(_) => {
            log_warning("No Content-Length in HEAD response, trying Range request...");
            // Fallback to Range request
        },
        Err(e) => {
            log_error(&format!("HEAD request failed: {}, trying Range request...", e));
            // Fallback to Range request
        },
    }

    // Proceed with chunked download if total_size is known
    if total_size == 0 {
        log_error("Unable to determine file size. Chunked download not possible.");
        return;
    }

    let chunk_size = (total_size + chunks - 1) / chunks;

    let output_file = if let Some(output_path) = output {
        output_path
    } else {
        let url_parsed = Url::parse(url).unwrap();
        let path = url_parsed.path();
        let default_file_name = path.rsplit('/').next().unwrap().to_string();
        env::current_dir().unwrap().join(&default_file_name)
    };

    // Use the system's temporary directory for storing chunk files
    let temp_dir = env::temp_dir();
    let multi_progress = MultiProgress::new();

    log_info(&format!("Downloading in {} chunks...", chunks));

    let mut futures = Vec::new();
    for i in 0..chunks {
        let temp_path = temp_dir.join(format!("{}.part{}", output_file.file_name().unwrap().to_string_lossy(), i));
        let pb = multi_progress.add(ProgressBar::new(chunk_size));
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Chunk {}", i));

        // 创建 TempFileGuard 来管理临时文件
        let temp_file_guard = Arc::new(TempFileGuard::new(temp_path.clone()));
        let client = client.clone();
        let url = url.to_string();
        let temp_file_guard_clone = Arc::clone(&temp_file_guard);

        let chunk_future = async move {
            if let Err(e) = download_chunk(&client, &url, i, chunk_size, total_size, chunks, temp_path.clone(), pb).await {
                log_error(&format!("Failed to download chunk {}: {}", i, e));
            }
            // 如果下载成功，手动忘记 TempFileGuard，避免删除临时文件
            Arc::into_inner(temp_file_guard_clone).unwrap();
        };
        futures.push(chunk_future);
    }

    future::join_all(futures).await;

    log_info("Merging chunks...");
    let mut output_file_handle = File::create(&output_file).await.unwrap();
    for i in 0..chunks {
        let temp_path = temp_dir.join(format!("{}.part{}", output_file.file_name().unwrap().to_string_lossy(), i));
        if !temp_path.exists() {
            log_error(&format!("Chunk file {}.part{} is missing, download failed.", output_file.display(), i));
            return;
        }
        let mut file = File::open(&temp_path).await.unwrap();
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).await.unwrap();
        output_file_handle.write_all(&buffer).await.unwrap();
        fs::remove_file(&temp_path).await.unwrap();
    }

    log_success(&format!("Download complete: {}", output_file.display()));
}

async fn download_chunk(client: &Client, url: &str, chunk_index: u64, chunk_size: u64, total_size: u64, chunks: u64, temp_path: PathBuf, pb: ProgressBar) -> Result<(), Box<dyn std::error::Error>> {
    let start = chunk_index * chunk_size;
    let end = if chunk_index == chunks - 1 {
        total_size - 1
    } else {
        start + chunk_size - 1
    };
    let range_header = format!("bytes={}-{}", start, end);

    let mut retry_count = 3;
    loop {
        match client.get(url).header("Range", range_header.clone()).send().await {
            Ok(mut response) => {
                if response.status().is_success() {
                    let mut file = File::create(&temp_path).await?;
                    let mut downloaded: u64 = 0;
                    while let Some(chunk) = response.chunk().await? {
                        file.write_all(&chunk).await?;
                        downloaded += chunk.len() as u64;
                        pb.set_position(downloaded);
                    }
                    pb.finish_with_message("done");
                    return Ok(());
                } else {
                    return Err(format!("Failed to download chunk {}: {}", chunk_index, response.status()).into());
                }
            },
            Err(_e) => {
                if retry_count == 0 {
                    return Err(format!("Failed to download chunk {}, giving up.", chunk_index).into());
                }
                retry_count -= 1;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

// Logging functions with colors
fn log_info(message: &str) {
    println!("{} {}", "[INFO]".bright_blue(), message);
}

fn log_success(message: &str) {
    println!("{} {}", "[SUCCESS]".bright_green(), message);
}

fn log_warning(message: &str) {
    println!("{} {}", "[WARNING]".bright_yellow(), message);
}

fn log_error(message: &str) {
    println!("{} {}", "[ERROR]".bright_red(), message);
}