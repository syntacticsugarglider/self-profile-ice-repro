use core::convert::Infallible as Void;
use core::pin::Pin;
use futures::task::Spawn;
use futures::{Sink, Stream};
use protocol::protocol;
use protocol_mve_transport::Transport;
use serde::{Deserialize, Serialize};

#[protocol]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Id([u8; 32]);

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Item {
    // pub generics: Generics,
    pub content: Type,
}

#[protocol]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum TypePosition {
    Generic(u32),
    Concrete(Id),
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Generics {
    pub base: [u8; 32],
    pub parameters: Vec<(Option<String>, Id)>,
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Binding {
    pub name: Option<String>,
    pub ty: TypePosition,
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Product {
    pub bindings: Vec<Binding>,
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Variant {
    pub name: Option<String>,
    pub ty: Product,
}

#[protocol]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum Receiver {
    Move,
    Mut,
    Ref,
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Method {
    pub receiver: Receiver,
    pub arguments: Vec<TypePosition>,
    pub ret: Option<TypePosition>,
}

#[protocol]
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum Capture {
    Once,
    Mut,
    Ref,
}

#[protocol]
#[derive(Debug, Clone, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub enum Type {
    Sum(Vec<Variant>),
    Product(Product),
    Opaque,
    Function {
        capture: Capture,
        arguments: Vec<TypePosition>,
        ret: Option<TypePosition>,
    },
    Object {
        methods: Vec<Method>,
    },
}

pub trait CloneSpawn: Spawn + Send {
    fn box_clone(&self) -> Box<dyn CloneSpawn>;
}

impl Clone for Box<dyn CloneSpawn> {
    fn clone(&self) -> Self {
        self.box_clone()
    }
}

struct Tester
where
    Item: protocol::Unravel<Transport<Box<dyn CloneSpawn>, Void, Void, Item>>;

fn main() {
    println!("Hello, world!");
}
