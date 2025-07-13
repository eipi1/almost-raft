use crate::{log_error, ClusterNode, Message, NodeState};
use futures_util::stream::FuturesUnordered;
use log::{debug, error, info, trace};
use rand::Rng;

use std::cmp::min;
use std::fmt::Debug;

use std::result::Result::Err;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::RwLock;
use tokio::time::Instant;
use tokio_stream::StreamExt;

/// Current state of the Raft
#[derive(Debug)]
pub struct RaftElectionState<T: ClusterNode> {
    self_id: T::NodeIdType,
    election_timeout: u64,
    node_state: NodeState,
    votes: usize,
    term: usize,
    peers: Vec<T>, //maybe a better choice
    voted_for_term: bool,
    has_leader: bool,
    leader_node: Option<T::NodeIdType>,
    tx: Sender<Message<T>>,
    #[allow(dead_code)]
    incoming_tx: Sender<Message<T>>,
    incoming_rx: Arc<RwLock<Receiver<Message<T>>>>,
    heartbeat_interval: u64,
    message_timeout: u64,
    /// minimum nodes needed before starting election, excluding self
    /// if a cluster require minimum 3 nodes, set `min_node=2`, this node & 2 other peers
    min_node: usize,
}

impl<T: ClusterNode> RaftElectionState<T> {
    /// Initiate Raft election. This method doesn't start the election.
    /// # Arguments
    /// * `self_id` - Identifier of this node
    /// * `election_timeout` - Time between the elections if no heartbeat is received. Will use a 
    ///   randomized value in range `[election_timeout..election_timeout*2]`
    /// * `heartbeat_interval` - Interval between heartbeat message
    /// * `message_timeout` - Timout before treating message sending as failure
    /// * `peers` - other nodes
    /// * `tx` - MPSC Sender to communicate with outside. Control messages will use this channel
    /// * `max_node` - Maximum number of allowed node in the cluster
    /// * `min_node` - Minimum node required. Election will not start until number of node reach `min_node`
    ///
    /// # Returns
    /// Tuple - the initialized `RaftElectionState` & a Sender of mpsc channel for incoming control messages
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        self_id: T::NodeIdType,
        election_timeout: u64,
        heartbeat_interval: u64,
        message_timeout: u64,
        peers: Vec<T>,
        tx: Sender<Message<T>>,
        max_node: usize,
        min_node: usize,
    ) -> (Self, Sender<Message<T>>) {
        let timeout = rand::thread_rng().gen_range(election_timeout..election_timeout * 2);
        let (incoming, rx) = channel(max_node * 2);
        (
            RaftElectionState {
                self_id,
                election_timeout: timeout,
                node_state: NodeState::Follower,
                votes: 0,
                term: 0,
                peers,
                voted_for_term: false,
                has_leader: false,
                leader_node: None,
                tx,
                incoming_tx: incoming.clone(),
                incoming_rx: Arc::new(RwLock::new(rx)),
                heartbeat_interval,
                message_timeout,
                min_node,
            },
            incoming,
        )
    }
}

