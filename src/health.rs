#![forbid(unsafe_code)]

use serde::Serialize;
use serde_json::Value;

use crate::identity::SidecarIdentity;

#[derive(Serialize, Clone, Debug)]
pub struct Health {
    pub ok: bool,
    pub service: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product: Option<Value>,
}

pub fn current(identity: SidecarIdentity, product: Option<Value>) -> Health {
    Health {
        ok: true,
        service: identity.service,
        product,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::SidecarIdentity;

    #[test]
    fn default_health_omits_product() {
        let json = serde_json::to_value(current(SidecarIdentity::ORES_OTEL, None)).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["service"], "ores-otel-sidecar");
        assert!(json.get("product").is_none());
    }

    #[test]
    fn product_health_is_nested() {
        let extra = serde_json::json!({ "tree_id": "premarital_protection_v1" });
        let json = serde_json::to_value(current(SidecarIdentity::ORES_OTEL, Some(extra))).unwrap();
        assert_eq!(json["product"]["tree_id"], "premarital_protection_v1");
    }
}
