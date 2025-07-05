use std::convert::Infallible;

use epoch::prelude::*;
use epoch_core::{MemProjectionStore, ProjectionError, ProjectionStore, Projector, StoreProjector};
use epoch_mem::*;
use uuid::Uuid;

#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
#[subset_enum(UserSnapshotEvent, UserSnapshot)]
#[derive(Debug, Clone, serde::Serialize, EventData)]
enum ApplicationEvent {
    UserCreated { id: Uuid, name: String },
    UserNameUpdated { id: Uuid, name: String },
    UserSnapshot(User),
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct User {
    id: Uuid,
    name: String,
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
                e => Err(UserProjectionError::UnexpectedEvent(
                    e.event_type().to_string(),
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
                e => Err(UserProjectionError::UnexpectedEvent(
                    e.event_type().to_string(),
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
            },
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = MemEventStore::<ApplicationEvent>::new();

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

    // store.store_event(user_created_event).await?;

    let mut stream = store.read_events::<UserEvent>(user_id).await?;

    let user_store = MemProjectionStore::<User>::new();
    let store = user_store.clone();
    let user_projector = StoreProjector::new(user_store);

    user_projector.project(user_created_event).await?;

    println!("User in store: {:?}", store.fetch_by_id(&user_id).await?);

    user_projector.project(user_name_udpated_event).await?;

    println!("User in store: {:?}", store.fetch_by_id(&user_id).await?);

    // let user: User = Projector::project(&mut stream).await?.unwrap();

    // println!("Created: {:?}", user);

    // store.store_event(user_name_udpated_event).await?;

    // let user: User = Projector::project_on_snapshot(user, &mut stream).await?;

    // println!("Updated: {:?}", user);

    Ok(())
}
