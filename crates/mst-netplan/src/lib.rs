//! metastratum's network plane, as a pure function.
//!
//! [`compile`] turns the network model (VPCs, networks, NICs, security
//! groups, guardrail policies, floating IPs) into the OVN northbound rows
//! that implement it. [`plan`] compares those with what the northbound
//! database holds and produces one OVSDB transaction that converges it.
//! Neither does I/O, so both are tested here exhaustively; the controller
//! only loads the model, reads the rows and sends the transaction.
//!
//! Every row metastratum owns carries `external_ids:metastratum-key`, a stable
//! logical name. Rows without metastratum's keys are never touched.

mod compile;
mod model;
mod plan;
mod row;

pub use compile::{Desired, acl_match, compile, first_host, port_group_name, switch_name};
pub use model::*;
pub use plan::{ActualRow, Plan, plan};
pub use row::{Datum, Row, Table};
