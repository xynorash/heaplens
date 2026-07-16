use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::mpsc;
use tracing::{info, warn};

use heaplens_protocol::{Frame, FrameDecoder};

use crate::msg::GraphMsg;

pub async fn run(pipe_name: String, tx: mpsc::UnboundedSender<GraphMsg>) {
    let mut first = true;
    loop {
        // Create the pipe server. first_pipe_instance only on the first creation.
        let mut server = {
            let mut opts = ServerOptions::new();
            if first {
                opts.first_pipe_instance(true);
            }
            match opts.create(&pipe_name) {
                Ok(s) => {
                    first = false;
                    s
                }
                Err(e) => {
                    warn!("pipe create failed: {e}");
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    continue;
                }
            }
        };

        info!("waiting for client on {pipe_name}");
        if server.connect().await.is_err() {
            warn!("pipe connect failed, retrying");
            continue;
        }
        info!("client connected");

        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 4096];
        let mut handshook_pid: Option<u64> = None;

        loop {
            match server.read(&mut buf).await {
                Ok(0) => {
                    info!("client disconnected");
                    if let Some(pid) = handshook_pid {
                        let _ = tx.send(GraphMsg::TargetDisconnected { pid });
                    }
                    break;
                }
                Ok(n) => {
                    decoder.push(&buf[..n]);
                    for frame in decoder.by_ref() {
                        match frame {
                            Frame::Events(events) => {
                                let _ = tx.send(GraphMsg::Events(events));
                            }
                            Frame::Symbols(syms) => {
                                let _ = tx.send(GraphMsg::Symbols(syms));
                            }
                            Frame::Handshake { pid, name } => {
                                handshook_pid = Some(pid);
                                let _ = tx.send(GraphMsg::TargetConnected { pid, name });
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("pipe read error: {e}");
                    if let Some(pid) = handshook_pid {
                        let _ = tx.send(GraphMsg::TargetDisconnected { pid });
                    }
                    break;
                }
            }
        }
        // Fall through to re-create server for the next client.
    }
}
