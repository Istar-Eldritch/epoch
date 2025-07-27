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
    CreateUser { id: Uuid, name: String },
    UpdateUserName { id: Uuid, name: String },
    DeleteUser { id: Uuid },
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

impl AggregateState for User {
    fn get_id(&self) -> Uuid {
        self.id
    }

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

#[async_trait]
impl Aggregate for UserAggregate {
    type State = User;
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type CreateCommand = CreateUserCommand;
    type UpdateCommand = UpdateUserNameCommand;
    type DeleteCommand = DeleteUserCommand;
    type EventData = ApplicationEvent;
    type CreateEvent = UserCreationEvent;
    type UpdateEvent = UserUpdatedEvent;
    type DeleteEvent = UserDeletionEvent;
    type EventStore = InMemoryEventStore<InMemoryEventBus<Self::EventData>>;
    type StateStore = InMemoryStateStore<Self::State>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    async fn handle_create_command(
        &self,
        command: Command<Self::CreateCommand, Self::CommandCredentials>,
    ) -> Result<
        (Self::State, Vec<Event<Self::CreateEvent>>),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        match command.data {
            CreateUserCommand::CreateUser { id, name } => {
                let user = User {
                    id,
                    name,
                    version: 0,
                };
                let event = UserCreationEvent::UserCreated {
                    name: user.name.clone(),
                }
                .into_builder()
                .stream_id(user.id)
                .stream_version(user.version)
                .build()?;
                Ok((user, vec![event]))
            }
        }
    }

    async fn handle_update_command(
        &self,
        mut state: Self::State,
        command: Command<Self::UpdateCommand, Self::CommandCredentials>,
    ) -> Result<
        (Self::State, Vec<Event<Self::UpdateEvent>>),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        match command.data {
            UpdateUserNameCommand::UpdateUserName { id: _, name } => {
                state.name = name;
                state.version += 1;
                let event = UserUpdatedEvent::UserNameUpdated {
                    name: state.name.clone(),
                }
                .into_builder()
                .stream_id(state.id)
                .stream_version(state.version)
                .build()?;
                Ok((state, vec![event]))
            }
        }
    }

    async fn handle_delete_command(
        &self,
        state: Self::State,
        _command: Command<Self::DeleteCommand, Self::CommandCredentials>,
    ) -> Result<
        (Option<Self::State>, Vec<Event<Self::DeleteEvent>>),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let event = UserDeletionEvent::UserDeleted
            .into_builder()
            .stream_id(state.id)
            .stream_version(state.version + 1)
            .build()?;
        Ok((None, vec![event]))
    }

    fn get_id_from_command(&self, command: &Self::CommandData) -> Uuid {
        match command {
            ApplicationCommand::CreateUser { id, .. } => *id, // This should be handled in handle_create_command
            ApplicationCommand::UpdateUserName { id, .. } => *id,
            ApplicationCommand::DeleteUser { id } => *id,
            _ => panic!("Unexpected command type"),
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

impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type StateStore = InMemoryStateStore<Self::State>;
    type CreateEvent = ProductCreationEvent;
    type UpdateEvent = ProductUpdateEvent;
    type DeleteEvent = EmptyEvent;

    fn get_storage(&self) -> Self::StateStore {
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
        id: user_id,
        name: "Debug Test".to_string(),
    };

    let update_user_name = ApplicationCommand::UpdateUserName {
        id: user_id,
        name: "Debug Testo".to_string(),
    };

    let delete_user = ApplicationCommand::DeleteUser { id: user_id };

    user_aggregate
        .handle_command(Command::new(create_user, (), None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle_command(Command::new(update_user_name, (), None))
        .await?;

    println!("User in store: {:?}", user_state);

    user_aggregate
        .handle_command(Command::new(delete_user, (), None))
        .await?;

    println!("User in store after deletion: {:?}", user_state);

    println!("Event store: {:?}", event_store);

    Ok(())
}
