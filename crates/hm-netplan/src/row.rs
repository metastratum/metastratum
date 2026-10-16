//! Desired northbound rows.

use std::collections::BTreeMap;

/// The northbound tables hypermon writes, and the columns it manages in each.
/// Other columns (status that ovn-northd fills in, options hypermon doesn't
/// use) are left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Table {
    LogicalSwitch,
    LogicalSwitchPort,
    LogicalRouter,
    LogicalRouterPort,
    GatewayChassis,
    StaticRoute,
    Nat,
    DhcpOptions,
    PortGroup,
    Acl,
}

impl Table {
    pub const ALL: [Table; 10] = [
        Table::LogicalSwitch,
        Table::LogicalSwitchPort,
        Table::LogicalRouter,
        Table::LogicalRouterPort,
        Table::GatewayChassis,
        Table::StaticRoute,
        Table::Nat,
        Table::DhcpOptions,
        Table::PortGroup,
        Table::Acl,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Table::LogicalSwitch => "Logical_Switch",
            Table::LogicalSwitchPort => "Logical_Switch_Port",
            Table::LogicalRouter => "Logical_Router",
            Table::LogicalRouterPort => "Logical_Router_Port",
            Table::GatewayChassis => "Gateway_Chassis",
            Table::StaticRoute => "Logical_Router_Static_Route",
            Table::Nat => "NAT",
            Table::DhcpOptions => "DHCP_Options",
            Table::PortGroup => "Port_Group",
            Table::Acl => "ACL",
        }
    }

    /// Root rows live until deleted; the others live while referenced.
    pub fn is_root(self) -> bool {
        matches!(
            self,
            Table::LogicalSwitch | Table::LogicalRouter | Table::DhcpOptions | Table::PortGroup
        )
    }

    /// Whether rows have a unique `name`, by which rows from before
    /// `hypermon-key` existed are adopted.
    pub fn has_name(self) -> bool {
        matches!(
            self,
            Table::LogicalSwitch
                | Table::LogicalSwitchPort
                | Table::LogicalRouter
                | Table::LogicalRouterPort
                | Table::GatewayChassis
                | Table::PortGroup
        )
    }

    /// Columns hypermon sets; everything a comparison needs.
    pub fn columns(self) -> &'static [&'static str] {
        match self {
            Table::LogicalSwitch => &["name", "ports", "external_ids"],
            Table::LogicalSwitchPort => &[
                "name",
                "type",
                "addresses",
                "port_security",
                "options",
                "dhcpv4_options",
                "external_ids",
            ],
            Table::LogicalRouter => &["name", "ports", "nat", "static_routes", "external_ids"],
            Table::LogicalRouterPort => {
                &["name", "mac", "networks", "gateway_chassis", "external_ids"]
            }
            Table::GatewayChassis => &["name", "chassis_name", "priority", "external_ids"],
            Table::StaticRoute => &["ip_prefix", "nexthop", "external_ids"],
            Table::Nat => &["type", "external_ip", "logical_ip", "external_ids"],
            Table::DhcpOptions => &["cidr", "options", "external_ids"],
            Table::PortGroup => &["name", "ports", "acls", "external_ids"],
            Table::Acl => &["direction", "priority", "match", "action", "external_ids"],
        }
    }
}

/// A column value. References name other desired rows by key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Datum {
    Str(String),
    Int(i64),
    /// A set of strings.
    Strs(Vec<String>),
    Map(BTreeMap<String, String>),
    /// An optional reference.
    Ref(Option<String>),
    /// A set of references.
    Refs(Vec<String>),
}

/// A row as hypermon wants it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub table: Table,
    /// Stable logical identity, stored as `external_ids:hypermon-key`.
    pub key: String,
    pub cols: BTreeMap<&'static str, Datum>,
}

impl Row {
    pub fn get(&self, col: &str) -> Option<&Datum> {
        self.cols.get(col)
    }

    /// A string column's value, for tests and diagnostics.
    pub fn str(&self, col: &str) -> Option<&str> {
        match self.cols.get(col)? {
            Datum::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn refs(&self, col: &str) -> Vec<&str> {
        match self.cols.get(col) {
            Some(Datum::Refs(r)) => r.iter().map(String::as_str).collect(),
            Some(Datum::Ref(Some(r))) => vec![r],
            _ => vec![],
        }
    }
}

pub const KEY: &str = "hypermon-key";
pub const MANAGED: &str = "hypermon-managed";
