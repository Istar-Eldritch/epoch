use epoch_derives::subset_enum;

#[derive(Debug, PartialEq)]
#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
pub enum AppEvent {
    UserCreated { id: String, name: String, age: u8 },
    UserNameUpdated { id: String, name: String },
    NewSession { user_id: String },
}

fn main() {
    let user_created = UserEvent::UserCreated {
        id: "one".to_string(),
        name: "buff".to_string(),
        age: 2,
    };
    let app_user_created = AppEvent::UserCreated {
        id: "one".to_string(),
        name: "buff".to_string(),
        age: 2,
    };
    assert_eq!(AppEvent::from(user_created), app_user_created);
}