/// Start the election process.
/// Note that, the function has a infinite loop, so will never return.
pub async fn raft_election<T: ClusterNode + Debug>(mut state: RaftElectionState<T>) {
    info!("[node: {}] starting election process...", &state.self_id);
    let incoming = state.incoming_rx.clone();
    let mut remaining_election_timeout = state.election_timeout;
    let mut remaining_heartbeat_interval = state.heartbeat_interval;
    loop {
        if matches!(&state.node_state, NodeState::Terminating) {
            break;
        }
        let instant = Instant::now();
        let current_timeout = get_current_timeout(
            remaining_heartbeat_interval,
            remaining_election_timeout,
            matches!(state.node_state, NodeState::Leader),
        );
        trace!(
            "[node: {}] setting new timeout to {}",
            &state.self_id,
            current_timeout
        );
        let result = {
            let mut recv = incoming.write().await;
            let recv = recv.recv();
            let msg_or_timeout = tokio::time::timeout(Duration::from_millis(current_timeout), recv);
            msg_or_timeout.await
        };
        let elapsed = instant.elapsed().as_millis() as u64;
        if state.node_state == NodeState::Leader {
            // this block can be reached for two reasons
            // 1. heartbeat timeout => send heartbeat
            // 2. got a message from outside => recalculate remaining time
            // leader will never get election timeout
            if elapsed >= remaining_heartbeat_interval {
                trace!("[node: {}] heartbeat timeout", &state.self_id);
                // case#1: heartbeat timeout
                do_leader_stuff(&state).await;
                remaining_heartbeat_interval = state.heartbeat_interval;
            } else {
                // case#2: received message or something
                remaining_heartbeat_interval =
                    unsigned_subtract(remaining_election_timeout, elapsed);
            }
            // continue;
        }
        if let Ok(msg) = result {
            //got some message
            let heartbeat = handle_message(&mut state, msg).await;
            if heartbeat {
                //if it's a heartbeat msg, reset the election timeout so that as long as
                // node is receiving heartbeat, it'll never start election
                remaining_election_timeout = state.election_timeout;
            } else {
                // for leader, following doesn't have any effect, as it'll reset to
                // MAX value in get_current_timeout
                remaining_election_timeout = unsigned_subtract(
                    remaining_election_timeout,
                    instant.elapsed().as_millis() as u64,
                );
            }
            // only leader will have heartbeat interval. so no need to care about that in here.
            // already handled in previous if block (state.node_state == NodeState::Leader)
        } else {
            //no message, normal timeout
            handle_after_timeout(&mut state).await;
            remaining_election_timeout = state.election_timeout;
        }
        trace!(
            "[node: {}] remaining_election_timeout: {}, remaining_heartbeat_interval: {}",
            &state.self_id,
            remaining_election_timeout,
            remaining_heartbeat_interval
        )
    }
}

#[inline(always)]
fn unsigned_subtract<T>(lhs: T, rhs: T) -> T
where
    T: PartialEq + PartialOrd + std::ops::Sub<Output = T> + From<u64>,
{
    if lhs < rhs {
        0.into()
    } else {
        lhs - rhs
    }
}

#[inline]
fn get_current_timeout(
    remaining_heartbeat_interval: u64,
    remaining_election_timeout: u64,
    leader: bool,
) -> u64 {
    if leader {
        //for leader, there's no election timeout
        min(u64::MAX, remaining_heartbeat_interval)
    } else {
        remaining_election_timeout
    }
}

async fn do_leader_stuff<T: ClusterNode>(state: &RaftElectionState<T>) {
    let heartbeat_fut = send_heartbeat(state);
    let result =
        tokio::time::timeout(Duration::from_millis(state.message_timeout), heartbeat_fut).await;
    if let Err(_e) = result {
        error!("failed to send heartbeat - request timeout.");
    }
}

async fn be_a_leader<T: ClusterNode>(state: &mut RaftElectionState<T>) {
    debug!(
        "[node: {}] updating node state to NodeState::Leader",
        &state.self_id
    );
    state.node_state = NodeState::Leader;
    state.leader_node = Some(state.self_id.clone());
    let result = state
        .tx
        .send_timeout(
            Message::ControlLeaderChanged(state.self_id.clone()),
            Duration::from_millis(state.message_timeout),
        )
        .await;
    if let Err(e) = result {
        //todo how to handle this error
        error!(
            "failed to send control message ControlLeaderChanged: {}",
            e.to_string()
        );
    }
    do_leader_stuff(state).await;
}

async fn send_heartbeat<T: ClusterNode>(state: &RaftElectionState<T>) {
    let mut messages = FuturesUnordered::new();
    for peer in state.peers.iter() {
        let msg = peer.send_message(Message::HeartBeat {
            leader_node_id: state.self_id.clone(),
            term: state.term,
        });
        messages.push(msg);
    }
    while let Some(_result) = messages.next().await {
        trace!("[node: {}] sent heartbeat", &state.self_id);
    }
}

