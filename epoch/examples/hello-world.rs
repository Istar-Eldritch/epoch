use std::{convert::Infallible, sync::Arc};

use epoch::prelude::*;
use epoch_mem::*;
use uuid::Uuid;

#[subset_enum(UserEvent, UserCreated, UserNameUpdated, UserDeleted)]
#[subset_enum(UserActivityEvent, UserCreated, UserDeleted)]
#[derive(Debug, Clone, serde::Serialize, EventData)]
enum ApplicationEvent {
    UserCreated { id: Uuid, name: String },
    UserNameUpdated { id: Uuid, name: String },
    UserEmailUpdated { id: Uuid, email: String },
    UserDeleted { id: Uuid },
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct User {
    id: Uuid,
    name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct UserActivity {
    active_user_ids: std::collections::HashSet<Uuid>,
    created_count: u64,
    deleted_count: u64,
}

impl UserActivity {
    const ID: Uuid = uuid::uuid!("51a2160a-f4d6-48a2-a171-4d38109f7963");
}

#[derive(thiserror::Error, Debug)]
pub enum UserActivityProjectionError {
    #[error("Cant hydrate user activity with event {0}")]
    UnexpectedEvent(String),
    #[error("Unexpected error projecting user activity: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("The event has no data attached to it")]
    NoData,
    #[error("This error should never happen: {0}")]
    Infallible(#[from] Infallible),
}

#[derive(thiserror::Error, Debug)]
pub enum UserProjectionError {
    #[error("Cant hydrate user with event {0}")]
    UnexpectedEvent(String),
    #[error("Unexpected error projecting user: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("The event has no data attached to it")]
    NoData,
    #[error("This error should never happen: {0}")]
    Infallible(#[from] Infallible),
}

impl ProjectionError for UserProjectionError {}

impl Projection for User {
    type Error = UserProjectionError;
    type EventType = UserEvent;

    fn apply(self, event: Event<Self::EventType>) -> Result<Self, Self::Error> {
        if let Some(data) = &event.data {
            match data {
                UserEvent::UserNameUpdated { id: _, name } => Ok(User {
                    name: name.clone(),
                    ..self
                }),
                UserEvent::UserDeleted { id: _ } => Err(UserProjectionError::UnexpectedEvent(
                    "UserDeleted should not be projected to a user".to_string(),
                )),
                _ => Err(UserProjectionError::UnexpectedEvent(
                    "Unexpected event for User projection".to_string(),
                )),
            }
        } else {
            Err(UserProjectionError::NoData)
        }
    }
    fn new(event: Event<Self::EventType>) -> Result<Self, Self::Error> {
        if let Some(data) = &event.data {
            match data {
                UserEvent::UserCreated { id, name } => Ok(User {
                    name: name.clone(),
                    id: id.clone(),
                }),
                _ => Err(UserProjectionError::UnexpectedEvent(
                    "Unexpected event for User projection".to_string(),
                )),
            }
        } else {
            Err(UserProjectionError::NoData)
        }
    }
    fn get_id(&self) -> &Uuid {
        &self.id
    }
    fn get_id_from_event(event: &Event<UserEvent>) -> Option<Uuid> {
        match &event.data {
            Some(d) => match d {
                UserEvent::UserCreated { id, name: _ } => Some(id.clone()),
                UserEvent::UserNameUpdated { id, name: _ } => Some(id.clone()),
                UserEvent::UserDeleted { id } => Some(id.clone()),
            },
            _ => None,
        }
    }
}

impl ProjectionError for UserActivityProjectionError {}

impl Projection for UserActivity {
    type Error = UserActivityProjectionError;
    type EventType = UserActivityEvent;

    fn apply(mut self, event: Event<Self::EventType>) -> Result<Self, Self::Error> {
        if let Some(data) = &event.data {
            match data {
                UserActivityEvent::UserCreated { id, name: _ } => {
                    self.active_user_ids.insert(id.clone());
                    self.created_count += 1;
                    Ok(self)
                }
                UserActivityEvent::UserDeleted { id } => {
                    self.active_user_ids.remove(id);
                    self.deleted_count += 1;
                    Ok(self)
                }
            }
        } else {
            Err(UserActivityProjectionError::NoData)
        }
    }

    fn new(event: Event<Self::EventType>) -> Result<Self, Self::Error> {
        if let Some(data) = &event.data {
            match data {
                UserActivityEvent::UserCreated { id, name: _ } => {
                    let mut active_user_ids = std::collections::HashSet::new();
                    active_user_ids.insert(id.clone());
                    Ok(UserActivity {
                        active_user_ids,
                        created_count: 1,
                        deleted_count: 0,
                    })
                }
                e => Err(UserActivityProjectionError::UnexpectedEvent(
                    e.event_type().to_string(),
                )),
            }
        } else {
            Err(UserActivityProjectionError::NoData)
        }
    }

    fn get_id(&self) -> &Uuid {
        &Self::ID
    }

    fn get_id_from_event(event: &Event<UserActivityEvent>) -> Option<Uuid> {
        match &event.data {
            Some(d) => match d {
                UserActivityEvent::UserCreated { id, name: _ } => Some(id.clone()),
                UserActivityEvent::UserDeleted { id } => Some(id.clone()),
            },
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    let event_store = InMemoryEventStore::new(bus);
    let user_store = MemProjectionStore::<User>::new();
    let user_projector = StoreProjector::new(user_store.clone());

    event_store
        .bus()
        .subscribe(Arc::new(user_projector))
        .await?;

    let user_id = Uuid::new_v4();
    let user_created_event: Event<UserEvent> = UserEvent::UserCreated {
        id: user_id,
        name: "Debug Test".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .id(Uuid::new_v4())
    .build()?;

    let user_name_udpated_event: Event<UserEvent> = UserEvent::UserNameUpdated {
        id: user_id,
        name: "Debug Testo".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .id(Uuid::new_v4())
    .build()?;

    let user_deleted_event: Event<UserEvent> = UserEvent::UserDeleted { id: user_id }
        .into_builder()
        .stream_id(user_id)
        .id(Uuid::new_v4())
        .build()?;

    event_store.store_event(user_created_event).await?;

    println!(
        "User in store: {:?}",
        user_store.fetch_by_id(&user_id).await?
    );
    // println!(
    //     "UserActivity after creation: {:?}",
    //     user_activity_store.fetch_by_id(&Uuid::nil()).await?
    // );

    event_store.store_event(user_name_udpated_event).await?;

    println!(
        "User in store: {:?}",
        user_store.fetch_by_id(&user_id).await?
    );
    // println!(
    //     "UserActivity after name update: {:?}",
    //     user_activity_store.fetch_by_id(&Uuid::nil()).await?
    // );

    event_store.store_event(user_deleted_event).await?;

    println!(
        "User in store after deletion: {:?}",
        user_store.fetch_by_id(&user_id).await?
    );
    // println!(
    //     "UserActivity after deletion: {:?}",
    //     user_activity_store.fetch_by_id(&Uuid::nil()).await?
    // );

    Ok(())
}
