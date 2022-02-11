// Copyright (c) Facebook, Inc. and its affiliates.
// SPDX-License-Identifier: Apache-2.0

use crate::transport::*;
use bytes::Bytes;
use futures::future::FutureExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use sui_types::{error::*, serialize::*};
use tracing::*;

use std::io;
use tokio::time;

#[derive(Clone)]
pub struct NetworkClient {
    base_address: String,
    base_port: u16,
    buffer_size: usize,
    send_timeout: std::time::Duration,
    recv_timeout: std::time::Duration,
}

impl NetworkClient {
    pub fn new(
        base_address: String,
        base_port: u16,
        buffer_size: usize,
        send_timeout: std::time::Duration,
        recv_timeout: std::time::Duration,
    ) -> Self {
        NetworkClient {
            base_address,
            base_port,
            buffer_size,
            send_timeout,
            recv_timeout,
        }
    }

    async fn send_recv_bytes_internal(&self, buf: Vec<u8>) -> Result<Vec<u8>, io::Error> {
        let address = format!("{}:{}", self.base_address, self.base_port);
        let mut stream = connect(address, self.buffer_size).await?;
        // Send message
        time::timeout(self.send_timeout, stream.write_data(&buf)).await??;
        // Wait for reply
        time::timeout(self.recv_timeout, stream.read_data()).await?
    }

    pub async fn send_recv_bytes(&self, buf: Vec<u8>) -> Result<SerializedMessage, SuiError> {
        match self.send_recv_bytes_internal(buf).await {
            Err(error) => Err(SuiError::ClientIoError {
                error: format!("{}", error),
            }),
            Ok(response) => {
                // Parse reply
                match deserialize_message(&response[..]) {
                    Ok(SerializedMessage::Error(error)) => Err(*error),
                    Ok(message) => Ok(message),
                    Err(_) => Err(SuiError::InvalidDecoding),
                    // _ => Err(SuiError::UnexpectedMessage),
                }
            }
        }
    }

    async fn batch_send_one_chunk(
        &self,
        requests: Vec<Bytes>,
        max_in_flight: u64,
    ) -> Result<Vec<Bytes>, io::Error> {
        let address = format!("{}:{}", self.base_address, self.base_port);
        let mut stream = connect(address, self.buffer_size).await?;
        let mut requests = requests.iter();
        let mut in_flight: u64 = 0;
        let mut responses = Vec::new();

        loop {
            while in_flight < max_in_flight {
                let request = match requests.next() {
                    None => {
                        if in_flight == 0 {
                            return Ok(responses);
                        }
                        // No more entries to send.
                        break;
                    }
                    Some(request) => request,
                };
                let status = time::timeout(self.send_timeout, stream.write_data(request)).await;
                if let Err(error) = status {
                    error!("Failed to send request: {}", error);
                    continue;
                }
                in_flight += 1;
            }
            if requests.len() % 5000 == 0 && requests.len() > 0 {
                info!("In flight {} Remaining {}", in_flight, requests.len());
            }
            match time::timeout(self.recv_timeout, stream.read_data()).await {
                Ok(Ok(buffer)) => {
                    in_flight -= 1;
                    responses.push(Bytes::from(buffer));
                }
                Ok(Err(error)) => {
                    if error.kind() == io::ErrorKind::UnexpectedEof {
                        info!("Socket closed by server");
                        return Ok(responses);
                    }
                    error!("Received error response: {}", error);
                }
                Err(error) => {
                    error!(
                        "Timeout while receiving response: {} (in flight: {})",
                        error, in_flight
                    );
                }
            }
        }
    }

    pub fn batch_send<I>(
        &self,
        requests: I,
        connections: usize,
        max_in_flight: u64,
    ) -> impl futures::stream::Stream<Item = Vec<Bytes>>
    where
        I: IntoIterator<Item = Bytes>,
    {
        let handles = futures::stream::FuturesUnordered::new();

        let outer_requests: Vec<_> = requests.into_iter().collect();
        let size = outer_requests.len() / connections;
        for chunk in outer_requests[..].chunks(size) {
            let requests: Vec<_> = chunk.to_vec();
            let client = self.clone();
            handles.push(
                tokio::spawn(async move {
                    info!(
                        "Sending TCP requests to {}:{}",
                        client.base_address, client.base_port,
                    );
                    let responses = client
                        .batch_send_one_chunk(requests, max_in_flight)
                        .await
                        .unwrap_or_else(|_| Vec::new());
                    info!(
                        "Done sending TCP requests to {}:{}",
                        client.base_address, client.base_port,
                    );
                    responses
                })
                .then(|x| async { x.unwrap_or_else(|_| Vec::new()) }),
            );
        }

        handles
    }
}

pub struct NetworkServer {
    pub base_address: String,
    pub base_port: u16,
    pub buffer_size: usize,
    // Stats
    packets_processed: AtomicUsize,
    user_errors: AtomicUsize,
}

impl NetworkServer {
    pub fn new(base_address: String, base_port: u16, buffer_size: usize) -> Self {
        Self {
            base_address,
            base_port,
            buffer_size,
            packets_processed: AtomicUsize::new(0),
            user_errors: AtomicUsize::new(0),
        }
    }

    pub fn packets_processed(&self) -> usize {
        self.packets_processed.load(Ordering::Relaxed)
    }

    pub fn increment_packets_processed(&self) {
        self.packets_processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn user_errors(&self) -> usize {
        self.user_errors.load(Ordering::Relaxed)
    }

    pub fn increment_user_errors(&self) {
        self.user_errors.fetch_add(1, Ordering::Relaxed);
    }
}
