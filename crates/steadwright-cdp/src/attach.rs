use std::collections::HashMap;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{CdpError, Connection, Event, Session};

pub type TargetId = String;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInfo {
    pub target_id: TargetId,
    pub session_id: Option<String>,
    pub type_: String,
    pub url: String,
    pub title: String,
    pub opener_id: Option<TargetId>,
    pub browser_context_id: Option<String>,
    pub parent_session_id: Option<String>,
}

pub async fn auto_attach(
    connection: &Connection,
    session: Option<&Session>,
) -> Result<(), CdpError> {
    let auto_attach = json!({
        "autoAttach": true,
        "waitForDebuggerOnStart": false,
        "flatten": true,
    });
    let discover = json!({"discover": true});
    if let Some(session) = session {
        session.send("Target.setAutoAttach", auto_attach).await?;
        session.send("Target.setDiscoverTargets", discover).await?;
    } else {
        connection
            .send("Target.setAutoAttach", auto_attach, None)
            .await?;
        connection
            .send("Target.setDiscoverTargets", discover, None)
            .await?;
    }
    Ok(())
}

pub struct TargetTracker {
    changes: watch::Receiver<HashMap<TargetId, TargetInfo>>,
}

impl TargetTracker {
    /// Starts tracking future target events.
    ///
    /// Construct the tracker before enabling target discovery/auto-attach so
    /// that the initial events cannot race the subscription.
    pub fn new(connection: &Connection) -> Self {
        let mut events = connection.events();
        let (changes_tx, changes) = watch::channel(HashMap::new());
        tokio::spawn(async move {
            let mut targets = HashMap::new();
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(_) => break,
                };
                if apply_event(&mut targets, event) {
                    changes_tx.send_replace(targets.clone());
                }
            }
        });
        Self { changes }
    }

    pub fn watch(&self) -> watch::Receiver<HashMap<TargetId, TargetInfo>> {
        self.changes.clone()
    }

    pub fn snapshot(&self) -> HashMap<TargetId, TargetInfo> {
        self.changes.borrow().clone()
    }
}

fn apply_event(targets: &mut HashMap<TargetId, TargetInfo>, event: Event) -> bool {
    match event.method.as_str() {
        "Target.attachedToTarget" => {
            let Some(target) = event.params.get("targetInfo") else {
                return false;
            };
            let Some(mut info) = parse_target_info(target) else {
                return false;
            };
            info.session_id = event
                .params
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            info.parent_session_id = event.session_id;
            targets.insert(info.target_id.clone(), info);
            true
        }
        "Target.detachedFromTarget" => {
            let target_id = event
                .params
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let session_id = event.params.get("sessionId").and_then(Value::as_str);
            let matched_id = target_id.or_else(|| {
                session_id.and_then(|session_id| {
                    targets
                        .iter()
                        .find(|(_, target)| target.session_id.as_deref() == Some(session_id))
                        .map(|(target_id, _)| target_id.clone())
                })
            });
            if let Some(target) = matched_id.and_then(|target_id| targets.get_mut(&target_id)) {
                target.session_id = None;
                target.parent_session_id = None;
                true
            } else {
                false
            }
        }
        "Target.targetCreated" | "Target.targetInfoChanged" => {
            let Some(value) = event.params.get("targetInfo") else {
                return false;
            };
            let Some(mut updated) = parse_target_info(value) else {
                return false;
            };
            if let Some(existing) = targets.get(&updated.target_id) {
                updated.session_id.clone_from(&existing.session_id);
                updated
                    .parent_session_id
                    .clone_from(&existing.parent_session_id);
            }
            targets.insert(updated.target_id.clone(), updated);
            true
        }
        "Target.targetDestroyed" => event
            .params
            .get("targetId")
            .and_then(Value::as_str)
            .is_some_and(|target_id| targets.remove(target_id).is_some()),
        _ => false,
    }
}

fn parse_target_info(value: &Value) -> Option<TargetInfo> {
    Some(TargetInfo {
        target_id: value.get("targetId")?.as_str()?.to_owned(),
        session_id: None,
        type_: value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        url: value
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        opener_id: value
            .get("openerId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        browser_context_id: value
            .get("browserContextId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parent_session_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_flattened_parent_and_session() {
        let mut targets = HashMap::new();
        assert!(apply_event(
            &mut targets,
            Event {
                method: "Target.attachedToTarget".into(),
                params: json!({
                    "sessionId": "child",
                    "targetInfo": {"targetId": "target", "type": "iframe", "url": "https://example.com", "title": "Example"}
                }),
                session_id: Some("parent".into()),
            }
        ));
        let target = &targets["target"];
        assert_eq!(target.session_id.as_deref(), Some("child"));
        assert_eq!(target.parent_session_id.as_deref(), Some("parent"));
    }
}
