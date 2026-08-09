use anyhow::{Result, anyhow};

use crate::interactive_commands::CollabInvocation;
use crate::modes::collab_service::{CollabRoomInfo, CollabService};

/// Shared host-side collaboration command context. It is a clone of the one
/// listener-owned room registry; interactive surfaces never create a second
/// service or bypass listener transport policy.
#[derive(Clone)]
pub struct CollabHost {
    service: CollabService,
    base_url: String,
}

impl CollabHost {
    #[must_use]
    pub fn new(service: CollabService, base_url: String) -> Self {
        Self { service, base_url }
    }

    pub async fn execute(&self, invocation: CollabInvocation) -> Result<String> {
        match invocation {
            CollabInvocation::Start => {
                let room = self.service.start_default(&self.base_url).await?;
                Ok(format!(
                    "Collaboration room {}\nControl link: {}\nView-only link: {}",
                    room.room_id, room.control_link, room.view_link
                ))
            }
            CollabInvocation::Status => match self.service.default_status().await {
                Some(room) => Ok(format_room_status(&room)),
                None => Ok("No collaboration room is running".to_owned()),
            },
            CollabInvocation::Stop => {
                let room = self.service.stop_default().await?;
                Ok(format!("Stopped collaboration room {}", room.room_id))
            }
        }
    }
}

pub async fn execute(
    host: Option<&CollabHost>,
    invocation: CollabInvocation,
) -> Result<String> {
    let host = host.ok_or_else(|| anyhow!("/collab requires --listen"))?;
    host.execute(invocation).await
}

fn format_room_status(room: &CollabRoomInfo) -> String {
    format!(
        "Collaboration room {} · {} participants ({} control, {} view-only) · limit {}",
        room.room_id,
        room.participants,
        room.control_participants,
        room.view_participants,
        room.participant_limit
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_listener_is_actionable_and_secret_free() {
        let error = execute(None, CollabInvocation::Start)
            .await
            .expect_err("listen required");
        assert_eq!(error.to_string(), "/collab requires --listen");
    }

    #[test]
    fn status_format_contains_counts_not_capabilities() {
        let text = format_room_status(&CollabRoomInfo {
            room_id: "room-1".to_owned(),
            session_id: "session-1".to_owned(),
            participants: 3,
            control_participants: 1,
            view_participants: 2,
            participant_limit: 8,
            running: true,
        });
        assert_eq!(
            text,
            "Collaboration room room-1 · 3 participants (1 control, 2 view-only) · limit 8"
        );
        assert!(!text.contains("#c=") && !text.contains("#v="));
    }
}
