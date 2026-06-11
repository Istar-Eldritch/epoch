//! Migration 010: Strip `data` from the `epoch_notify_event()` NOTIFY payload.
//!
//! PostgreSQL truncates/errors on `pg_notify` payloads larger than 8 000 bytes.
//! Because the NOTIFY trigger fires inside the inserting transaction, an
//! oversized payload error rolls back the INSERT and the event is lost
//! (see CLOUD-155).
//!
//! In `DispatchMode::Async` the notification is only a wake signal: the listener
//! re-queries the database for committed events. The catch-up buffer listener
//! likewise only needs event identity (id + global_sequence) and fetches full
//! data from the database. This migration therefore replaces
//! `epoch_notify_event()` so the payload carries identity/sequencing metadata
//! only — never `data`, `purger_id`, `purged_at`, `causation_id`, or
//! `correlation_id`.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Replaces `epoch_notify_event()` to drop `data` from the NOTIFY payload.
pub struct StripDataFromNotifyPayload;

#[async_trait]
impl Migration for StripDataFromNotifyPayload {
    fn version(&self) -> i64 {
        10
    }

    fn name(&self) -> &'static str {
        "strip_data_from_notify_payload"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION epoch_notify_event()
            RETURNS TRIGGER AS $$
            BEGIN
                PERFORM pg_notify(
                    TG_ARGV[0],
                    json_build_object(
                        'id',              NEW.id,
                        'stream_id',       NEW.stream_id,
                        'stream_version',  NEW.stream_version,
                        'event_type',      NEW.event_type,
                        'actor_id',        NEW.actor_id,
                        'global_sequence', NEW.global_sequence,
                        'created_at',      NEW.created_at
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
