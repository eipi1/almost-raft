//! Consensus or agreeing on some value is a fundamental issue in a distributed system.
//! While there are algorithms like Paxos exists since long back, the complexity of those
//! make implementation complicated.
//!
//! So Raft was designed to solve the problem while keeping the algorithm understandable.
//!
//! Raft tackles the problem in two steps -
//! * Leader Election - Elect a node as a leader on startup or when the existing one fails
//! * Log Replication - Maintain the log consistency among nodes
//!
//! **This crate handles Leader election provided a list of nodes.**
//!
//! For more on Raft [https://raft.github.io](https://raft.github.io).
//!
//! ## Usage
//! *almost-raft* uses a closed loop, the only way to communicate is to use mpsc channel and control
//! messages.
//!
//! First step is to implement `trait Node`.
//! For example - a simple node that uses mpsc channel to communicate with others
//! ```ignore
//! use tokio::sync::mpsc::Sender;
//! use almost_raft::{Message, Node};
//! #[derive(Debug, Clone)]
//! struct NodeMPSC {
//!     id: String,
//!     sender: Sender<Message<NodeMPSC>>,
//! }
//!
//! #[async_trait]
//! impl Node for NodeMPSC {
//!     type NodeType = NodeMPSC;
//!     async fn send_message(&self, msg: Message<Self::NodeType>) {
//!         self.sender.send(msg).await;
//!     }
//!
//!     fn node_id(&self) -> &String {
//!         &self.id
//!     }
//! }
//! ```
//! To initiate [RaftElectionState](crate::election::RaftElectionState)
//! ```ignore
//! use tokio::sync::mpsc;
//! use almost_raft::election::RaftElectionState;
//! let (heartbeat_interval, message_timeout, timeout, max_node, min_node) =
//!     (1000, 20, 5000, 5, 3);
//! let (tx, mut from_raft) = mpsc::channel(10);
//! let self_id = uuid::Uuid::new_v4().to_string();
//! let nodes = vec![]; // we'll add node later
//! let (state, tx_to_raft) = RaftElectionState::init(
//!     self_id,
//!     timeout,
//!     heartbeat_interval,
//!     message_timeout,
//!     nodes,
//!     tx.clone(),
//!     max_node,
//!     min_node,
//! );
//! ```
//!
//! Now we can start the election process using the `state`. But this will not necessarily start the
//! election, it'll wait as long as there isn't enough node (`min_node`).
//!
//! ```ignore
//! use almost_raft::election::raft_election;
//! tokio::spawn(raft_election(state));
//! ```
//!
//! Let's add nodes
//! ```ignore
//! use tokio::sync::mpsc;
//! use almost_raft::Message;
//! let (tx,rx) = mpsc::channel(10);
//! tx_to_raft
//!     .send(Message::ControlAddNode(NodeMPSC {
//!         id: uuid::Uuid::new_v4().to_string(),
//!         sender: tx,
//!     }))
//!     .await;
//! ```
//!
//! Raft will notify through mpsc channel if there's any change in leadership. To receive the event
//! ```ignore
//! // let (tx, mut from_raft) = mpsc::channel(10);
//! // tx was used to initialize RaftElectionState
//! from_raft.recv().await;
//! ```
//!

/// handles election process
pub mod election;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// States of the node
#[derive(Debug, PartialEq)]
pub enum NodeState {
    /// Initial or the normal state of the node
    Follower,
    /// Node is holding an election and calling for votes
    Candidate,
    /// Node won the election with majority votes and became leader
    Leader,
}

/// A Cluster node
#[async_trait]
pub trait Node {
    /// concrete node type
    type NodeType;
    /// send message to the node
    async fn send_message(&self, msg: Message<Self::NodeType>);
    /// unique node identifier
    fn node_id(&self) -> &String;
}

/// Messages to communicate with Raft
#[derive(Debug, Serialize, Deserialize)]
pub enum Message<T> {
    /// Asking for vote from other nodes for term
    RequestVote {
        /// Sender node id
        node_id: String,
        term: usize
    },
    /// Message in response to `Message::RequestVote`
    RequestVoteResponse { term: usize, vote: bool },
    /// Heartbeat message
    HeartBeat { leader_node_id: String, term: usize },
    /// Add a new node
    ControlAddNode(T),
    /// Remove an existing node
    ControlRemoveNode(T),
    /// A leader has been elected or change of existing one
    ControlLeaderChanged(String),
}

#[doc(hidden)]
#[macro_export]
macro_rules! log_error {
    ($result:expr) => {
        if let Err(e) = $result {
            error!("{}", e.to_string());
        }
    };
}

#[cfg(test)]
mod test {}
