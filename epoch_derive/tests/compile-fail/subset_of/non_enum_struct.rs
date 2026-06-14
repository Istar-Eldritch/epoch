// Applying #[derive(SubsetOf)] to a struct must produce a clear compile error.

use epoch_derive::SubsetOf;

#[derive(SubsetOf)]
#[subset_of(SomeSuper)]
struct NotAnEnum {
    field: i32,
}

fn main() {}
