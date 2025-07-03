use epoch::prelude::*;
use epoch_mem_store::*;
use uuid::Uuid;

#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
#[derive(Debug, Clone, serde::Serialize, EventData)]
enum ApplicationEvent {
    UserCreated { id: Uuid, name: String },
    UserNameUpdated { id: Uuid, name: String },
}

#[derive(Debug)]
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
    Unexpected(#[from] Box<dyn std::error::Error>),
    #[error("The event has no data attached to it")]
    NoData,
}

impl Projection<UserEvent> for User {
    type ProjectionError = UserProjectionError;
    fn apply(self, event: &Event<UserEvent>) -> Result<Self, Self::ProjectionError> {
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
    fn new(event: &Event<UserEvent>) -> Result<Self, Self::ProjectionError> {
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
    .sequence_number(0)
    .id(Uuid::new_v4())
    .build()?;

    let user_name_udpated_event: Event<UserEvent> = UserEvent::UserNameUpdated {
        id: user_id,
        name: "Debug Testo".to_string(),
    }
    .into_builder()
    .sequence_number(0)
    .id(Uuid::new_v4())
    .build()?;

    let mut stream = store.fetch_stream::<UserEvent>(user_id).await?;

    stream.append_to_stream(&vec![user_created_event]).await?;

    let user: User = Projector::project(&mut stream).await?.unwrap();

    println!("Created: {:?}", user);

    stream
        .append_to_stream(&vec![user_name_udpated_event])
        .await?;

    let user: User = Projector::project_on_snapshot(user, &mut stream).await?;

    println!("Updated: {:?}", user);

    Ok(())
}
