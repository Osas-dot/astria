/// Gossip network for receiving new block headers from the sequencer.
/// Used for "soft commitments".
use color_eyre::eyre::{eyre, Result, WrapErr};
use futures::{stream::FusedStream, StreamExt};
use libp2p::{
    core::upgrade::Version,
    gossipsub::{self, Message, MessageId, TopicHash},
    identity, mdns, noise, ping,
    swarm::{NetworkBehaviour, Swarm, SwarmBuilder, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Transport,
};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tracing::info;

const HEADER_TOPIC: &str = "header";

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    ping: ping::Behaviour,
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::tokio::Behaviour,
}

pub struct Network {
    pub multiaddr: Multiaddr,
    swarm: Swarm<MyBehaviour>,
    terminated: bool,
}

impl Network {
    pub fn new(bootnode: Option<&str>, port: u16) -> Result<Self> {
        // TODO: store this on disk instead of randomly generating
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!("local peer id: {local_peer_id:?}");

        let transport = tcp::tokio::Transport::default()
            .upgrade(Version::V1Lazy)
            .authenticate(noise::NoiseAuthenticated::xx(&local_key)?)
            .multiplex(yamux::YamuxConfig::default())
            .boxed();

        // content-address message by using the hash of it as an ID
        let message_id_fn = |message: &gossipsub::Message| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            gossipsub::MessageId::from(s.finish().to_string())
        };

        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1))
            .validation_mode(gossipsub::ValidationMode::Strict) // the default is Strict (enforce message signing)
            .message_id_fn(message_id_fn) // content-address messages so that duplicates aren't propagated
            .build()
            .map_err(|e| eyre!("failed to build gossipsub config: {}", e))?;

        // build a gossipsub network behaviour
        let mut gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(local_key),
            gossipsub_config,
        )
        .map_err(|e| eyre!("failed to create gossipsub behaviour: {}", e))?;

        let topic = gossipsub::IdentTopic::new(HEADER_TOPIC);
        gossipsub.subscribe(&topic)?;

        let mut swarm = {
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?;
            let behaviour = MyBehaviour {
                gossipsub,
                mdns,
                ping: ping::Behaviour::default(),
            };
            SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build()
        };

        let listen_addr = format!("/ip4/0.0.0.0/tcp/{}", port);
        swarm.listen_on(listen_addr.parse()?)?;

        println!("bootnode: {:?}", bootnode);
        if let Some(addr) = bootnode {
            println!("dialing {:?}", addr);
            let remote: Multiaddr = addr.parse()?;
            swarm.dial(remote)?;
            info!("dialed {addr}")
        }

        let multiaddr = Multiaddr::from_str(&format!("{}/p2p/{}", listen_addr, local_peer_id))?;
        Ok(Network {
            multiaddr,
            swarm,
            terminated: false,
        })
    }

    pub async fn publish(&mut self, message: Vec<u8>) -> Result<MessageId> {
        let topic = gossipsub::IdentTopic::new(HEADER_TOPIC);
        self.swarm
            .behaviour_mut()
            .gossipsub
            .publish(topic, message)
            .wrap_err("failed to publish message")
    }
}

pub enum StreamItem {
    NewListenAddr(Multiaddr),
    Message(Message),
    // MdnsPeersConnected(Vec<PeerId>),
    // MdnsPeersDisconnected(Vec<PeerId>),
    PeerConnected(PeerId),
    PeerSubscribed(PeerId, TopicHash),
}

