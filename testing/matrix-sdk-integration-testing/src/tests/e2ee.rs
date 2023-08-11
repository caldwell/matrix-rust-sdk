use std::{
    ops::Deref,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    config::SyncSettings,
    ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
    ruma::events::{
        room::message::{MessageType, RoomMessageEventContent, SyncRoomMessageEvent},
        AnySyncTimelineEvent,
    },
    Client,
};
use tracing::info;

use crate::helpers::get_client_for_user;

struct SyncAwareClient {
    client: Client,
    token: Mutex<Option<String>>,
}

impl SyncAwareClient {
    async fn sync_once(&self) -> Result<()> {
        let mut settings = SyncSettings::default().timeout(Duration::from_secs(3));

        let token = { self.token.lock().unwrap().clone() };
        if let Some(token) = token {
            settings = settings.token(token);
        }

        let response = self.client.sync_once(settings).await?;
        *self.token.lock().unwrap() = Some(response.next_batch);
        Ok(())
    }
}

impl Deref for SyncAwareClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

async fn get_sync_aware_client_for_user(username: String) -> Result<SyncAwareClient> {
    let client = get_client_for_user(username, true).await?;
    Ok(SyncAwareClient { client, token: None.into() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_encryption_missing_member_keys() -> Result<()> {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let alice = get_sync_aware_client_for_user(format!("alice{time}")).await?;
    let bob = get_sync_aware_client_for_user(format!("bob{time}")).await?;

    let invite = vec![bob.user_id().unwrap().to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;

    info!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;
    bob.get_room(alice_room.room_id()).unwrap().join().await?;

    info!("bob has joined");

    // New person joins the room.
    //let carl = get_sync_aware_client_for_user("carl".to_owned()).await?;
    //alice_room.invite_user_by_id(carl.user_id().unwrap()).await?;

    //carl.sync_once().await?;
    //carl.get_room(alice_room.room_id()).unwrap().join().await?;

    //info!("carl has joined");

    // Bob sends message WITHOUT syncing.
    info!("bob sends message...");
    let bob_room = bob.get_room(alice_room.room_id()).unwrap();
    //bob_room.mark_members_missing();
    bob_room.send(RoomMessageEventContent::text_plain("Hello world!"), None).await?;
    info!("bob is done sending the message");

    //bob.sync_once().await?;
    //alice.sync_once().await?;

    // All the other uses get to decrypt the message.
    for user in [alice] {
        //for user in [alice, carl] {
        info!("{} is looking for decrypted message", user.user_id().unwrap());

        let found_event = Arc::new(Mutex::new(false));

        let found_event_handler = found_event.clone();
        user.add_event_handler(move |event: SyncRoomMessageEvent| {
            info!("Found a message \\o/ {:?}", event);
            let found_event = found_event_handler.clone();
            async move {
                let MessageType::Text(text_content) = &event.as_original().unwrap().content.msgtype else { return };
                if text_content.body == "Hello world!" {
                    *found_event.lock().unwrap() = true;
                }
            }
        });

        user.add_event_handler(|event: AnySyncTimelineEvent| {
            info!("Found an event \\o/ {:?}", event);
            async {}
        });

        user.sync_once().await?;

        let found = *found_event.lock().unwrap();
        assert!(found, "event has not been found for {}", user.user_id().unwrap());
    }

    Ok(())
}
