use async_trait::async_trait;
use bson::{doc, Document};
use bytes::buf::BufExt;
use lazy_static::lazy_static;
use mqtt3::Event;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    client::{Handled, MqttEventHandler},
    pump::{PumpHandle, PumpMessage},
    upstream::{CommandId, RemoteUpstreamPumpEvent, RpcCommand},
};

use super::RpcError;

/// An RPC handlers that responsible to connect to part of the bridge which
/// connects to local broker.
///
/// It receives RPC commands on a special topic, converts it to a `RpcCommand`
/// and sends to remote pump as a `PumpMessage`.
pub struct LocalRpcMqttEventHandler {
    remote_pump: PumpHandle<RemoteUpstreamPumpEvent>,
}

impl LocalRpcMqttEventHandler {
    /// Creates a new instance of local part of RPC handler.
    pub fn new(remote_pump: PumpHandle<RemoteUpstreamPumpEvent>) -> Self {
        Self { remote_pump }
    }
}

#[async_trait]
impl MqttEventHandler for LocalRpcMqttEventHandler {
    type Error = RpcError;

    async fn handle(&mut self, event: Event) -> Result<Handled, Self::Error> {
        if let Event::Publication(publication) = &event {
            if let Some(command_id) = capture_command_id(&publication.topic_name) {
                let doc = Document::from_reader(&mut publication.payload.clone().reader())?;
                match bson::from_document(doc)? {
                    VersionedRpcCommand::V1(command) => {
                        let event =
                            RemoteUpstreamPumpEvent::RpcCommand(command_id.clone(), command);
                        let msg = PumpMessage::Event(event);
                        self.remote_pump
                            .send(msg)
                            .await
                            .map_err(|e| RpcError::SendToRemotePump(command_id, e))?;

                        return Ok(Handled::Fully);
                    }
                }
            }
        }

        Ok(Handled::Skipped(event))
    }
}

fn capture_command_id(topic_name: &str) -> Option<CommandId> {
    lazy_static! {
        static ref RPC_TOPIC_PATTERN: Regex = Regex::new("\\$upstream/rpc/(?P<command_id>[^/ ]+)$")
            .expect("failed to create new Regex from pattern");
    }

    RPC_TOPIC_PATTERN
        .captures(topic_name)
        .and_then(|captures| captures.name("command_id"))
        .map(|command_id| command_id.as_str().into())
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "version")]
enum VersionedRpcCommand {
    V1(RpcCommand),
}

#[cfg(test)]
mod tests {
    use bson::{bson, spec::BinarySubtype};
    use bytes::Bytes;
    use matches::assert_matches;
    use mqtt3::{proto::QoS, ReceivedPublication};
    use test_case::test_case;

    use super::*;

    #[test]
    fn it_deserializes_from_bson() {
        let commands = vec![
            (
                bson!({
                    "version": "v1",
                    "cmd": "sub",
                    "topic": "/foo",
                }),
                VersionedRpcCommand::V1(RpcCommand::Subscribe {
                    topic_filter: "/foo".into(),
                }),
            ),
            (
                bson!({
                    "version": "v1",
                    "cmd": "unsub",
                    "topic": "/foo",
                }),
                VersionedRpcCommand::V1(RpcCommand::Unsubscribe {
                    topic_filter: "/foo".into(),
                }),
            ),
            (
                bson!({
                    "version": "v1",
                    "cmd": "pub",
                    "topic": "/foo",
                    "payload": vec![100, 97, 116, 97]
                }),
                VersionedRpcCommand::V1(RpcCommand::Publish {
                    topic: "/foo".into(),
                    payload: b"data".to_vec(),
                }),
            ),
        ];

        for (command, expected) in commands {
            let rpc: VersionedRpcCommand = bson::from_bson(command).unwrap();
            assert_eq!(rpc, expected);
        }
    }

    #[test_case(r"$upstream/rpc/foo", Some("foo".into()); "when word")]
    #[test_case(r"$upstream/rpc/CA761232-ED42-11CE-BACD-00AA0057B223", Some("CA761232-ED42-11CE-BACD-00AA0057B223".into()); "when uuid")]
    #[test_case(r"$downstream/rpc/ack/CA761232-ED42-11CE-BACD-00AA0057B223", None; "when ack")]
    #[test_case(r"$iothub/rpc/ack/CA761232-ED42-11CE-BACD-00AA0057B223", None; "when wrong topic")]
    #[test_case(r"$iothub/rpc/ack/some id", None; "when spaces")]
    #[allow(clippy::needless_pass_by_value)]
    fn it_captures_command_id(topic: &str, expected: Option<CommandId>) {
        assert_eq!(capture_command_id(topic), expected)
    }

    #[tokio::test]
    async fn it_handles_rpc_commands() {
        let (pump_handle, mut rx) = crate::pump::channel();
        let mut handler = LocalRpcMqttEventHandler::new(pump_handle);

        let event = command("1", "sub", "/foo", None);
        let res = handler.handle(event).await;
        assert_matches!(res, Ok(Handled::Fully));
        assert_matches!(rx.recv().await, Some(PumpMessage::Event(RemoteUpstreamPumpEvent::RpcCommand(id, RpcCommand::Subscribe{topic_filter}))) if topic_filter == "/foo" && id == "1".into());

        let event = command("2", "unsub", "/foo", None);
        let res = handler.handle(event).await;
        assert_matches!(res, Ok(Handled::Fully));
        assert_matches!(rx.recv().await, Some(PumpMessage::Event(RemoteUpstreamPumpEvent::RpcCommand(id, RpcCommand::Unsubscribe{topic_filter}))) if topic_filter == "/foo" && id == "2".into());

        let event = command("3", "pub", "/foo", Some(b"hello".to_vec()));
        let res = handler.handle(event).await;
        assert_matches!(res, Ok(Handled::Fully));
        assert_matches!(rx.recv().await, Some(PumpMessage::Event(RemoteUpstreamPumpEvent::RpcCommand(id, RpcCommand::Publish{topic, payload}))) if topic == "/foo" && payload == b"hello" && id == "3".into());
    }

    #[tokio::test]
    async fn it_skips_when_not_rpc_command() {
        let (pump_handle, _) = crate::pump::channel();
        let mut handler = LocalRpcMqttEventHandler::new(pump_handle);

        let event = Event::Publication(ReceivedPublication {
            topic_name: "$edgehub/twin/$edgeHub".into(),
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            payload: Bytes::default(),
        });
        let res = handler.handle(event).await;
        assert_matches!(res, Ok(Handled::Skipped(_)));
    }

    fn command(id: &str, cmd: &str, topic: &str, payload: Option<Vec<u8>>) -> Event {
        let mut command = doc! {
            "version": "v1",
            "cmd": cmd,
            "topic": topic
        };
        if let Some(payload) = payload {
            command.insert(
                "payload",
                bson::Binary {
                    subtype: BinarySubtype::Generic,
                    bytes: payload,
                },
            );
        }

        let mut payload = Vec::new();
        command.to_writer(&mut payload).unwrap();

        Event::Publication(ReceivedPublication {
            topic_name: format!("$upstream/rpc/{}", id),
            dup: false,
            qos: QoS::AtLeastOnce,
            retain: false,
            payload: payload.into(),
        })
    }
}
