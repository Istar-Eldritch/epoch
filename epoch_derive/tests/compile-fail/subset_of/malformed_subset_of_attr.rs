// Applying #[derive(SubsetOf)] with a malformed #[subset_of(...)] argument
// (a string literal instead of a path) must produce a clear compile error.

use epoch_derive::SubsetOf;

#[derive(SubsetOf)]
#[subset_of("not a path")]
enum BadAttrEnum {
    VariantA,
}

fn main() {}
