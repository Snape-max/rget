use clap::{Arg, Command};
use reqwest::{Client, StatusCode};
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use indicatif::{ProgressBar, ProgressStyle, MultiProgress, ProgressState};
use std::path::PathBuf;
use std::env;
use url::Url;
use futures::future;
use colored::*;
use std::time::Duration;
use std::process;
use lazy_static::lazy_static;
use std::sync::Mutex;
use futures::StreamExt;

lazy_static! {
    static ref TEMP_FILES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
                .default_value("1")
                .value_parser(clap::value_parser!(u64)),
        )
        .get_matches();

    let url = matches.get_one::<String>("url").unwrap();
    let output = matches.get_one::<String>("output").map(PathBuf::from);
    let chunks: u64 = *matches.get_one::<u64>("chunks").unwrap();

    let client = Client::builder()
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .http2_keep_alive_interval(Some(Duration::from_secs(30)))
        .build()?;

    match check_download_capabilities(&client, url).await {
        Ok((supports_range, total_size)) => {
            if !supports_range || total_size == 0 || chunks == 1 {
                log_info("Using single-threaded download");
                let output_file = get_output_file(output.clone(), url);
                single_threaded_download(&client, url, output_file).await;
                return Ok(());
            }

            let chunk_size = (total_size + chunks - 1) / chunks;
            let output_file = get_output_file(output, url);
            let temp_dir = env::temp_dir();
            let multi_progress = MultiProgress::new();

            log_info(&format!("Downloading in {} chunks...", chunks));

            let mut futures = Vec::new();
            for i in 0..chunks {
                let temp_path = temp_dir.join(format!(
                    "{}.part{}", 
                    output_file.file_name().unwrap().to_string_lossy(), 
                    i
                ));
                register_temp_file(temp_path.clone());
                
                let pb = multi_progress.add(ProgressBar::new(chunk_size));
                setup_progress_bar(&pb, i);

                let chunk_future = download_chunk(
                    &client,
                    url,
                    i,
                    chunk_size,
                    total_size,
                    chunks,
                    temp_path,
                    pb.clone(),
                );
                futures.push(chunk_future);
            }

            future::join_all(futures).await;
            merge_chunks(&output_file, chunks, &temp_dir).await?;
            cleanup_temp_files();
            log_success(&format!("Download complete: {}", output_file.display()));
            Ok(())
        },
        Err(e) => {
            log_error(&e);
            process::exit(1);
        }
    }
}

async fn check_download_capabilities(client: &Client, url: &str) -> Result<(bool, u64), String> {
    match check_head_request(client, url).await {
        Ok(result) => return Ok(result),
        Err(e) => log_warning(&format!("HEAD check failed: {}", e)),
    }

    match test_range_support(client, url).await {
        Ok((size, supports)) => Ok((supports, size)),
        Err(e) => Err(format!("Failed to verify range support: {}", e)),
    }
}

async fn check_head_request(client: &Client, url: &str) -> Result<(bool, u64), String> {
    let response = client.head(url)
        .send()
        .await
        .map_err(|e| format!("HEAD request failed: {}", e))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Err("404 Resource not found".to_string());
    }

    if !response.status().is_success() {
        return Err(format!("Unexpected status: {}", response.status()));
    }

    let content_length = response.headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or("Missing Content-Length")?;

    let supports_range = response.headers()
        .get("Accept-Ranges")
        .map_or(false, |v| v == "bytes");

    Ok((supports_range, content_length))
}

async fn test_range_support(client: &Client, url: &str) -> Result<(u64, bool), String> {
    let response = client.get(url)
        .header("Range", "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("Range test failed: {}", e))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Err("404 Resource not found".to_string());
    }

    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Ok((0, false));
    }

    let content_range = response.headers()
        .get("Content-Range")
        .ok_or("Missing Content-Range header")?
        .to_str()
        .map_err(|_| "Invalid Content-Range header")?;

    parse_content_range(content_range)
        .map(|(_, _, total)| (total, true))
        .ok_or("Failed to parse Content-Range".to_string())
}

fn parse_content_range(header: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = header.split('/').collect();
    if parts.len() != 2 {
        return None;
    }

    let range_part = parts[0].split(' ').nth(1)?;
    let range: Vec<&str> = range_part.split('-').collect();
    if range.len() != 2 {
        return None;
    }

    Some((
        range[0].parse().ok()?,
        range[1].parse().ok()?,
        parts[1].parse().ok()?,
    ))
}

