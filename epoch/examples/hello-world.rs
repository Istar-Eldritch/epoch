use std::{collections::HashMap, pin::Pin, sync::Arc};

use epoch::prelude::*;
use epoch_mem::*;
use serde::Deserialize;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, EventData, Deserialize)]
enum ApplicationEvent {
    UserCreated { id: Uuid, name: String },
    UserNameUpdated { id: Uuid, name: String },
    UserDeleted { id: Uuid },
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct User {
    id: Uuid,
    name: String,
    version: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum UserProjectionError {
    #[error("Ther user with id {0} already exists")]
    UserAlreadyExists(Uuid),
    #[error("Ther user with id {0} does not exists")]
    UserDoesNotExist(Uuid),
    #[error("Cant hydrate user with event {0}")]
    UnexpectedEvent(String),
    #[error("Unexpected error projecting user: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error + Send + Sync>),
}

// In memory projection for tests
#[derive(Debug)]
struct UserProjection(Arc<Mutex<HashMap<Uuid, User>>>);

impl UserProjection {
    pub fn new() -> Self {
        UserProjection(Arc::new(Mutex::new(HashMap::new())))
    }
}

impl Projection<ApplicationEvent> for UserProjection {
    fn apply(
        &mut self,
        event: &Event<ApplicationEvent>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>
    {
        let event = event.clone();
        let users = self.0.clone();
        Box::pin(async move {
            if let Some(data) = event.data {
                match data {
                    ApplicationEvent::UserCreated { id, name } => {
                        let mut users = users.lock().await;
                        users.insert(
                            id.clone(),
                            User {
                                id,
                                name,
                                version: event.stream_version,
                            },
                        );
                        Ok(())
                    }
                    ApplicationEvent::UserNameUpdated { id, name } => {
                        let mut users = users.lock().await;
                        match users.get_mut(&id) {
                            Some(u) => {
                                u.name = name;
                                u.version = event.stream_version;
                                Ok(())
                            }
                            None => Err(UserProjectionError::UserDoesNotExist(id))?,
                        }
                    }
                    ApplicationEvent::UserDeleted { id } => {
                        let mut users = users.lock().await;
                        users.remove(&id);
                        Ok(())
                    }
                }
            } else {
                Ok(())
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let user_projection = UserProjection::new();
    let user_state = user_projection.0.clone();

    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    bus.subscribe(Box::new(user_projection)).await?;

    let event_store = InMemoryEventStore::new(bus);

    let user_id = Uuid::new_v4();
    let user_created_event = ApplicationEvent::UserCreated {
        id: user_id,
        name: "Debug Test".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .stream_version(0)
    .id(Uuid::new_v4())
    .build()?;

    let user_name_udpated_event = ApplicationEvent::UserNameUpdated {
        id: user_id,
        name: "Debug Testo".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .stream_version(1)
    .id(Uuid::new_v4())
    .build()?;

    let user_deleted_event = ApplicationEvent::UserDeleted { id: user_id }
        .into_builder()
        .stream_id(user_id)
        .stream_version(2)
        .id(Uuid::new_v4())
        .build()?;

    event_store.store_event(user_created_event).await?;

    println!("User in store: {:?}", user_state);

    event_store.store_event(user_name_udpated_event).await?;

    println!("User in store: {:?}", user_state);

    event_store.store_event(user_deleted_event).await?;

    println!("User in store after deletion: {:?}", user_state);

    Ok(())
}
