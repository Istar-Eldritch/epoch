use async_trait::async_trait;
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
pub enum ApplicationEvent {
    UserCreated { name: String },
    UserNameUpdated { name: String },
    UserDeleted,
    ProductCreated { name: String, price: f64 },
    ProductNameUpdated { name: String },
    ProductPriceUpdated { price: f64 },
}

#[subset_enum(CreateUserCommand, CreateUser)]
#[subset_enum(UpdateUserNameCommand, UpdateUserName)]
#[subset_enum(DeleteUserCommand, DeleteUser)]
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

impl Projection<ApplicationEvent> for UserAggregate {
    type State = User;
    type CreateEvent = UserCreationEvent;
    type UpdateEvent = UserUpdatedEvent;
    type DeleteEvent = UserDeletionEvent;
    type StateStore = InMemoryStateStore<User>;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply_create(
        &self,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap().clone() {
            UserCreationEvent::UserCreated { name } => Ok(User {
                id: event.stream_id,
                name,
                version: 0,
            }),
        }
    }
    fn apply_update(
        &self,
        mut state: Self::State,
        event: &Event<Self::UpdateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        match event.data.as_ref().unwrap().clone() {
            UserUpdatedEvent::UserNameUpdated { name } => {
                state.name = name;
                state.version += 1;
                Ok(state)
            }
        }
    }
}

#[async_trait]
impl Aggregate<ApplicationEvent> for UserAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type CreateCommand = CreateUserCommand;
    type UpdateCommand = UpdateUserNameCommand;
    type DeleteCommand = DeleteUserCommand;

    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_create_command(
        &self,
        command: Command<Self::CreateCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Box<dyn std::error::Error + Send + Sync>> {
        match command.data {
            CreateUserCommand::CreateUser { name } => {
                let event = ApplicationEvent::UserCreated { name: name.clone() }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .stream_version(0)
                    .build()?;
                Ok(vec![event])
            }
        }
    }

    async fn handle_update_command(
        &self,
        state: &Self::State,
        command: Command<Self::UpdateCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Box<dyn std::error::Error + Send + Sync>> {
        match command.data {
            UpdateUserNameCommand::UpdateUserName { name } => {
                let event = ApplicationEvent::UserNameUpdated { name }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .stream_version(state.version + 1)
                    .build()?;
                Ok(vec![event])
            }
        }
    }

    async fn handle_delete_command(
        &self,
        state: &Self::State,
        _command: Command<Self::DeleteCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Box<dyn std::error::Error + Send + Sync>> {
        let event = ApplicationEvent::UserDeleted
            .into_builder()
            .stream_id(state.id)
            .stream_version(state.version + 1)
            .build()?;
        Ok(vec![event])
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
    type CreateEvent = ProductCreationEvent;
    type UpdateEvent = ProductUpdateEvent;
    type DeleteEvent = EmptyEvent;

    fn get_state_store(&self) -> Self::StateStore {
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
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let product_projection = ProductProjection::new();
    let _product_state = product_projection.0.clone();

    let bus: InMemoryEventBus<ApplicationEvent> = InMemoryEventBus::new();
    bus.subscribe(product_projection).await?;

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
        .handle_command(Command::new(user_id, create_user, None, None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle_command(Command::new(user_id, update_user_name, None, None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle_command(Command::new(user_id, delete_user, None, None))
        .await?;

    println!("User in store after deletion: {:?}", user_state);

    println!("Event store: {:?}", event_store);

    Ok(())
}
