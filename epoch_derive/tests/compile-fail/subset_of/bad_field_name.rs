use epoch_derive::SubsetOf;

mod super_mod {
    use epoch_core::prelude::EventData;
    use epoch_derive::EventData as EventDataDerive;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Serialize, Deserialize, EventDataDerive)]
    pub enum AppEvent {
        X { name: String },
    }
}

#[derive(SubsetOf)]
#[subset_of(super_mod::AppEvent)]
enum BadSubset {
    X { wrong_field: String },
}

fn main() {}
