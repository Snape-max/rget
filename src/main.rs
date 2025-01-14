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
use lazy_static::lazy_static;
use std::sync::Mutex;
use std::process;

lazy_static! {
    // 全局临时文件列表
    static ref TEMP_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
}

#[tokio::main]
async fn main() {
    // 注册一个 Ctrl+C 信号处理函数
    tokio::spawn(async {
        tokio::signal::ctrl_c().await.unwrap();
        log_info("Received interrupt signal, cleaning up...");
        cleanup_temp_files();
        process::exit(1);
    });

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
        register_temp_file(temp_path.clone()); // 注册临时文件
        let pb = multi_progress.add(ProgressBar::new(chunk_size));
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .with_key("eta", |state: &ProgressState, w: &mut dyn Write| write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap())
                .progress_chars("#>-"),
        );
        pb.set_message(format!("Chunk {}", i));
        let chunk_future = download_chunk(&client, url, i, chunk_size, total_size, chunks, temp_path, pb);
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
        unregister_temp_file(temp_path); // 取消注册已清理的临时文件
    }

    log_success(&format!("Download complete: {}", output_file.display()));
}

async fn download_chunk(client: &Client, url: &str, chunk_index: u64, chunk_size: u64, total_size: u64, chunks: u64, temp_path: PathBuf, pb: ProgressBar) {
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
                    let mut file = match File::create(&temp_path).await {
                        Ok(f) => f,
                        Err(e) => {
                            log_error(&format!("Failed to create file {}: {}", temp_path.display(), e));
                            break;
                        }
                    };
                    let mut downloaded: u64 = 0;
                    while let Some(chunk) = response.chunk().await.unwrap() {
                        if let Err(e) = file.write_all(&chunk).await {
                            log_error(&format!("Failed to write to file {}: {}", temp_path.display(), e));
                            break;
                        }
                        downloaded += chunk.len() as u64;
                        pb.set_position(downloaded);
                    }
                    pb.finish_with_message("done");
                    break;
                } else {
                    log_error(&format!("Failed to download chunk {}: {}", chunk_index, response.status()));
                }
            },
            Err(e) => {
                log_error(&format!("Connection failed: {}, retrying...", e));
                retry_count -= 1;
                if retry_count == 0 {
                    log_error(&format!("Failed to download chunk {}, giving up.", chunk_index));
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

// 注册临时文件
fn register_temp_file(path: PathBuf) {
    TEMP_FILES.lock().unwrap().push(path);
}

// 取消注册临时文件
fn unregister_temp_file(path: PathBuf) {
    TEMP_FILES.lock().unwrap().retain(|p| p != &path);
}

// 清理所有临时文件
fn cleanup_temp_files() {
    let temp_files = TEMP_FILES.lock().unwrap();
    for path in temp_files.iter() {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                log_error(&format!("Failed to clean up temp file {}: {}", path.display(), e));
            } else {
                log_info(&format!("Cleaned up temp file: {}", path.display()));
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