// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use crate::store::{
    Applier, Callback, CasualMessage, CasualRouter, LocalReader, PeerFsm, PeerMsg, PeerStates,
    ProposalRouter, RaftCommand, SignificantMsg, StoreMsg, StoreRouter,
};
use crate::Error::RegionNotFound;
use crate::{Error as RaftStoreError, Result as RaftStoreResult};
use kvproto::raft_cmdpb::RaftCmdRequest;
use kvproto::raft_serverpb::RaftMessage;
use slog_global::info;
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tikv_util::deadline::Deadline;
use tikv_util::mpsc::Sender;
use tikv_util::time::ThreadReadId;

/// Routes messages to the raftstore.
pub trait RaftStoreRouter: StoreRouter + ProposalRouter + CasualRouter + Send + Clone {
    /// Sends RaftMessage to local store.
    fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()>;

    /// Sends a significant message. We should guarantee that the message can't be dropped.
    fn significant_send(&self, region_id: u64, msg: SignificantMsg) -> RaftStoreResult<()>;

    /// Broadcast a message generated by `msg_gen` to all Raft groups.
    fn broadcast_normal(&self, msg_gen: impl FnMut() -> PeerMsg);

    /// Send a casual message to the given region.
    fn send_casual_msg(&self, region_id: u64, msg: CasualMessage) -> RaftStoreResult<()> {
        <Self as CasualRouter>::send(self, region_id, msg)
    }

    /// Send a store message to the backend raft batch system.
    fn send_store_msg(&self, msg: StoreMsg) {
        <Self as StoreRouter>::send(self, msg)
    }

    /// Sends RaftCmdRequest to local store.
    fn send_command(&self, req: RaftCmdRequest, cb: Callback) -> RaftStoreResult<()> {
        send_command_impl(self, req, cb, None)
    }

    fn send_command_with_deadline(
        &self,
        req: RaftCmdRequest,
        cb: Callback,
        deadline: Deadline,
    ) -> RaftStoreResult<()> {
        send_command_impl(self, req, cb, Some(deadline))
    }

    /// Reports the peer being unreachable to the Region.
    fn report_unreachable(&self, store_id: u64) -> RaftStoreResult<()> {
        self.broadcast_normal(|| {
            PeerMsg::SignificantMsg(SignificantMsg::StoreUnreachable { store_id })
        });
        Ok(())
    }

    /// Broadcast an `StoreUnreachable` event to all Raft groups.
    fn broadcast_unreachable(&self, store_id: u64) {
        let _ = self.send_store_msg(StoreMsg::StoreUnreachable { store_id });
    }
}

pub trait LocalReadRouter: Send + Clone {
    fn read(
        &self,
        read_id: Option<ThreadReadId>,
        req: RaftCmdRequest,
        cb: Callback,
    ) -> RaftStoreResult<()>;
}

/// A router that routes messages to the raftstore
pub struct ServerRaftStoreRouter {
    router: RaftRouter,
    local_reader: RefCell<LocalReader>,
}

impl Clone for ServerRaftStoreRouter {
    fn clone(&self) -> Self {
        ServerRaftStoreRouter {
            router: self.router.clone(),
            local_reader: self.local_reader.clone(),
        }
    }
}

impl ServerRaftStoreRouter {
    /// Creates a new router.
    pub fn new(router: RaftRouter, reader: LocalReader) -> ServerRaftStoreRouter {
        let local_reader = RefCell::new(reader);
        ServerRaftStoreRouter {
            router,
            local_reader,
        }
    }
}

impl StoreRouter for ServerRaftStoreRouter {
    fn send(&self, msg: StoreMsg) {
        StoreRouter::send(&self.router, msg)
    }
}

impl ProposalRouter for ServerRaftStoreRouter {
    fn send(&self, cmd: RaftCommand) -> RaftStoreResult<()> {
        ProposalRouter::send(&self.router, cmd)
    }
}

impl CasualRouter for ServerRaftStoreRouter {
    fn send(&self, region_id: u64, msg: CasualMessage) -> RaftStoreResult<()> {
        CasualRouter::send(&self.router, region_id, msg)
    }
}

impl RaftStoreRouter for ServerRaftStoreRouter {
    fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()> {
        RaftStoreRouter::send_raft_msg(&self.router, msg)
    }

    /// Sends a significant message. We should guarantee that the message can't be dropped.
    fn significant_send(&self, region_id: u64, msg: SignificantMsg) -> RaftStoreResult<()> {
        RaftStoreRouter::significant_send(&self.router, region_id, msg)
    }

