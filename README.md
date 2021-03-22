# almost-raft
Consensus or agreeing on some value is a fundamental issue in a distributed system. 
While there are algorithms like Paxos exists since long back, the complexity of those 
make implementation complicated.

So Raft was designed to solve the problem while keeping the algorithm understandable.

Raft tackles the problem in two steps -
* Leader Election - Elect a node as a leader on startup or when the existing one fails
* Log Replication - Maintain the log consistency among nodes 
  
For more on Raft [https://raft.github.io](https://raft.github.io).

## Why almost-raft
There is already an implementation of Raft in Rust by awesome people at tikv. If the complete raft is the requirement, 
check out the [tikv/raft-rs](https://github.com/tikv/raft-rs).

*almost-raft* was written with a few things in mind -
* Personal necessity, of course
* Minimalism
* Offloading the inter-node communication mechanism to implementing crates
* Users don't need to know or care about the inner workings of Raft 

*almost-raft* handles only the first step of the algorithm, the election, hence the name.
It's up to the developer on how to handle the log replication, if necessary.

## Usage
*almost-raft* uses a closed loop, the only way to communicate is to use mpsc channel and control
messages.

First step is to implement `trait Node`. 
For example - a simple node that uses mpsc channel to communicate with others
```rust
use tokio::sync::mpsc::Sender;
use almost_raft::{Message, Node};
#[derive(Debug, Clone)]
struct NodeMPSC {
    id: String,
    sender: Sender<Message<NodeMPSC>>,
}

#[async_trait]
impl Node for NodeMPSC {
    type NodeType = NodeMPSC;
    async fn send_message(&self, msg: Message<Self::NodeType>) {
        self.sender.send(msg).await;
    }

    fn node_id(&self) -> &String {
        &self.id
    }
}
```
To initiate `RaftElectionState`
```rust
let (heartbeat_interval, message_timeout, timeout, max_node, min_node) =
    (1000, 20, 5000, 5, 3);
let (tx, mut from_raft) = mpsc::channel(10);
let self_id = uuid::Uuid::new_v4().to_string();
let nodes = vec![]; // we'll add node later
let (state, tx_to_raft) = RaftElectionState::init(
    self_id,
    timeout,
    heartbeat_interval,
    message_timeout,
    nodes,
    tx.clone(),
    max_node,
    min_node,
);
```

Now we can start the election process using the `state`. But this will not necessarily start the
election, it'll wait as long as there isn't enough node (`min_node`).

```rust
tokio::spawn(raft_election(state));
```

Let's add nodes
```rust
let (tx,rx) = mpsc::channel(10);
tx_to_raft
    .send(Message::ControlAddNode(NodeMPSC {
        id: uuid::Uuid::new_v4().to_string(),
        sender: tx,
    }))
    .await;
```

Raft will notify through mpsc channel if there's any change in leadership. To receive the event
```rust
// let (tx, mut from_raft) = mpsc::channel(10);
// tx was used to initialize RaftElectionState
from_raft.recv().await;
```