async fn handle_after_timeout<T: ClusterNode + Debug>(state: &mut RaftElectionState<T>) {
    //don't start election there isn't enough nodes.
    let current_node_count = state.peers.len();
    if current_node_count < state.min_node {
        trace!(
            "[node: {}] not enough node - required: {}, found: {}",
            &state.self_id,
            state.min_node,
            current_node_count
        );
        return;
    }

    if state.has_leader || (state.node_state == NodeState::Leader) {
        // let's reset has_leader to false.
        // some external process(or message) should set the flag to true on heartbeat message.
        // has_leader after timeout means no heartbeat was received from the leader
        // during timout
        state.has_leader = false | (state.node_state == NodeState::Leader);
        return;
    }

    //has no leader or initializing the raft system
    //promote to Candidate
    // if candidate, just restart the voting process.
    if matches!(state.node_state, NodeState::Follower | NodeState::Candidate) {
        debug!(
            "[node: {}] updating node state to NodeState::Candidate",
            &state.self_id
        );
        trace!(
            "[node: {}] updating term from {} to {}",
            &state.self_id,
            &state.term,
            state.term + 1
        );
        state.term += 1;
        state.node_state = NodeState::Candidate;
        //self vote
        state.votes = 1;
        //ask peers to vote
        for peer in state.peers.iter() {
            let msg = Message::RequestVote {
                requester_node_id: state.self_id.clone(),
                term: state.term,
            };
            trace!(
                "[node: {}] sending vote request for term {} to: {:?}",
                &state.self_id,
                &state.term,
                peer
            );
            peer.send_message(msg).await;
        }
    }
    if state.votes > (current_node_count + 1) / 2 {
        state.node_state = NodeState::Leader;
        // for peer in state.peers.iter() {
        //     peer.send_message(Message::LeaderAnnouncement(state.self_id.clone()))
        //         .await;
        // }
        let result = state
            .tx
            .send_timeout(
                Message::ControlLeaderChanged(state.self_id.clone()),
                Duration::from_millis(state.message_timeout),
            )
            .await;
        log_error!(result);
    }
}

async fn handle_message<T: ClusterNode + Debug>(
    state: &mut RaftElectionState<T>,
    msg: Option<Message<T>>,
) -> bool {
    trace!("[node: {}] got message {:?}", &state.self_id, &msg);
    let mut heartbeat = false;
    if let Some(msg) = msg {
        match msg {
            Message::HeartBeat {
                leader_node_id,
                term,
            } => {
                handle_heartbeat(state, leader_node_id, term).await;
                heartbeat = true;
            }
            Message::RequestVote {
                requester_node_id: node_id,
                term,
            } => {
                handle_request_vote(state, node_id, term).await;
            }
            Message::RequestVoteResponse { term, vote } => {
                handle_vote_response(state, term, vote).await;
            }
            Message::ControlAddNode(node) => {
                handle_add_node(state, node);
            }
            Message::ControlRemoveNode(node) => {
                handle_remove_node(state, node);
            }
            _ => {}
        }
    }
    heartbeat
}

#[inline]
fn handle_remove_node<T: ClusterNode>(state: &mut RaftElectionState<T>, node: T) {
    if node.node_id() == &state.self_id {
        info!("[node: {}] terminating node", &state.self_id);
        state.node_state = NodeState::Terminating;
    }
    let mut found_at = usize::MAX;
    for (idx, peer) in state.peers.iter().enumerate() {
        if peer.node_id() == node.node_id() {
            found_at = idx;
            info!(
                "[node: {}] removing node {}",
                &state.self_id,
                node.node_id()
            );
            break;
        }
    }
    if found_at != usize::MAX {
        state.peers.remove(found_at);
    }
}

