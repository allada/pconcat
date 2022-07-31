// Copyright 2022 Nathan (Blaise) Bruer.  All rights reserved.

use std::fs::OpenOptions;
use std::io::{stdin, stdout, BufRead, BufReader};
use std::io::{IoSlice, Write};
use std::os::unix::prelude::AsRawFd;
use std::sync::mpsc::{sync_channel, Receiver};
use std::thread::{spawn, JoinHandle};

use aws_sdk_s3::model::RequestPayer;
use async_std::task;
use clap::Parser;
use futures::future::ready;
use futures::{stream, StreamExt};
use bytes::Bytes;
use nix::fcntl::{vmsplice, SpliceFFlags};

const DEFAULT_CONCURRENT_LIMIT: usize = 16;
const DEFAULT_BUFFER_SIZE: usize = 1 << 30; // 1Gb

const CHUNK_BUFFER_SIZE: usize = 2 * 1024 * 1024; // 2mb

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Number of commands to run in parallel
    #[clap(short, long, value_parser, default_value_t = DEFAULT_CONCURRENT_LIMIT)]
    parallel_count: usize,

    /// Size in bytes of the stdout buffer for reach command
    #[clap(short, long, value_parser, default_value_t = DEFAULT_BUFFER_SIZE)]
    buffer_size: usize,

    /// Path to write file. Prints to stdout if not set. Using a file can be faster than stdout
    #[clap(value_parser)]
    output_file: Option<String>,
}

fn collect_and_write_data<S, F>(mut process_stream: S, mut write_fn: F)
where
    S: futures::Stream<Item = (Receiver<Bytes>, JoinHandle<()>)> + std::marker::Unpin,
    F: FnMut(Bytes),
{
    task::block_on(async {
        while let Some((rx, thread_join)) = process_stream.next().await {
            while let Ok(chunk) = rx.recv() {
                (write_fn)(chunk);
            }
            thread_join.join().unwrap();
        }
    })
}

fn main() {
    let args = Args::parse();

    let s3_client = task::block_on(async {
        let shared_config = aws_config::load_from_env().await;
        aws_sdk_s3::Client::new(&shared_config)
    });

    let stdin = stdin().lock();
    let stdin_buf = BufReader::new(stdin);
    let process_stream = stream::iter(stdin_buf.lines())
        .map(move |maybe_s3_path| {
            let s3_path = maybe_s3_path.unwrap();

            let (bucket, key) = s3_path.split_once('/').expect("Bucket was not present in s3 path");

            let get_object_builder = s3_client
                .get_object()
                .bucket(bucket)
                .key(key)
                .request_payer(RequestPayer::Requester);

            let (tx, rx) = sync_channel(args.buffer_size / CHUNK_BUFFER_SIZE);

            let thread_join = spawn(move || {
                // let mut stdout = child_process.stdout.take().unwrap();
                task::block_on(async move {
                    use tokio_stream::StreamExt;

                    let mut response = get_object_builder.send().await.unwrap();
                    while let Some(bytes) = response.body.try_next().await.unwrap() {
                        tx.send(bytes).unwrap();
                    }
                });
                // loop {
                //     let mut buffer = Vec::with_capacity(CHUNK_BUFFER_SIZE);
                //     unsafe {
                //         buffer.set_len(CHUNK_BUFFER_SIZE);
                //     }
                //     let mut bytes_read = 0;
                //     while bytes_read <= CHUNK_BUFFER_SIZE {
                //         let len = stdout
                //             .read(&mut buffer[bytes_read..CHUNK_BUFFER_SIZE])
                //             .unwrap();
                //         if len == 0 {
                //             break;
                //         }
                //         bytes_read += len;
                //     }
                //     buffer.truncate(bytes_read);
                //     if bytes_read == 0 {
                //         let exit_code = child_process.wait().unwrap();
                //         assert!(exit_code.success());
                //         break;
                //     }
                //     tx.send(buffer).unwrap();
                // }
            });
            ready((rx, thread_join))
        })
        .buffered(args.parallel_count);

    if let Some(output_file) = args.output_file {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(output_file)
            .unwrap();
        collect_and_write_data(process_stream, move |chunk| file.write_all(&chunk).unwrap());
    } else {
        let stdout = stdout().lock();
        // This is an optimization to make writing to a pipe significantly faster.
        collect_and_write_data(process_stream, move |chunk| {
            let chunk_size = chunk.len();
            let mut bytes_written = 0;
            while bytes_written < chunk_size {
                let iov = [IoSlice::new(&chunk[bytes_written..])];
                match vmsplice(stdout.as_raw_fd(), &iov, SpliceFFlags::SPLICE_F_GIFT) {
                    Ok(sz) => bytes_written += sz,
                    Err(e) => panic!("{}", e),
                }
            }
        });
    }
}
