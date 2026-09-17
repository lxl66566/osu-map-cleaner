//! osu-map-cleaner: filter osu! beatmap sets with an expression
//! and move the matched difficulties — or whole sets — to the
//! recycle bin.

pub mod clean;
pub mod db;
pub mod expr;
pub mod stars;