    fn broadcast_normal(&self, msg_gen: impl FnMut() -> PeerMsg) {
        self.router.broadcast_normal(msg_gen)
    }
}

impl LocalReadRouter for ServerRaftStoreRouter {
    fn read(
        &self,
        read_id: Option<ThreadReadId>,
        req: RaftCmdRequest,
        cb: Callback,
    ) -> RaftStoreResult<()> {
        let mut local_reader = self.local_reader.borrow_mut();
        local_reader.read(read_id, req, cb);
        Ok(())
    }
}

#[derive(Clone)]
pub struct RaftStoreBlackHole;

impl CasualRouter for RaftStoreBlackHole {
    fn send(&self, _: u64, _: CasualMessage) -> RaftStoreResult<()> {
        Ok(())
    }
}

impl ProposalRouter for RaftStoreBlackHole {
    fn send(&self, _: RaftCommand) -> RaftStoreResult<()> {
        Ok(())
    }
}

impl StoreRouter for RaftStoreBlackHole {
    fn send(&self, _: StoreMsg) {}
}

impl RaftStoreRouter for RaftStoreBlackHole {
    /// Sends RaftMessage to local store.
    fn send_raft_msg(&self, _: RaftMessage) -> RaftStoreResult<()> {
        Ok(())
    }

    /// Sends a significant message. We should guarantee that the message can't be dropped.
    fn significant_send(&self, _: u64, _: SignificantMsg) -> RaftStoreResult<()> {
        Ok(())
    }

    fn broadcast_normal(&self, _: impl FnMut() -> PeerMsg) {}
}

#[derive(Clone)]
pub struct RaftRouter {
    pub(crate) store_sender: Sender<StoreMsg>,
    pub(crate) peers: Arc<dashmap::DashMap<u64, PeerStates>>,
    pub(crate) peer_sender: Sender<(u64, PeerMsg)>,
}

impl RaftRouter {
    pub(crate) fn new(peer_sender: Sender<(u64, PeerMsg)>, store_sender: Sender<StoreMsg>) -> Self {
        Self {
            store_sender,
            peers: Arc::new(dashmap::DashMap::new()),
            peer_sender,
        }
    }

    pub(crate) fn get(&self, region_id: u64) -> Option<dashmap::mapref::one::Ref<u64, PeerStates>> {
        self.peers.get(&region_id)
    }

    pub(crate) fn register(&self, peer: PeerFsm) {
        let id = peer.peer.region().id;
        let ver = peer.peer.region().get_region_epoch().get_version();
        info!(
            "register region {}:{}, peer {}",
            id,
            ver,
            peer.peer.peer_id()
        );
        let applier = Applier::new_from_peer(&peer);
        let new_peer = PeerStates::new(applier, peer);
        self.peers.insert(id, new_peer);
    }

    pub(crate) fn close(&self, id: u64) {
        if let Some(peer) = self.peers.get(&id) {
            peer.closed.store(true, Ordering::Release);
            self.peers
                .remove(&peer.peer_fsm.lock().unwrap().peer.region_id);
        }
    }

    pub(crate) fn send(&self, id: u64, mut msg: PeerMsg) -> RaftStoreResult<()> {
        if let Some(peer) = self.peers.get(&id) {
            if !peer.closed.load(Ordering::Relaxed) {
                self.peer_sender.send((id, msg));
                return Ok(());
            }
        }
        Err(RegionNotFound(id, Some(msg)))
    }

    pub(crate) fn send_store(&self, msg: StoreMsg) {
        self.store_sender.send(msg);
    }
}

impl RaftStoreRouter for RaftRouter {
    fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()> {
        let region_id = msg.get_region_id();
        let raft_msg = PeerMsg::RaftMessage(msg);
        self.send(region_id, raft_msg)
    }

    fn significant_send(&self, region_id: u64, msg: SignificantMsg) -> RaftStoreResult<()> {
        let msg = PeerMsg::SignificantMsg(msg);
        self.send(region_id, msg)
    }

    fn broadcast_normal(&self, mut msg_gen: impl FnMut() -> PeerMsg) {
        for peer in self.peers.iter() {
            let msg = msg_gen();
            self.peer_sender.send((*peer.key(), msg));
        }
    }
}

fn send_command_impl(
    router: &impl ProposalRouter,
    req: RaftCmdRequest,
    cb: Callback,
    deadline: Option<Deadline>,
) -> RaftStoreResult<()> {
    let mut cmd = RaftCommand::new(req, cb);
    // TODO(x) handle deadline
    router.send(cmd)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_run() {
        println!("run")
    }
}
