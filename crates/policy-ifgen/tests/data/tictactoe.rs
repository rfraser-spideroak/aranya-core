//! Code generated by `policy-ifgen`. DO NOT EDIT.
#![allow(clippy::enum_variant_names)]
#![allow(missing_docs)]
#![allow(non_snake_case)]
#![allow(unused_imports)]
extern crate alloc;
use alloc::{string::String, vec::Vec};
use policy_ifgen::{
    macros::{actions, effect, effects, value},
    ClientError, Id, Value,
};
/// Enum of policy effects that can occur in response to a policy action.
#[effects]
pub enum Effect {
    GameStart(GameStart),
    GameUpdate(GameUpdate),
    GameOver(GameOver),
}
/// GameStart policy effect.
#[effect]
pub struct GameStart {
    pub gameID: Id,
    pub x: Id,
    pub o: Id,
}
/// GameUpdate policy effect.
#[effect]
pub struct GameUpdate {
    pub gameID: Id,
    pub player: Id,
    pub p: String,
    pub X: i64,
    pub Y: i64,
}
/// GameOver policy effect.
#[effect]
pub struct GameOver {
    pub gameID: Id,
    pub winner: Id,
    pub p: String,
}
/// Implements all supported policy actions.
#[actions]
pub trait ActorExt {
    fn StartGame(&mut self, profileX: Id, profileO: Id) -> Result<(), ClientError>;
    fn MakeMove(&mut self, gameID: Id, x: i64, y: i64) -> Result<(), ClientError>;
}
