//! Context pipeline: request -> strategy -> segments -> compressed, validated context.
//!
//! Each stage is a separate module and can be measured on its own: `route` decides what to
//! fetch, `segment` splits it, `compress` selects, `validate` proves nothing important was
//! lost, and `distribute` runs the whole thing with a cache in front.

pub mod compress;
pub mod distribute;
pub mod route;
pub mod segment;
pub mod validate;
