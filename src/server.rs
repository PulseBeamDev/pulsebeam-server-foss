use crate::proto::signaling_server::Signaling;
use crate::proto::{self, PeerInfo};
use std::pin::Pin;
use std::time::Duration;
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::field::valuable;

use crate::manager::{Manager, ManagerConfig, PeerConn};
const RESERVED_CONN_ID_DISCOVERY: u32 = 0;
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(45);

#[derive(Clone)]
pub struct Server {
    manager: Manager,
}

impl Default for Server {
    fn default() -> Self {
        Self::new(ManagerConfig {
            max_groups: 65536,
            max_peers_per_group: 16,
        })
    }
}

impl Server {
    pub fn new(cfg: ManagerConfig) -> Self {
        let manager = Manager::new(cfg);
        Self { manager }
    }

    pub fn recv_stream(&self, src: PeerInfo) -> RecvStream {
        let group = self.manager.get(src.group_id);
        let conn = PeerConn {
            peer_id: src.peer_id,
            conn_id: src.conn_id,
        };
        let peer = group.get(conn);

        let repeat = std::iter::repeat(Ok(proto::RecvResp {
            msg: Some(proto::Message {
                header: None,
                payload: Some(proto::MessagePayload {
                    payload_type: Some(proto::message_payload::PayloadType::Ping(proto::Ping {})),
                }),
            }),
        }));
        let keep_alive = tokio_stream::iter(repeat).throttle(KEEP_ALIVE_INTERVAL);
        let payload_stream = peer.mailbox.1.into_stream().map(|msg| {
            tracing::trace!(msg = valuable(&msg), "payload stream");
            Ok(proto::RecvResp { msg: Some(msg) })
        });
        let merged = keep_alive.merge(payload_stream);
        Box::pin(merged) as RecvStream
    }
}

pub type RecvStream = Pin<Box<dyn Stream<Item = Result<proto::RecvResp, tonic::Status>> + Send>>;

#[tonic::async_trait]
impl Signaling for Server {
    async fn prepare(
        &self,
        _req: tonic::Request<proto::PrepareReq>,
    ) -> Result<tonic::Response<proto::PrepareResp>, tonic::Status> {
        // WARNING: PLEASE READ THIS FIRST!
        // By default, OSS/self-hosting only provides a public STUN server.
        // You must provide your own TURN and STUN services.
        // TURN is required in some network condition.
        // Public TURN and STUN services are UNRELIABLE.
        Ok(tonic::Response::new(proto::PrepareResp {
            ice_servers: vec![proto::IceServer {
                urls: vec![String::from("stun:stun.l.google.com:19302")],
                username: None,
                credential: None,
            }],
        }))
    }

    async fn send(
        &self,
        req: tonic::Request<proto::SendReq>,
    ) -> Result<tonic::Response<proto::SendResp>, tonic::Status> {
        let mut msg = req
            .into_inner()
            .msg
            .ok_or(tonic::Status::invalid_argument("msg is required"))?;
        tracing::trace!(msg = valuable(&msg), "send");
        let hdr = msg
            .header
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("header is required"))?;
        let dst = hdr
            .dst
            .as_mut()
            .ok_or(tonic::Status::invalid_argument("dst is required"))?;

        let cloned_dst = dst.clone();
        let group = self.manager.get(cloned_dst.group_id);

        // TODO: use a different RPC for connecting?
        let peer = if dst.conn_id == RESERVED_CONN_ID_DISCOVERY {
            tracing::trace!(
                conn_id = dst.conn_id,
                group_id = dst.group_id,
                peer_id = cloned_dst.peer_id,
                "electing"
            );
            let selected = group
                .select_one(cloned_dst.peer_id)
                .ok_or(tonic::Status::out_of_range("peer id not present"))?;
            dst.conn_id = selected.0.conn_id;
            tracing::trace!(dst = valuable(&dst), "select_one found");
            selected.1
        } else {
            let conn = PeerConn {
                peer_id: cloned_dst.peer_id,
                conn_id: dst.conn_id,
            };
            group.get(conn)
        };
        peer.mailbox
            .0
            .send_async(msg)
            .await
            .map_err(|err| tonic::Status::aborted(err.to_string()))?;

