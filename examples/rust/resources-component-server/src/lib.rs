mod bindings {
    use crate::Handler;

    wit_bindgen::generate!({
       with: {
           "wrpc-examples:resources/resources": generate
       }
    });

    export!(Handler);
}

use bindings::exports::wrpc_examples::resources::resources::{FooBorrow, GuestFoo};

pub struct Handler;

pub struct Foo;

impl bindings::exports::wrpc_examples::resources::resources::GuestFoo for Foo {
    fn new() -> Self {
        Self
    }

    fn foo(_: bindings::exports::wrpc_examples::resources::resources::Foo) -> String {
        "foo".to_string()
    }

    fn bar(&self) -> String {
        "bar".to_string()
    }
}

impl bindings::exports::wrpc_examples::resources::resources::Guest for Handler {
    type Foo = Foo;

    fn bar(v: FooBorrow<'_>) -> String {
        let v: &Foo = v.get();
        v.bar()
    }
}