#[inline(always)]
fn handle_add_node<T: ClusterNode>(state: &mut RaftElectionState<T>, node: T) {
    state.peers.push(node);
}

async fn handle_vote_response<T: ClusterNode>(
    state: &mut RaftElectionState<T>,
    term: usize,
    vote: bool,
) {
    if term != state.term || !matches!(state.node_state, NodeState::Candidate) || !vote {
        // reject vote
        return;
    }
    state.votes += 1;
    if state.votes > (state.peers.len() + 1) / 2 {
        // yay,I won
        be_a_leader(state).await
    }
}

async fn handle_heartbeat<T: ClusterNode>(
    state: &mut RaftElectionState<T>,
    leader_node_id: T::NodeIdType,
    term: usize,
) {
    if state.term > term && matches!(state.node_state, NodeState::Follower) {
        error!("follower's term shouldn't be greater than leader.");
        return;
    }
    if matches!(state.node_state, NodeState::Leader | NodeState::Candidate) {
        // if a leader/candidate receives heartbeat
        // todo verify demotion logic
        if term >= state.term {
            state.node_state = NodeState::Follower;
        } else {
            // todo reconfirm
            return;
        }
    }

    //todo review the necessity of the voted_for_term
    state.voted_for_term = true;
    state.term = term;
    state.has_leader = true;
    if let Some(current_leader_node) = &state.leader_node {
        if current_leader_node == &leader_node_id {
            //leader hasn't changed
            return;
        }
    }
    trace!("[node: {}] updating term to {}", &state.self_id, &term);
    debug!(
        "[node: {}] leader changed to node: {}",
        &state.self_id, &leader_node_id
    );
    //only node with higher term will get vote
    state.leader_node = Some(leader_node_id.clone());
    let result = state
        .tx
        .send_timeout(
            Message::ControlLeaderChanged(leader_node_id),
            Duration::from_millis(state.message_timeout),
        )
        .await;
    log_error!(result);
}

async fn handle_request_vote<T: ClusterNode>(
    state: &mut RaftElectionState<T>,
    requester_node_id: T::NodeIdType,
    term: usize,
) {
    if matches!(state.node_state, NodeState::Candidate) {
        trace!(
            "[node: {}] already a candidate node, don't vote",
            &state.self_id
        );
        return;
    }
    if state.term > term {
        trace!(
            "[node: {}] term is not high enough; current term: {}, requester: {}",
            &state.self_id,
            state.term,
            term
        );
        return;
    } else if state.term == term && state.voted_for_term {
        trace!("Already voted for the term: {}", &term);
        return;
    }
    trace!(
        "[node: {}] updating term from {} to {}",
        &state.self_id,
        &state.term,
        &term
    );
    state.term = term;
    state.voted_for_term = true;
    send_vote(state, requester_node_id, term).await;
}

async fn send_vote<T: ClusterNode>(
    state: &mut RaftElectionState<T>,
    requester_node_id: T::NodeIdType,
    term: usize,
) {
    for peer in state.peers.iter() {
        if peer.node_id() == &requester_node_id {
            trace!(
                "[node: {}] current term: {}, sending vote to node: {}, for term: {}",
                &state.self_id,
                state.term,
                &requester_node_id,
                term
            );
            peer.send_message(Message::RequestVoteResponse {
                vote: true,
                term: state.term,
            })
            .await;
            break;
        }
    }
}

#[cfg(test)]
#[allow(unused_imports)]
#[allow(unused_variables)]
mod test {
    use crate::election::{raft_election, RaftElectionState};
    use crate::{ClusterNode, Message, NodeState};
    use async_trait::async_trait;
    use log::trace;
    use rand::Rng;
    use std::sync::{Arc, Once};
    use std::thread::yield_now;
    use std::time::Duration;
    use tokio::sync::mpsc::{channel, Receiver, Sender};
    use tokio::sync::{mpsc, RwLock};
    use tokio::task;
    use tokio::time::{advance, pause, resume};

    static ONCE: Once = Once::new();

