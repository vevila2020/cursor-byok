//! Defines commands accepted by a Conversation runtime.

use crate::{cursor::protocol::proto::agent::v1 as pb, Error};

#[derive(Debug)]
pub enum RunFinish {
    TurnCompleted,
    Transport(TransportFinish),
}

#[derive(Debug)]
pub enum TransportFinish {
    Success,
    Failed(Error),
    Cancelled,
}

#[derive(Debug)]
pub enum TransportCommand {
    Append {
        seqno: i64,
        message: Box<pb::AgentClientMessage>,
    },
    RunFinished {
        generation: u64,
        finish: RunFinish,
    },
    /// Cursor's agent host now owns the loop; stop the server-side run and
    /// keep this transport open only for Cursor's control messages.
    SwitchToLocalLoop,
    /// The local loop finished a turn. The agent run stream must report the
    /// terminal interaction so Cursor does not treat the stream as dropped.
    LocalLoopFinished {
        turn_completed: bool,
    },
    Disconnect,
}
