// Applying #[derive(SubsetOf)] without the #[subset_of(...)] attribute
// must produce a clear compile error.

use epoch_derive::SubsetOf;

#[derive(SubsetOf)]
enum NoAttrEnum {
    VariantA,
}

fn main() {}