    fn setup() {
        ONCE.call_once(|| {
            tracing_subscriber::fmt()
                .with_env_filter("trace")
                .try_init()
                .unwrap();
        });
    }

    macro_rules! d {
        ($ex:expr) => {
            Duration::from_millis($ex)
        };
    }

    #[derive(Debug, Clone)]
    struct NodeDummy {
        id: String,
        sender: Sender<Message<NodeDummy>>,
    }

    #[async_trait]
    impl ClusterNode for NodeDummy {
        type NodeIdType = String;
        type NodeType = NodeDummy;
        async fn send_message(&self, msg: Message<Self::NodeType>) {
            trace!("[node: {}] sending message {:?}", self.id, &msg);
            let _ = self.sender.send(msg).await;
        }

        fn node_id(&self) -> &String {
            &self.id
        }
    }

    #[tokio::test]
    async fn test_min_node() {
        setup();
        let vec1: Vec<NodeDummy> = vec![];
        let (heartbeat_interval, heartbeat_timeout, mut timeout, max_node, min_node) =
            (1000, 20, 5000, 5, 3);
        let (tx, from_raft) = mpsc::channel(10);
        let self_id = uuid::Uuid::new_v4().to_string();
        let (state, tx) = RaftElectionState::init(
            self_id,
            timeout,
            heartbeat_interval,
            heartbeat_timeout,
            vec1,
            tx.clone(),
            max_node,
            min_node,
        );
        timeout = state.election_timeout;
        pause();
        let handle = tokio::spawn(raft_election(state));
        trace!("{}", timeout);
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        let (tx_node, mut rx_node) = channel(10);

        // add node
        let dummy = new_node("1", tx_node.clone());
        let _ = tx.send(Message::ControlAddNode(dummy)).await;
        let dummy = new_node("2", tx_node.clone());
        let _ = tx.send(Message::ControlAddNode(dummy)).await;
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        resume();
        // not enough node, shouldn't be any request for vote
        let result = tokio::time::timeout(Duration::from_millis(5), rx_node.recv()).await;
        assert!(result.is_err());

        pause();
        let dummy = new_node("3", tx_node.clone());
        let _ = tx.send(Message::ControlAddNode(dummy)).await;
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        resume();
        // enough node, node should get request for vote
        let result = tokio::time::timeout(Duration::from_millis(5), rx_node.recv()).await;
        trace!("{:?}", result);
        assert!(matches!(
            result.ok().flatten().unwrap(),
            Message::RequestVote { .. }
        ));
    }

