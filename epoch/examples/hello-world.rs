use std::marker::PhantomData;

use epoch::prelude::*;
use epoch_mem::*;
use serde::Deserialize;
use uuid::Uuid;

#[subset_enum(UserCreationEvent, UserCreated)]
#[subset_enum(UserUpdatedEvent, UserNameUpdated)]
#[subset_enum(UserDeletionEvent, UserDeleted)]
#[subset_enum(ProductCreationEvent, ProductCreated)]
#[subset_enum(ProductUpdateEvent, ProductNameUpdated, ProductPriceUpdated)]
#[subset_enum(EmptyEvent)]
#[derive(Debug, Clone, serde::Serialize, EventData, Deserialize)]
enum ApplicationEvent {
    UserCreated { name: String },
    UserNameUpdated { name: String },
    UserDeleted,
    ProductCreated { name: String, price: f64 },
    ProductNameUpdated { name: String },
    ProductPriceUpdated { price: f64 },
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct User {
    id: Uuid,
    name: String,
    version: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
struct Product {
    id: Uuid,
    name: String,
    price: f64,
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

#[derive(Debug, Clone)]
struct UserProjector<ED>
where
    ED: EventData,
{
    _phantom: PhantomData<ED>,
    storage: InMemoryStateStorage<User>,
}

impl<ED> UserProjector<ED>
where
    ED: EventData,
{
    pub fn new() -> Self {
        UserProjector {
            _phantom: PhantomData,
            storage: InMemoryStateStorage::new(),
        }
    }
}

impl Projection<ApplicationEvent> for UserProjector<ApplicationEvent> {
    type State = User;
    type StateStorage = InMemoryStateStorage<Self::State>;
    type CreateEvent = UserCreationEvent;
    type UpdateEvent = UserUpdatedEvent;
    type DeleteEvent = UserDeletionEvent;
    fn get_storage(&self) -> Self::StateStorage {
        self.storage.clone()
    }
    fn apply_create(
        &self,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap() {
            UserCreationEvent::UserCreated { name } => Ok(User {
                id: event.stream_id,
                name: name.clone(),
                version: 0,
            }),
        }
    }
    fn apply_update(
        &self,
        mut state: Self::State,
        event: &Event<Self::UpdateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap() {
            UserUpdatedEvent::UserNameUpdated { name } => {
                state.name = name.clone();
                Ok(state)
            }
        }
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ProductProjectionError {
    #[error("The product with id {0} already exists")]
    ProductAlreadyExists(Uuid),
    #[error("The product with id {0} does not exist")]
    ProductDoesNotExist(Uuid),
    #[error("Cannot hydrate product with event {0}")]
    UnexpectedEvent(String),
    #[error("Unexpected error projecting product: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug)]
struct ProductProjection(InMemoryStateStorage<Product>);

impl ProductProjection {
    pub fn new() -> Self {
        ProductProjection(InMemoryStateStorage::new())
    }
}

impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type StateStorage = InMemoryStateStorage<Self::State>;
    type CreateEvent = ProductCreationEvent;
    type UpdateEvent = ProductUpdateEvent;
    type DeleteEvent = EmptyEvent;

    fn get_storage(&self) -> Self::StateStorage {
        self.0.clone()
    }

    fn apply_create(
        &self,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap().clone() {
            ProductCreationEvent::ProductCreated { name, price } => Ok(Product {
                id: event.stream_id,
                name,
                price,
                version: event.stream_version,
            }),
        }
    }
    fn apply_update(
        &self,
        mut state: Self::State,
        event: &Event<Self::UpdateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap().clone() {
            ProductUpdateEvent::ProductNameUpdated { name } => {
                state.name = name;
                Ok(state)
            }
            ProductUpdateEvent::ProductPriceUpdated { price } => {
                state.price = price;
                Ok(state)
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let user_projection = UserProjector::new();
    let user_state = user_projection.get_storage().clone();

    let product_projection = ProductProjection::new();
    let _product_state = product_projection.0.clone();

    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    bus.subscribe(user_projection).await?;
    bus.subscribe(product_projection).await?;

    let event_store = InMemoryEventStore::new(bus);

    let user_id = Uuid::new_v4();
    let user_created_event = ApplicationEvent::UserCreated {
        name: "Debug Test".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .stream_version(0)
    .id(Uuid::new_v4())
    .build()?;

    let user_name_udpated_event = ApplicationEvent::UserNameUpdated {
        name: "Debug Testo".to_string(),
    }
    .into_builder()
    .stream_id(user_id)
    .stream_version(1)
    .id(Uuid::new_v4())
    .build()?;

    let user_deleted_event = ApplicationEvent::UserDeleted
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
