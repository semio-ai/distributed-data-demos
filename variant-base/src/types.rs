use serde::{Deserialize, Serialize};
use std::fmt;

/// Quality of Service levels for data replication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Qos {
    BestEffort = 1,
    LatestValue = 2,
    ReliableUdp = 3,
    ReliableTcp = 4,
}

impl Qos {
    /// Convert an integer to a QoS level.
    pub fn from_int(value: u8) -> Option<Qos> {
        match value {
            1 => Some(Qos::BestEffort),
            2 => Some(Qos::LatestValue),
            3 => Some(Qos::ReliableUdp),
            4 => Some(Qos::ReliableTcp),
            _ => None,
        }
    }

    /// Return the integer representation.
    pub fn as_int(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for Qos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_int())
    }
}

/// Test protocol phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Connect,
    Stabilize,
    Operate,
    Eot,
    Silent,
}

impl Phase {
    /// Return the phase name as a lowercase string.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Connect => "connect",
            Phase::Stabilize => "stabilize",
            Phase::Operate => "operate",
            Phase::Eot => "eot",
            Phase::Silent => "silent",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A received update from the transport layer.
#[derive(Debug, Clone)]
pub struct ReceivedUpdate {
    /// Runner name of the node that wrote the value.
    pub writer: String,
    /// The writer's sequence number for this update.
    pub seq: u64,
    /// Key path (e.g. `/sensors/lidar`).
    pub path: String,
    /// QoS level.
    pub qos: Qos,
    /// Serialized payload bytes.
    pub payload: Vec<u8>,
}