    #[tokio::test]
    async fn test_raft_election() {
        setup();
        let (tx_node, mut rx_node) = channel(10);
        let nodes = vec![
            new_node("1", tx_node.clone()),
            new_node("2", tx_node.clone()),
            new_node("3", tx_node.clone()),
            new_node("3", tx_node.clone()),
        ];
        let (heartbeat_interval, heartbeat_timeout, timeout, max_node, min_node) =
            (1000, 20, 5000, 5, 3);
        let (tx, mut from_raft) = mpsc::channel(10);
        let self_id = uuid::Uuid::new_v4().to_string();
        let (state, tx_to_raft) = RaftElectionState::init(
            self_id,
            timeout,
            heartbeat_interval,
            heartbeat_timeout,
            nodes,
            tx.clone(),
            max_node,
            min_node,
        );

        let timeout = state.election_timeout;
        pause();
        tokio::spawn(raft_election(state));
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        resume();

        //should get four request
        for _i in 0..4 {
            let msg = rx_node.recv().await;
            trace!("{:?}", &msg);
            assert!(matches!(msg.unwrap(), Message::RequestVote { .. }));
        }

        // send vote
        let _ = tx_to_raft
            .send(Message::RequestVoteResponse {
                vote: true,
                term: 1,
            })
            .await;
        let _ = tx_to_raft
            .send(Message::RequestVoteResponse {
                vote: true,
                term: 1,
            })
            .await;
        let _ = tx_to_raft
            .send(Message::RequestVoteResponse {
                vote: true,
                term: 1,
            })
            .await;

        // this node should be leader now.
        let result = from_raft.recv().await.unwrap();
        assert!(matches!(result, Message::ControlLeaderChanged { .. }));

        // other nodes should receive heartbeat
        let result = rx_node.recv().await.unwrap();
        assert!(matches!(result, Message::HeartBeat { .. }));
        let result = rx_node.recv().await.unwrap();
        let result = rx_node.recv().await.unwrap();
        let result = rx_node.recv().await.unwrap();
        let result = tokio::time::timeout(Duration::from_millis(2), rx_node.recv()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_raft_election_multi_term() {
        setup();
        let (tx_node, mut rx_node) = channel(10);
        let nodes = vec![
            new_node("1", tx_node.clone()),
            new_node("2", tx_node.clone()),
            new_node("3", tx_node.clone()),
            new_node("4", tx_node.clone()),
        ];
        let (heartbeat_interval, heartbeat_timeout, timeout, max_node, min_node) =
            (1000, 20, 5000, 5, 3);
        let (tx, mut from_raft) = mpsc::channel(10);
        let self_id = "raft-node".to_string();
        let (state, tx_to_raft) = RaftElectionState::init(
            self_id,
            timeout,
            heartbeat_interval,
            heartbeat_timeout,
            nodes,
            tx.clone(),
            max_node,
            min_node,
        );

        let timeout = state.election_timeout;
        pause();
        tokio::spawn(raft_election(state));
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        resume();
        //should get four request
        for _i in 0..4 {
            let msg = rx_node.recv().await;
        }
        // send 1 vote, it'll be 2(self+1), not majority
        pause();
        let _ = tx_to_raft
            .send(Message::RequestVoteResponse {
                vote: true,
                term: 1,
            })
            .await;
        let _ = task::yield_now().await;
        advance(d!(timeout)).await;
        resume();

        // restart the vote on next term
        let msg = rx_node.recv().await;
        let msg = msg.unwrap();
        assert!(matches!(&msg, Message::RequestVote { .. }));
        trace!("multi-term: {:?}", msg);
        if let Message::RequestVote {
            term,
            requester_node_id: node_id,
        } = msg
        {
            assert_eq!(term, 2);
        } else {
            panic!("Wrong message.");
        }

        //should get four request
        for _i in 0..3 {
            let msg = rx_node.recv().await;
            trace!("{:?}", &msg);
            assert!(matches!(msg.unwrap(), Message::RequestVote { .. }));
        }

        for _i in 0..2 {
            let _ = tx_to_raft
                .send(Message::RequestVoteResponse {
                    vote: true,
                    term: 2,
                })
                .await;
        }

        // this node should be leader now.
        let result = from_raft.recv().await.unwrap();
        assert!(matches!(result, Message::ControlLeaderChanged { .. }));
    }

    #[tokio::test]
    async fn test_raft_election_multi_node() {
        setup();
        let nodes: Vec<NodeDummy> = vec![];
        let (heartbeat_interval, message_timeout, timeout, max_node, min_node) = (20, 2, 50, 5, 2);

        let election_timeout_1 = 60;
        let (mut from_raft_node_1, self_id_1, state_1, tx_to_raft_node_1) =
            create_raft_node_fixed_timeout(
                "raft-node-1".to_string(),
                &nodes,
                election_timeout_1,
                heartbeat_interval,
                message_timeout,
                max_node,
                min_node,
            );

        let election_timeout_2 = 70;
        let (from_raft_node_2, self_id_2, state_2, tx_to_raft_node_2) =
            create_raft_node_fixed_timeout(
                "raft-node-2".to_string(),
                &nodes,
                election_timeout_2,
                heartbeat_interval,
                message_timeout,
                max_node,
                min_node,
            );

        let election_timeout_3 = 80;
        let (from_raft_node_3, self_id_3, state_3, tx_to_raft_node_3) =
            create_raft_node_fixed_timeout(
                "raft-node-3".to_string(),
                &nodes,
                election_timeout_3,
                heartbeat_interval,
                message_timeout,
                max_node,
                min_node,
            );

        // pause();
        tokio::spawn(raft_election(state_1));
        tokio::spawn(raft_election(state_2));
        tokio::spawn(raft_election(state_3));

        //add raft-node to other node
        let _ = tx_to_raft_node_1
            .send(Message::ControlAddNode(new_node(
                self_id_2.as_str(),
                tx_to_raft_node_2.clone(),
            )))
            .await;
        let _ = tx_to_raft_node_1
            .send(Message::ControlAddNode(new_node(
                self_id_3.as_str(),
                tx_to_raft_node_3.clone(),
            )))
            .await;

        let _ = tx_to_raft_node_2
            .send(Message::ControlAddNode(new_node(
                self_id_1.as_str(),
                tx_to_raft_node_1.clone(),
            )))
            .await;
        let _ = tx_to_raft_node_2
            .send(Message::ControlAddNode(new_node(
                self_id_3.as_str(),
                tx_to_raft_node_3.clone(),
            )))
            .await;
        let _ = tx_to_raft_node_3
            .send(Message::ControlAddNode(new_node(
                self_id_1.as_str(),
                tx_to_raft_node_1.clone(),
            )))
            .await;
        let _ = tx_to_raft_node_3
            .send(Message::ControlAddNode(new_node(
                self_id_2.as_str(),
                tx_to_raft_node_2.clone(),
            )))
            .await;

        tokio::time::sleep(d!(85)).await;

        let msg = from_raft_node_1.recv().await;
        if let Some(msg) = msg {
            trace!("{:?}", &msg);
            if let Message::ControlLeaderChanged(id) = msg {
                assert_eq!(id, self_id_1);
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn create_raft_node_fixed_timeout(
        self_id: String,
        nodes: &[NodeDummy],
        election_timeout: u64,
        heartbeat_interval: u64,
        heartbeat_timeout: u64,
        max_node: usize,
        min_node: usize,
    ) -> (
        Receiver<Message<NodeDummy>>,
        String,
        RaftElectionState<NodeDummy>,
        Sender<Message<NodeDummy>>,
    ) {
        let (tx_node_1, from_raft_node_1) = mpsc::channel(10);
        let (state_1, tx_to_raft_node_1) = init_raft_fixed_timeout(
            self_id.clone(),
            election_timeout,
            heartbeat_interval,
            heartbeat_timeout,
            nodes.to_owned(),
            tx_node_1.clone(),
            max_node,
            min_node,
        );
        (from_raft_node_1, self_id, state_1, tx_to_raft_node_1)
    }

    fn new_node(id: &str, tx_node: Sender<Message<NodeDummy>>) -> NodeDummy {
        NodeDummy {
            id: id.to_string(),
            sender: tx_node.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn init_raft_fixed_timeout<T: ClusterNode<NodeIdType=String>>(
        self_id: String,
        timeout: u64,
        heartbeat_interval: u64,
        heartbeat_timeout: u64,
        peers: Vec<T>,
        tx: Sender<Message<T>>,
        max_node: usize,
        min_node: usize,
    ) -> (RaftElectionState<T>, Sender<Message<T>>) {
        let (incoming, rx) = channel(max_node * 2);
        (
            RaftElectionState {
                self_id,
                election_timeout: timeout,
                node_state: NodeState::Follower,
                votes: 0,
                term: 0,
                peers,
                voted_for_term: false,
                has_leader: false,
                leader_node: None,
                tx,
                incoming_tx: incoming.clone(),
                incoming_rx: Arc::new(RwLock::new(rx)),
                heartbeat_interval,
                message_timeout: heartbeat_timeout,
                min_node,
            },
            incoming,
        )
    }
}
