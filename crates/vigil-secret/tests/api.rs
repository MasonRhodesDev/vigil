#![forbid(unsafe_code)]

use std::borrow::Borrow;
use std::fmt::Display;
use std::hash::Hash;
use std::io::Read;
use std::ops::Deref;

use vigil_secret::Secret;

macro_rules! never_implements {
    ($($name:ident: $bound:path),+ $(,)?) => {$(
        #[allow(dead_code)]
        mod $name {
            use super::*;
            pub trait AmbiguousIfImplemented<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfImplemented<()> for T {}
            pub struct Implemented;
            impl<T: ?Sized + $bound> AmbiguousIfImplemented<Implemented> for T {}
            pub const CHECK: fn() = <Secret as AmbiguousIfImplemented<_>>::check;
        }
    )+};
}

never_implements!(
    deref: Deref,
    as_ref_str: AsRef<str>,
    as_ref_bytes: AsRef<[u8]>,
    borrow_str: Borrow<str>,
    borrow_bytes: Borrow<[u8]>,
    display: Display,
    clone: Clone,
    into_string: Into<String>,
    into_bytes: Into<Vec<u8>>,
    hash: Hash,
    read: Read,
    partial_eq_str: PartialEq<str>,
    partial_eq_ref_str: PartialEq<&'static str>,
    serialize: serde::Serialize,
);

#[test]
fn secret_offers_plaintext_only_through_expose() {
    for check in [
        deref::CHECK,
        as_ref_str::CHECK,
        as_ref_bytes::CHECK,
        borrow_str::CHECK,
        borrow_bytes::CHECK,
        display::CHECK,
        clone::CHECK,
        into_string::CHECK,
        into_bytes::CHECK,
        hash::CHECK,
        read::CHECK,
        partial_eq_str::CHECK,
        partial_eq_ref_str::CHECK,
        serialize::CHECK,
    ] {
        check();
    }
}
