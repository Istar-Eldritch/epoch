use epoch_derive::SubsetOf;

#[derive(SubsetOf)]
#[subset_of(some::path::AppEvent)]
union BadUnion {
    a: u32,
    b: f32,
}

fn main() {}