        Ok(tonic::Response::new(proto::SendResp {}))
    }

    type RecvStream = RecvStream;
    async fn recv(
        &self,
        req: tonic::Request<proto::RecvReq>,
    ) -> std::result::Result<tonic::Response<Self::RecvStream>, tonic::Status> {
        let src = req
            .into_inner()
            .src
            .ok_or(tonic::Status::invalid_argument("src is required"))?;

        tracing::trace!(src = valuable(&src), "recv");
        let token = CancellationToken::new();
        let _guard = token.clone().drop_guard();

        let manager = self.manager.clone();
        let peer = src.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            tracing::info!(
                peer = valuable(&peer),
                "detected connection dropped, removing peer"
            );
            manager.remove(peer);
        });
        let payload = self.recv_stream(src);
        Ok(tonic::Response::new(payload))
    }
}

#[cfg(test)]
mod test {
    use std::iter::zip;

    use super::*;
    use proto::*;

    fn dummy_msg(src: PeerInfo, dst: PeerInfo, seqnum: u32) -> Message {
        Message {
            header: Some(MessageHeader {
                src: Some(src),
                dst: Some(dst),
                seqnum,
                reliable: true,
            }),
            payload: Some(MessagePayload { payload_type: None }),
        }
    }

    fn assert_msgs(received: &[Message], sent: &[Message]) {
        assert_eq!(received.len(), sent.len());
        let mut received = received.to_vec();
        received.sort_by_key(|m| m.header.as_ref().unwrap().seqnum);
        let mut sent = sent.to_vec();
        sent.sort_by_key(|m| m.header.as_ref().unwrap().seqnum);
        let pairs = zip(received, sent);
        for (a, b) in pairs.into_iter() {
            assert_eq!(a, b);
        }
    }

    async fn stream_to_vec(received: RecvStream, take: usize) -> Vec<Message> {
        received
            .filter_map(|r| r.ok())
            .filter_map(|r| r.msg)
            .filter(|m| m.header.is_some())
            .take(take)
            .collect()
            .await
    }

    fn setup() -> (Server, PeerInfo, PeerInfo) {
        let s = Server::default();
        let peer1 = PeerInfo {
            group_id: String::from("default"),
            peer_id: String::from("peer1"),
            conn_id: 32,
        };
        let peer2 = PeerInfo {
            group_id: peer1.group_id.clone(),
            peer_id: String::from("peer2"),
            conn_id: 64,
        };
        (s, peer1, peer2)
    }

    #[tokio::test]
    async fn recv_normal_single() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];
        let (send, recv) = tokio::join!(
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[0].clone()),
            })),
            stream_to_vec(s.recv_stream(peer2), 1),
        );

        send.unwrap();
        assert_msgs(&recv, &msgs);
    }

    #[tokio::test]
    async fn recv_normal_many() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![
            dummy_msg(peer1.clone(), peer2.clone(), 0),
            dummy_msg(peer1.clone(), peer2.clone(), 1),
        ];
        let (send1, send2, recv) = tokio::join!(
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[0].clone()),
            })),
            s.send(tonic::Request::new(SendReq {
                msg: Some(msgs[1].clone()),
            })),
            stream_to_vec(s.recv_stream(peer2), 2),
        );

        send1.unwrap();
        send2.unwrap();
        assert_msgs(&recv, &msgs);
    }

    #[tokio::test]
    async fn recv_first_then_send() {
        let (s, peer1, peer2) = setup();
        let msgs = vec![dummy_msg(peer1.clone(), peer2.clone(), 0)];

        let cloned_s = s.clone();
        let join = tokio::spawn(async move {
            let recv_stream = cloned_s.recv_stream(peer2.clone());
            stream_to_vec(recv_stream, 1).await
        });

        // let recv runs first since tokio test starts with single thread by default
        tokio::task::yield_now().await;
        s.send(tonic::Request::new(SendReq {
            msg: Some(msgs[0].clone()),
        }))
        .await
        .unwrap();

        let resp = join.await.unwrap();
        assert_msgs(&resp, &msgs);
    }

    #[tokio::test]
    async fn query_peers() {
        let (s, peer1, peer2) = setup();
        let group = s.manager.get(peer1.group_id.clone());
        let results = group.collect();
        assert_eq!(results.len(), 0);

        let _stream1 = s.recv_stream(peer1.clone());
        let _stream2 = s.recv_stream(peer2.clone());
        let mut results = group.collect();
        println!("{:?}", results);
        assert_eq!(results.len(), 2);
        results.sort();
        assert_eq!(results[0].peer_id, peer1.peer_id);
        assert_eq!(results[0].conn_id, peer1.conn_id);
        assert_eq!(results[1].peer_id, peer2.peer_id);
        assert_eq!(results[1].conn_id, peer2.conn_id);
    }
}
