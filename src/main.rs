use actix_web::web::{Bytes, Data};
use actix_web::{web, App, HttpResponse, HttpServer};
use anyhow::{Context, Result};
use rusoto_core::Region;
use rusoto_s3::{
    CompleteMultipartUploadRequest, CompletedPart, CreateMultipartUploadRequest, S3Client,
    UploadPartRequest, S3,
};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration as timeDuration, Instant};
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use tokio_util::bytes::Buf;

const BUCKET_NAME: &str = "your-s3-bucket-name";
const OBJECT_KEY: &str = "your-object-key";
const REPORT_INTERVAL_SECONDS: u64 = 1; // reduced for testing purposes
const CHUNK_SIZE: usize = 5 * 1024 * 1024; // 5 MB
const MAX_CHUNK_UPLOAD_RETRIES: usize = 3;

async fn report_progress(progress: Arc<Mutex<usize>>, total_parts: usize) {
    loop {
        time::sleep(Duration::from_secs(REPORT_INTERVAL_SECONDS)).await;
        let current_progress = *progress.lock().unwrap();
        println!(
            "{}% of the file uploaded.",
            (current_progress * 100) / total_parts
        );
    }
}

async fn create_multipart_upload(s3_client: &S3Client) -> Result<String> {
    let create_request = CreateMultipartUploadRequest {
        bucket: BUCKET_NAME.to_string(),
        key: OBJECT_KEY.to_string(),
        ..Default::default()
    };

    let result = s3_client.create_multipart_upload(create_request).await?;

    Ok(result.upload_id.unwrap_or_default())
}

async fn upload_part(
    s3_client: &S3Client,
    upload_id: &str,
    body: Bytes,
    part_number: i64,
) -> Result<()> {
    let mut stream_reader = body.reader();
    let mut buffer = Vec::new();
    stream_reader.read_to_end(&mut buffer)?;

    let upload_part_request = UploadPartRequest {
        bucket: BUCKET_NAME.to_string(),
        key: OBJECT_KEY.to_string(),
        part_number,
        upload_id: upload_id.to_string(),
        body: Some(buffer.into()),
        ..Default::default()
    };

    s3_client.upload_part(upload_part_request).await?;

    Ok(())
}

async fn complete_multipart_upload(
    s3_client: &S3Client,
    upload_id: &str,
    completed_parts: Vec<CompletedPart>,
) -> Result<()> {
    let complete_request = CompleteMultipartUploadRequest {
        bucket: BUCKET_NAME.to_string(),
        key: OBJECT_KEY.to_string(),
        upload_id: upload_id.to_string(),
        multipart_upload: Some(rusoto_s3::CompletedMultipartUpload {
            parts: Some(completed_parts),
        }),
        ..Default::default()
    };

    s3_client
        .complete_multipart_upload(complete_request)
        .await
        .context("Error completing multipart upload")?;

    Ok(())
}

async fn upload_to_s3_parallel(
    s3_client: Arc<S3Client>,
    body: Bytes,
    progress: Arc<Mutex<usize>>,
) -> Result<HttpResponse> {
    let time_started = Instant::now();
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let checksum = hasher.finalize();

    let total_parts = (body.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

    let chunks: Vec<Bytes> = body
        .chunks_exact(CHUNK_SIZE)
        .map(|c| Bytes::copy_from_slice(&Cow::Borrowed(c)))
        .collect();

    let upload_id = create_multipart_upload(&s3_client).await?;

    let (tx, mut rx) = mpsc::channel::<usize>(total_parts);

    let s3_client_clone = Arc::clone(&s3_client);

    for (i, chunk) in chunks.into_iter().enumerate() {
        let tx = tx.clone();
        let upload_id = upload_id.clone();
        let s3_client_clone = Arc::clone(&s3_client_clone);

        tokio::spawn(async move {
            let mut retries = 0;
            let mut success = false;

            while retries < MAX_CHUNK_UPLOAD_RETRIES && !success {
                let mut part_hasher = Sha256::new();
                part_hasher.update(&chunk);
                let part_checksum = part_hasher.finalize();

                if part_checksum != checksum {
                    eprintln!("Checksum mismatch for part {} of the file", i + 1);
                    break;
                }

                if let Err(e) =
                    upload_part(&s3_client_clone, &upload_id, chunk.clone(), (i + 1) as i64)
                        .await
                        .context(format!("Error during upload (retry {})", retries + 1))
                {
                    eprintln!("{}", e);
                    retries += 1;
                    time::sleep(Duration::from_secs(1)).await;
                } else {
                    success = true;
                }
            }

            if success {
                tx.send(i + 1).await.unwrap();
            }
        });
    }

    let mut uploaded_parts = 0;
    let progress_clone = Arc::clone(&progress);
    let progress_task = tokio::spawn(report_progress(progress_clone, total_parts));

    while uploaded_parts < total_parts {
        if let Some(_) = rx.recv().await {
            uploaded_parts += 1;
            *progress.lock().unwrap() = uploaded_parts;
        }
    }

    progress_task.await.unwrap();

    let completed_parts: Vec<rusoto_s3::CompletedPart> = (1..=total_parts)
        .map(|part_number| rusoto_s3::CompletedPart {
            e_tag: Some(part_number.to_string()),
            part_number: Some(part_number as i64),
        })
        .collect();

    complete_multipart_upload(&s3_client, &upload_id, completed_parts).await?;
    let time_elapsed: timeDuration = time_started.elapsed();
    Ok(HttpResponse::Ok().body(format!(
        "File uploaded successfully! and time taken: {:?} seconds",
        time_elapsed.as_secs()
    )))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let s3_client = Arc::new(S3Client::new(Region::UsEast1));
    let progress = Arc::new(Mutex::new(0usize));

    HttpServer::new(move || {
        let s3_client = Arc::clone(&s3_client);
        let progress = Arc::clone(&progress);

        App::new().app_data(Data::new(progress.clone())).route(
            "/upload",
            web::post().to(move |body| {
                let s3_client = Arc::clone(&s3_client);
                let progress = Arc::clone(&progress);

                async move {
                    match upload_to_s3_parallel(s3_client, body, progress).await {
                        Ok(_) => Ok::<HttpResponse, actix_web::Error>(
                            HttpResponse::Ok().body("File uploaded successfully!"),
                        ),
                        Err(e) => {
                            eprintln!("Error during upload: {}", e);
                            Ok(HttpResponse::InternalServerError().finish())
                        }
                    }
                }
            }),
        )
    })
    .bind("127.0.0.1:3030")?
    .run()
    .await
}
