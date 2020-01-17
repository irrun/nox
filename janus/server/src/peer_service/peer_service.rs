/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::config::PeerServiceConfig;
use crate::peer_service::{
    behaviour::PeerServiceBehaviour,
    notifications::{InPeerNotification, OutPeerNotification},
    transport::build_transport,
    transport::PeerServiceTransport,
};
use libp2p::{
    core::muxing::{StreamMuxerBox, SubstreamRef},
    identity, PeerId, Swarm,
};
use log::trace;
use parity_multiaddr::{Multiaddr, Protocol};
use std::sync::{Arc, Mutex};
use tokio::prelude::*;
use tokio::runtime::TaskExecutor;
use tokio::sync::mpsc;

pub struct PeerService {
    pub swarm:
        Box<Swarm<PeerServiceTransport, PeerServiceBehaviour<SubstreamRef<Arc<StreamMuxerBox>>>>>,
}

pub struct PeerServiceDescriptor {
    pub exit_sender: tokio::sync::oneshot::Sender<()>,
    pub peer_channel_out: mpsc::UnboundedReceiver<OutPeerNotification>,
    pub peer_channel_in: mpsc::UnboundedSender<InPeerNotification>,
}

impl PeerService {
    pub fn new(config: PeerServiceConfig) -> Arc<Mutex<Self>> {
        let local_key = identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        println!("peer service is starting with id = {}", local_peer_id);

        let mut swarm = {
            let transport = build_transport(local_key.clone(), config.socket_timeout);
            let behaviour = PeerServiceBehaviour::new(&local_peer_id, local_key.public());

            Box::new(Swarm::new(transport, behaviour, local_peer_id))
        };

        let mut listen_addr = Multiaddr::from(config.listen_ip);
        listen_addr.push(Protocol::Tcp(config.listen_port));
        Swarm::listen_on(&mut swarm, listen_addr).unwrap();

        Arc::new(Mutex::new(Self { swarm }))
    }
}

pub fn start_peer_service(
    node_service: Arc<Mutex<PeerService>>,
    executor: &TaskExecutor,
) -> Result<PeerServiceDescriptor, ()> {
    let (exit_sender, exit_receiver) = tokio::sync::oneshot::channel();
    let (channel_in_1, channel_out_1) = tokio::sync::mpsc::unbounded_channel();
    let (channel_in_2, channel_out_2) = tokio::sync::mpsc::unbounded_channel();

    executor.spawn(
        peer_service_executor(node_service.clone(), channel_out_1, channel_in_2)
            .select(exit_receiver.then(|_| Ok(())))
            .then(move |_| {
                trace!("peer_service/service: shutting down by external cmd");

                // notify network that this node just has been shutdown
                // TODO: hardering
                node_service.lock().unwrap().swarm.exit();
                Ok(())
            }),
    );

    Ok(PeerServiceDescriptor {
        exit_sender,
        peer_channel_in: channel_in_1,
        peer_channel_out: channel_out_2,
    })
}

fn peer_service_executor(
    peer_service: Arc<Mutex<PeerService>>,
    mut peer_service_in: mpsc::UnboundedReceiver<InPeerNotification>,
    mut peer_service_out: mpsc::UnboundedSender<OutPeerNotification>,
) -> impl futures::Future<Item = (), Error = ()> {
    futures::future::poll_fn(move || -> Result<_, ()> {
        loop {
            match peer_service_in.poll() {
                Ok(Async::Ready(Some(e))) => match e {
                    InPeerNotification::Relay {
                        src_id,
                        dst_id,
                        data,
                    } => peer_service
                        .lock()
                        .unwrap()
                        .swarm
                        .relay_message(src_id, dst_id, data),
                    InPeerNotification::NetworkState { dst_id, state } => peer_service
                        .lock()
                        .unwrap()
                        .swarm
                        .send_network_state(dst_id, state),
                },
                Ok(Async::NotReady) => break,
                Ok(Async::Ready(None)) => {
                    // TODO: propagate error
                    break;
                }
                Err(_) => {
                    // TODO: propagate error
                    break;
                }
            }
        }

        loop {
            match peer_service.lock().unwrap().swarm.poll() {
                Ok(Async::Ready(Some(e))) => {
                    trace!("peer_service/poll: received {:?} event", e);
                }
                Ok(Async::Ready(None)) => unreachable!("stream never ends"),
                Ok(Async::NotReady) => break,
                Err(_) => break,
            }
        }

        if let Some(e) = peer_service.lock().unwrap().swarm.pop_out_node_event() {
            trace!("peer_service/poll: sending {:?} to peer_service", e);

            peer_service_out.try_send(e).unwrap();
        }

        Ok(Async::NotReady)
    })
}