async fn single_threaded_download(client: &Client, url: &str, output: PathBuf) {
    let pb = ProgressBar::new_spinner();
    setup_spinner(pb.clone(), "Downloading...".to_string());

    let response = match client.get(url).send().await {
        Ok(res) if res.status().is_success() => res,
        Ok(res) => {
            log_error(&format!("Server returned: {}", res.status()));
            return;
        },
        Err(e) => {
            log_error(&format!("Connection failed: {}", e));
            return;
        }
    };

    let total_size = response.content_length().unwrap_or(0);
    pb.set_length(total_size);
    setup_progress_bar(&pb, 0);

    let mut file = match File::create(&output).await {
        Ok(f) => f,
        Err(e) => {
            log_error(&format!("Failed to create file: {}", e));
            return;
        }
    };
    
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if let Err(e) = file.write_all(&bytes).await {
                    log_error(&format!("Write failed: {}", e));
                    break;
                }
                downloaded += bytes.len() as u64;
                pb.set_position(downloaded);
            },
            Err(e) => {
                log_error(&format!("Download failed: {}", e));
                break;
            }
        }
    }

    pb.finish_with_message("done");
    log_success(&format!("Download complete: {}", output.display()));
}

async fn download_chunk(
    client: &Client,
    url: &str,
    chunk_index: u64,
    chunk_size: u64,
    total_size: u64,
    chunks: u64,
    temp_path: PathBuf,
    pb: ProgressBar,
) {
    let start = chunk_index * chunk_size;
    let end = if chunk_index == chunks - 1 {
        total_size - 1
    } else {
        start + chunk_size - 1
    };
    let range_header = format!("bytes={}-{}", start, end);

    let mut retry_count = 5;
    let mut backoff = 1;

    while retry_count > 0 {
        match client.get(url)
            .header("Range", range_header.clone())
            .send()
            .await
        {
            Ok(mut response) if response.status().is_success() => {
                let mut file = match File::create(&temp_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        log_error(&format!("Failed to create temp file: {}", e));
                        return;
                    }
                };
                
                let mut downloaded: u64 = 0;
                while let Ok(Some(chunk)) = response.chunk().await {
                    if let Err(e) = file.write_all(&chunk).await {
                        log_error(&format!("Write failed: {}", e));
                        break;
                    }
                    downloaded += chunk.len() as u64;
                    pb.set_position(downloaded);
                }
                pb.finish_with_message("done");
                return;
            },
            Ok(response) if response.status().is_server_error() => {
                log_error(&format!("Server error: {}", response.status()));
                retry_count -= 1;
            },
            Ok(response) => {
                log_error(&format!("Non-retryable status: {}", response.status()));
                break;
            },
            Err(e) if e.is_connect() || e.is_timeout() => {
                log_error(&format!("Connection error: {}", e));
                retry_count -= 1;
            },
            Err(e) => {
                log_error(&format!("Non-retryable error: {}", e));
                break;
            }
        }

        if retry_count > 0 {
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = std::cmp::min(backoff * 2, 30);
        } else {
            log_error(&format!("Failed to download chunk {}", chunk_index));
            pb.abandon_with_message("failed");
        }
    }
}

fn setup_progress_bar(pb: &ProgressBar, chunk_index: u64) {
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({eta}) ")
            .unwrap()
            .with_key("eta", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
            })
            .progress_chars("#>-"),
    );
    pb.set_message(format!("Chunk {}", chunk_index));
}

fn setup_spinner(pb: ProgressBar, message: String) {
    pb.set_style(
        ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}]  {msg}")
            .unwrap()
            .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
    );
    pb.set_message(message);
}

async fn merge_chunks(output_file: &PathBuf, chunks: u64, temp_dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    log_info("Merging chunks...");
    let mut output = File::create(output_file).await?;
    
    for i in 0..chunks {
        let temp_path = temp_dir.join(format!(
            "{}.part{}", 
            output_file.file_name().unwrap().to_string_lossy(), 
            i
        ));
        
        if !temp_path.exists() {
            log_error(&format!("Chunk file missing: {}", temp_path.display()));
            continue;
        }
        
        let mut chunk_file = File::open(&temp_path).await?;
        let mut buffer = Vec::new();
        chunk_file.read_to_end(&mut buffer).await?;
        output.write_all(&buffer).await?;
        fs::remove_file(&temp_path).await?;
        unregister_temp_file(temp_path);
    }
    
    Ok(())
}

fn get_output_file(output: Option<PathBuf>, url: &str) -> PathBuf {
    output.unwrap_or_else(|| {
        let url_parsed = Url::parse(url).unwrap();
        let path = url_parsed.path();
        let default_name = path.rsplit('/')
            .next()
            .filter(|s| !s.is_empty()) // 过滤掉空字符串
            .unwrap_or("download");     // 默认文件名
        
        let mut default_path = env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(default_name);
        
        // 防止覆盖现有文件
        let mut counter = 1;
        while default_path.exists() {
            default_path = default_path.with_file_name(
                format!("{}_{}", 
                    default_name.rsplit('.').next().unwrap_or("download"), 
                    counter
                )
            );
            counter += 1;
        }
        default_path
    })
}

fn register_temp_file(path: PathBuf) {
    TEMP_FILES.lock().unwrap().push(path);
}

fn unregister_temp_file(path: PathBuf) {
    TEMP_FILES.lock().unwrap().retain(|p| p != &path);
}

fn cleanup_temp_files() {
    let temp_files = TEMP_FILES.lock().unwrap();
    for path in temp_files.iter() {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                log_error(&format!("Failed to clean up {}: {}", path.display(), e));
            }
        }
    }
}

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