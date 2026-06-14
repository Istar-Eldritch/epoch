use epoch_core::event::EnumConversionError;
use epoch_derive::subset_enum;

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
    let app_new_session = AppEvent::NewSession {
        user_id: "one".to_string(),
    };
    assert_eq!(AppEvent::from(user_created), app_user_created);
    assert!(UserEvent::try_from(app_new_session).is_err());
    assert!(UserEvent::try_from(app_user_created).is_ok());
}