impl futures::Stream for Network {
    type Item = StreamItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(maybe_event) = self.swarm.poll_next_unpin(cx) {
            let Some(event) = maybe_event else {
                self.terminated = true;
                return Poll::Ready(None);
            };

            match event {
                SwarmEvent::Behaviour(MyBehaviourEvent::Ping(_)) => {}
                // SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                //     let peers = Vec::with_capacity(list.len());
                //     for (peer_id, _multiaddr) in list {
                //         println!("mDNS discovered a new peer: {peer_id}");
                //         self.swarm
                //             .behaviour_mut()
                //             .gossipsub
                //             .add_explicit_peer(&peer_id);
                //     }
                //     return Poll::Ready(Some(StreamItem::MdnsPeersConnected(peers)));
                // }
                // SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                //     let peers = Vec::with_capacity(list.len());
                //     for (peer_id, _multiaddr) in list {
                //         println!("mDNS discover peer has expired: {peer_id}");
                //         self.swarm
                //             .behaviour_mut()
                //             .gossipsub
                //             .remove_explicit_peer(&peer_id);
                //     }
                //     return Poll::Ready(Some(StreamItem::MdnsPeersDisconnected(peers)));
                // }
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => {
                    println!(
                        "Got message: '{}' with id: {id} from peer: {peer_id}",
                        String::from_utf8_lossy(&message.data),
                    );
                    return Poll::Ready(Some(StreamItem::Message(message)));
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                    return Poll::Ready(Some(StreamItem::NewListenAddr(address)));
                }
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(
                    gossipsub::Event::Subscribed { peer_id, topic },
                )) => {
                    println!(
                        "Peer {peer_id} subscribed to topic: {topic:?}",
                        peer_id = peer_id,
                        topic = topic,
                    );
                    return Poll::Ready(Some(StreamItem::PeerSubscribed(peer_id, topic)));
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    endpoint: _,
                    num_established,
                    concurrent_dial_errors: _,
                    established_in: _,
                } => {
                    println!(
                        "Connection with {peer_id} established (total: {num_established})",
                        peer_id = peer_id,
                        num_established = num_established,
                    );
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .add_explicit_peer(&peer_id);
                    return Poll::Ready(Some(StreamItem::PeerConnected(peer_id)));
                }
                _ => {
                    println!("unhandled swarm event: {:?}", event);
                }
            }
        }

        Poll::Pending
    }
}

impl FusedStream for Network {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use futures::{channel::oneshot, join, select};

    #[tokio::test]
    async fn test_gossip_two_nodes() {
        let (bootnode_tx, bootnode_rx) = oneshot::channel();
        let (alice_tx, mut alice_rx) = oneshot::channel();

        let msg_a = b"hello world".to_vec();
        let recv_msg_a = msg_a.clone();
        let msg_b = b"i am responding".to_vec();
        let recv_msg_b = msg_b.clone();

        let alice_handle = tokio::task::spawn(async move {
            let mut alice = Network::new(None, 9000).unwrap();

            let Some(event) = alice.next().await else {
                panic!("unexpected event");
            };

            match event {
                StreamItem::NewListenAddr(addr) => {
                    println!("Alice listening on {:?}", addr);
                    bootnode_tx.send(addr.clone()).unwrap();
                }
                _ => panic!("unexpected event"),
            };

            loop {
                let Some(event) = alice.next().await else {
                    break;
                };

                match event {
                    StreamItem::PeerConnected(peer_id) => {
                        println!("Alice connected to {:?}", peer_id);
                    }
                    StreamItem::PeerSubscribed(peer_id, topic) => {
                        println!("Remote peer {:?} subscribed to {:?}", peer_id, topic);
                        alice.publish(msg_a.clone()).await.unwrap();
                    }
                    StreamItem::Message(msg) => {
                        println!("Alice got message: {:?}", msg);
                        assert_eq!(msg.data, recv_msg_b);
                        alice_tx.send(()).unwrap();
                        return;
                    }
                    _ => {}
                }
            }
        });

        let bob_handle = tokio::task::spawn(async move {
            let bootnode = bootnode_rx.await.unwrap();
            let mut bob = Network::new(Some(&bootnode.to_string()), 9001).unwrap();
            loop {
                select! {
                    event = bob.select_next_some() => {
                        match event {
                            StreamItem::PeerConnected(peer_id) => {
                                println!("Bob connected to {:?}", peer_id);
                            }
                            StreamItem::Message(msg) => {
                                println!("Bob got message: {:?}", msg);
                                assert_eq!(msg.data, recv_msg_a);
                                bob.publish(msg_b.clone()).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                    _ = alice_rx => {
                        return;
                    }
                }
            }
        });

        let (res_a, res_b) = join!(alice_handle, bob_handle);
        res_a.unwrap();
        res_b.unwrap();
    }
}