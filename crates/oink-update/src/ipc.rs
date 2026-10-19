//! Dedicated local service codec. Never carried by the peer networking protocol.
use crate::{
    Error,
    barrier::Blocker,
    policy::{Command, UpdateStatus},
};
use serde::{Deserialize, Serialize};
pub const MAX_FRAME: usize = 65536;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Query,
    UserQuit,
    Startup,
    Command {
        command: Command,
    },
    Register {
        build: u64,
    },
    Poll {
        transaction: Option<String>,
    },
    Quiesced {
        transaction: String,
    },
    Busy {
        transaction: String,
        blockers: Vec<Blocker>,
    },
    Health {
        transaction: String,
        build: u64,
        input_ready: bool,
        routing_ready: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Status { status: UpdateStatus },
    Prepare { transaction: String, automatic: bool },
    Abort { transaction: String },
    Shutdown { transaction: String },
    HealthCheck { transaction: String, build: u64 },
    Registered,
    Error { error: Error },
}
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(value).map_err(|_| Error::Metadata)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    Ok(bytes)
}
pub fn decode<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, Error> {
    if bytes.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    let value: T = crate::feed::strict_json(bytes)?;
    // serde's internally tagged unit variants can ignore extra fields even
    // with deny_unknown_fields. Compare the complete typed shape as well.
    let original: serde_json::Value = crate::feed::strict_json(bytes)?;
    let normalized = serde_json::to_value(&value).map_err(|_| Error::Metadata)?;
    if original != normalized {
        return Err(Error::Metadata);
    }
    Ok(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_execution_or_trust_parameters_can_enter_local_codec() {
        for bytes in [
            br#"{"type":"command","command":{"type":"install","expected_build":2,"url":"https://example.com/x"}}"#
                .as_slice(),
            br#"{"type":"query","path":"C:\\x"}"#,
            br#"{"type":"query","type":"register","build":2}"#,
        ] {
            assert!(decode::<Request>(bytes).is_err());
        }
        assert!(matches!(
            decode::<Request>(&encode(&Request::Query).unwrap()).unwrap(),
            Request::Query
        ));
    }
}
