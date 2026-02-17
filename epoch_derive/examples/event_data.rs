use epoch_core::event::EventData;
use epoch_derive::EventData as EventDataDerive;

#[derive(Clone, Debug, PartialEq, EventDataDerive, serde::Deserialize, serde::Serialize)]
pub enum AppEvent {
    RebootNow,
    RebootDelay(u32),
    UserCreated { id: String, name: String },
}

fn main() {
    let app_reboot_now = AppEvent::RebootNow;
    let app_reboot_delay = AppEvent::RebootDelay(5);
    let app_user_created = AppEvent::UserCreated {
        id: "one".to_string(),
        name: "root".to_string(),
    };

    assert_eq!(app_user_created.event_type(), "UserCreated");
    assert_eq!(app_reboot_delay.event_type(), "RebootDelay");
    assert_eq!(app_reboot_now.event_type(), "RebootNow");
}
