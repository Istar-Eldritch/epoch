# epoch_pg

`epoch_pg` is a library that provides PostgreSQL implementations for the `EventStoreBackend` and `EventBus` traits from the `epoch_core` crate. It leverages `sqlx` for efficient database interactions and `tokio` for asynchronous operations.


## Running the tests

You must start the docker container defined in `docker-compose.yml` before running the tests.

The tests do not run well in parallel, so you must add the `--test-threads=1` to the cargo command:

```bash
cargo test -- --test-threads=1
```

