//! Strongly typed identifiers. All IDs are UUIDv7 so they sort by creation
//! time, which keeps B-tree inserts local and makes listings stable.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "sqlx", derive(sqlx::Type), sqlx(transparent))]
        #[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema), schema(value_type = String, format = Uuid))]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    )*};
}

id_type!(
    TenantId,
    UserId,
    TaskId,
    HostId,
    VmId,
    ImageId,
    VolumeId,
    NetworkId,
    NicId,
    ClusterId,
    StoragePoolId,
);

/// The built-in pool of host-local storage.
pub const LOCAL_POOL: StoragePoolId = StoragePoolId(Uuid::from_u128(1));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_through_json_as_plain_strings() {
        let id = TenantId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<TenantId>(&json).unwrap(), id);
    }

    #[test]
    fn ids_are_time_ordered() {
        let a = VmId::new();
        let b = VmId::new();
        assert!(a < b);
    }
}
