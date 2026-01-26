use async_trait::async_trait;
use epoch::prelude::*;
use epoch_mem::*;
use serde::Deserialize;
use uuid::Uuid;

#[subset_enum(UserEvent, UserCreated, UserNameUpdated, UserDeleted)]
#[subset_enum(ProductEvent, ProductCreated, ProductNameUpdated, ProductPriceUpdated)]
#[subset_enum(EmptyEvent)]
#[derive(Debug, Clone, serde::Serialize, EventData, Deserialize)]
pub enum ApplicationEvent {
    UserCreated { name: String },
    UserNameUpdated { name: String },
    UserDeleted,
    ProductCreated { name: String, price: f64 },
    ProductNameUpdated { name: String },
    ProductPriceUpdated { price: f64 },
}

#[subset_enum(UserCommand, CreateUser, UpdateUserName, DeleteUser)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ApplicationCommand {
    CreateUser { name: String },
    UpdateUserName { name: String },
    DeleteUser,
    CreateProduct { name: String, price: f64 },
    UpdateProductName { id: Uuid, name: String },
    UpdateProductPrice { id: Uuid, price: f64 },
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct User {
    id: Uuid,
    name: String,
    version: u64,
}

impl ProjectionState for User {
    fn get_id(&self) -> Uuid {
        self.id
    }
}

impl AggregateState for User {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

pub struct UserAggregate {
    state_store: InMemoryStateStore<User>,
    event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
}

impl UserAggregate {
    pub fn new(
        event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
        state_store: InMemoryStateStore<User>,
    ) -> Self {
        UserAggregate {
            state_store,
            event_store,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserProjectionError {
    #[error("No state present for id {0}")]
    NoState(Uuid),
}

impl Projection<ApplicationEvent> for UserAggregate {
    type State = User;
    type EventType = UserEvent;
    type StateStore = InMemoryStateStore<User>;
    type ProjectionError = UserProjectionError;

    fn subscriber_id(&self) -> &str {
        "projection:user-aggregate"
    }

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, UserProjectionError> {
        match event.data.as_ref().unwrap() {
            UserEvent::UserNameUpdated { name } => {
                if let Some(mut state) = state {
                    state.name = name.clone();
                    Ok(Some(state))
                } else {
                    Err(UserProjectionError::NoState(event.stream_id))
                }
            }
            UserEvent::UserCreated { name } => Ok(Some(User {
                id: event.stream_id,
                name: name.clone(),
                version: 0,
            })),
            UserEvent::UserDeleted => Ok(None),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserAggregateError {
    #[error("Error building event")]
    EventBuild(#[from] EventBuilderError),
}

#[async_trait]
impl Aggregate<ApplicationEvent> for UserAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type Command = UserCommand;
    type AggregateError = UserAggregateError;

    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> {
        match command.data {
            UserCommand::CreateUser { name } => {
                let event = ApplicationEvent::UserCreated { name: name.clone() }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
            UserCommand::UpdateUserName { name } => {
                if let Some(_state) = state {
                    let event = ApplicationEvent::UserNameUpdated { name }
                        .into_builder()
                        .stream_id(command.aggregate_id)
                        .build()?;
                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
            UserCommand::DeleteUser => {
                if let Some(state) = state {
                    let event = ApplicationEvent::UserDeleted
                        .into_builder()
                        .stream_id(state.id)
                        .build()?;
                    Ok(vec![event])
                } else {
                    Ok(vec![])
                }
            }
        }
    }
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
struct ProductProjection(InMemoryStateStore<Product>);

impl ProductProjection {
    pub fn new() -> Self {
        ProductProjection(InMemoryStateStore::new())
    }
}

impl ProjectionState for Product {
    fn get_id(&self) -> Uuid {
        self.id
    }
}

impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = ProductEvent;
    type ProjectionError = ProductProjectionError;

    fn subscriber_id(&self) -> &str {
        "projection:product"
    }

    fn get_state_store(&self) -> Self::StateStore {
        self.0.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ProjectionError> {
        match event.data.as_ref().unwrap() {
            ProductEvent::ProductCreated { name, price } => Ok(Some(Product {
                id: event.stream_id,
                name: name.clone(),
                price: *price,
                version: event.stream_version,
            })),
            ProductEvent::ProductNameUpdated { name } => {
                if let Some(mut state) = state {
                    state.name = name.clone();
                    Ok(Some(state))
                } else {
                    Err(ProductProjectionError::ProductDoesNotExist(event.stream_id))
                }
            }
            ProductEvent::ProductPriceUpdated { price } => {
                if let Some(mut state) = state {
                    state.price = *price;
                    Ok(Some(state))
                } else {
                    Err(ProductProjectionError::ProductDoesNotExist(event.stream_id))
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let product_projection = ProductProjection::new();
    let _product_state = product_projection.0.clone();
    env_logger::init();

    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    bus.subscribe(epoch::prelude::ProjectionHandler::new(product_projection))
        .await?;

    let event_store = InMemoryEventStore::new(bus);

    let user_state = InMemoryStateStore::new();
    let user_aggregate = UserAggregate::new(event_store.clone(), user_state.clone());

    let user_id = Uuid::new_v4();
    let create_user = ApplicationCommand::CreateUser {
        name: "Debug Test".to_string(),
    };

    let update_user_name = ApplicationCommand::UpdateUserName {
        name: "Debug Testo".to_string(),
    };

    let delete_user = ApplicationCommand::DeleteUser;

    user_aggregate
        .handle(Command::new(user_id, create_user, None, None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle(Command::new(user_id, update_user_name, None, None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle(Command::new(user_id, delete_user, None, None))
        .await?;

    println!("User in store after deletion: {:?}", user_state);

    println!("Event store: {:?}", event_store);

    Ok(())
}
